"""Owned job processes, literal runner channels and container cleanup."""

from __future__ import annotations

import os
from pathlib import Path
import shlex
import signal
import subprocess
import threading
import time

from .spec import Job


NOT_STARTED = 97
STOPPED = 98


class Cancellation:
    """Cancels only registered resources; finished process identities are forgotten."""

    def __init__(self):
        self.lock = threading.Lock()
        self.tripped = False
        self.callbacks = {}
        self.cancelled = set()

    def register(self, name, callback):
        with self.lock:
            self.callbacks[name] = callback
            late = self.tripped
            if late:
                self.cancelled.add(name)
        if late:
            callback()

    def finish(self, name):
        with self.lock:
            self.callbacks.pop(name, None)

    def trip(self, origin):
        with self.lock:
            self.tripped = True
            callbacks = [(name, callback) for name, callback in self.callbacks.items()
                         if name != origin and name not in self.cancelled]
            self.cancelled.update(name for name, _ in callbacks)
        for _, callback in callbacks:
            callback()


def driver(root: str, writable: list[str], owner: str | None) -> str:
    """Runs shell steps while parsing runner environment files strictly as data."""
    handback = ""
    if owner:
        destinations = shlex.join([root, *writable])
        handback = (
            f'refused=$(LC_ALL=C chown -R {owner} {destinations} 2>&1 >/dev/null) || handback=$?; '
            'if [ -n "$refused" ] && ! printf "%s\\n" "$refused" | grep -qv ": No such file or directory$"; then handback=0; '
            'elif [ -n "$refused" ]; then printf "%s\\n" "$refused" >&2; fi; '
            'if [ "$status" -eq 0 ] && [ "${handback:-0}" -ne 0 ]; then status=$handback; fi; '
        )
    interrupt_child = 'kill -TERM -- "-$child"' if owner else 'kill -TERM "$child"'
    return (
        ('set -m; ' if owner else '') + 'child=""; runner_files=""; finish() { status=$?; trap - EXIT; '
        'if [ -n "$runner_files" ]; then rm -rf -- "$runner_files"; fi; '
        + handback + 'exit "$status"; }; '
        'interrupt() { status=$1; trap - TERM INT; if [ -n "$child" ]; then '
        + interrupt_child + ' 2>/dev/null || :; wait "$child" 2>/dev/null || :; fi; exit "$status"; }; '
        "trap finish EXIT; trap 'interrupt 143' TERM; trap 'interrupt 130' INT; "
        'runner_files=$(mktemp -d -t shunt-runner.XXXXXX) || exit 1; '
        'export GITHUB_PATH="$runner_files/path" GITHUB_ENV="$runner_files/env" '
        'GITHUB_OUTPUT="$runner_files/output" GITHUB_STEP_SUMMARY="$runner_files/summary"; '
        'touch "$GITHUB_PATH" "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_STEP_SUMMARY"; '
        'step=0; for command in "$@"; do step=$((step + 1)); printf "step %s started\\n" "$step"; '
        ': > "$GITHUB_OUTPUT"; : > "$GITHUB_STEP_SUMMARY"; '
        'bash -eo pipefail -c "$command" & child=$!; wait "$child"; status=$?; child=""; '
        '[ "$status" -eq 0 ] || exit "$status"; '
        'while IFS= read -r path || [ -n "$path" ]; do '
        '[ -z "$path" ] || PATH="$path:$PATH"; done < "$GITHUB_PATH"; : > "$GITHUB_PATH"; export PATH; '
        'while IFS= read -r line || [ -n "$line" ]; do '
        '[ -z "$line" ] && continue; '
        'if [[ "$line" == *"<<"* ]]; then key=${line%%<<*}; delimiter=${line#*<<}; value=""; found=0; '
        'while IFS= read -r part; do if [ "$part" = "$delimiter" ]; then found=1; break; fi; '
        'value="${value:+$value$\'\\n\'}$part"; done; [ "$found" -eq 1 ] || exit 1; '
        'else [[ "$line" == *"="* ]] || exit 1; key=${line%%=*}; value=${line#*=}; fi; '
        '[[ "$key" =~ ^[a-zA-Z_][a-zA-Z_0-9]*$ ]] || exit 1; '
        '[[ "$key" == GITHUB_* || "$key" == RUNNER_* ]] && continue; '
        'printf -v "$key" "%s" "$value"; export "$key"; '
        'done < "$GITHUB_ENV"; : > "$GITHUB_ENV"; printf "step %s passed\\n" "$step"; done'
    )


def step_commands(job: Job, root: Path) -> list[str]:
    result = []
    for step in job.steps:
        directory = (root / step.directory).resolve()
        if not directory.is_relative_to(root.resolve()):
            raise ValueError("a step's directory resolves outside its workspace")
        prefix = f"cd -- {shlex.quote(str(directory))}\n"
        variables = "\n".join(f"export {key}={shlex.quote(value)}" for key, value in step.environment.items())
        result.append(prefix + (variables + "\n" if variables else "") + step.run)
    return result


def _docker(*args, run=subprocess.run):
    return run(["docker", *args], capture_output=True, text=True, timeout=30)


def _container_flags(cpus, parent):
    result = [f"--cpus={len(cpus)}", f"--cpuset-cpus={','.join(map(str, cpus))}"]
    if parent:
        result.append(f"--cgroup-parent={parent}")
    return result


def container_argv(job, root, name, cpus, parent, environment):
    argv = ["docker", "run", "--rm", "--init", "--name", name, "--network", name,
            "--user", "root", *_container_flags(cpus, parent), "-w", str(root), "-v", f"{root}:{root}"]
    writable = []
    for mount in job.mounts:
        argv += ["-v", f"{mount.host}:{mount.inside}" + (":ro" if mount.read_only else "")]
        if not mount.read_only:
            writable.append(mount.inside)
    for key, value in environment.items():
        argv += ["-e", f"{key}={value}"]
    for path, options in job.tmpfs.items():
        argv += ["--tmpfs", f"{path}:{options}"]
    return argv + [job.image, "bash", "-c", driver(str(root), writable, f"{os.getuid()}:{os.getgid()}"),
                   "shunt-ci", *step_commands(job, root)]


def _wait_service(name, seconds, cancellation, run):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline and not cancellation.tripped:
        observed = _docker("inspect", "-f", "{{json .State}}", name, run=run)
        if observed.returncode:
            return False
        import json
        state = json.loads(observed.stdout)
        if not state.get("Running") or state.get("Health", {}).get("Status") == "unhealthy":
            return False
        if state.get("Health", {}).get("Status") == "healthy":
            return True
        if "Health" not in state:
            raise ValueError("service readiness requires a health check")
        time.sleep(min(.25, max(0, deadline - time.monotonic())))
    return False


def execute(job, root, name, cpus, parent, log, cancellation, run=subprocess.run):
    resources = {"cpus":len(cpus),"name":name,"worktree":str(root),"service":f"{name}-service","network":name}
    environment = {**job.environment, **{key: value.format(**resources) for key, value in job.resource_environment.items()}}
    process_env = {key: value for key, value in os.environ.items() if not key.startswith("TACHO_")}
    scope = f"{name}.scope" if job.host_scope and not job.image else None
    service = f"{name}-service"
    additional = [template.format(**resources) for template in job.owned_containers]
    containers = [name, service, *additional] if job.image else additional
    child = None
    process_lock = threading.Lock()
    finished = False

    def halt():
        with process_lock:
            if finished:
                return
            if scope:
                run(["systemctl", "--user", "stop", scope], capture_output=True, timeout=30)
            elif child:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if containers:
                _docker("rm", "-f", *containers, run=run)

    cancellation.register(job.name, halt)
    try:
        if cancellation.tripped:
            return STOPPED
        commands = step_commands(job, root)
        if job.image:
            made = _docker("network", "create", name, run=run)
            if made.returncode:
                log.write(made.stderr.encode()); log.flush()
                return NOT_STARTED
            if job.service:
                spec = job.service
                argv = ["docker", "run", "-d", "--init", "--name", service, "--network", name,
                        "--network-alias", spec.alias, *_container_flags(cpus, parent)]
                for key, value in spec.environment.items():
                    argv += ["-e", f"{key}={value}"]
                started = run([*argv, *spec.options, spec.image, *spec.command], capture_output=True, text=True, timeout=30)
                if started.returncode or not _wait_service(service, spec.health_timeout, cancellation, run):
                    log.write(f"service {spec.alias} did not become ready\n{started.stderr}".encode()); log.flush()
                    return NOT_STARTED
            argv = container_argv(job, root, name, cpus, parent, environment)
        else:
            argv = ["taskset", "-c", ",".join(map(str, cpus)), "bash", "-c", driver(str(root), [], None), "shunt-ci", *commands]
            if scope:
                argv = ["systemd-run", "--user", "--scope", "--quiet", "--collect", "--unit", name,
                        "--property", f"AllowedCPUs={','.join(map(str, cpus))}", *argv]
            process_env.update(environment)
        with process_lock:
            if cancellation.tripped:
                return STOPPED
            child = subprocess.Popen(argv, cwd=root, env=process_env, stdout=log,
                                     stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + job.timeout
        while True:
            observed = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if observed is not None:
                break
            if time.monotonic() >= deadline:
                halt()
                with process_lock:
                    finished = True
                child.wait()
                log.write(f"job exceeded its {job.timeout}s timeout\n".encode()); log.flush()
                return 124
            time.sleep(.05)
        with process_lock:
            finished = True
        return child.wait()
    finally:
        cancellation.finish(job.name)
        with process_lock:
            unfinished = child is not None and not finished
        if unfinished:
            halt()
            child.wait()
        if scope:
            run(["systemctl", "--user", "stop", scope], capture_output=True, timeout=30)
        if containers:
            _docker("rm", "-f", *containers, run=run)
        if job.image:
            _docker("network", "rm", name, run=run)
