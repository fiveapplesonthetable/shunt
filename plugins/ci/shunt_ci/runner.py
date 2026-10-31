"""Parallel job admission, live logs and complete execution receipts."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, wait, FIRST_COMPLETED
from dataclasses import asdict
import hashlib
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
import uuid

from . import process
from .spec import Plan


def allocations(budgets: dict[str, int], cpus: set[int]) -> dict[str, tuple[int, ...]]:
    """Gives admitted jobs disjoint CPU sets, sharing scarcity in plan order."""
    available = sorted(cpus)
    admitted = list(budgets)[:len(available)]
    result = {name: () for name in admitted}
    while available:
        wanting = [name for name in admitted if len(result[name]) < budgets[name]]
        if not wanting:
            break
        for name in wanting:
            if available:
                result[name] = (*result[name], available.pop(0))
    return result


def _receipt(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            json.dump(value, output, indent=2)
            output.flush()
            os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def execute(plan: Plan, root: Path, output: Path, *, available_cpus=None, executor=process.execute):
    """Executes every declared job once; failed, cancelled and missing jobs never pass."""
    plan.validate()
    root = root.resolve(strict=True)
    output = output.resolve()
    if output == root:
        raise ValueError("job reports need their own directory")
    output.mkdir(parents=True, exist_ok=True)
    for job in plan.jobs:
        process.step_commands(job, root)
    usable = sorted(os.sched_getaffinity(0) if available_cpus is None else available_cpus)
    if plan.reserved_cpus:
        usable = usable[:-plan.reserved_cpus]
    if not usable:
        raise ValueError("no CPUs remain after the configured reservation")
    free = set(usable)
    stop = process.Cancellation()
    run_id = f"shunt-{uuid.uuid4().hex[:16]}"
    raw_plan = json.dumps(asdict(plan), sort_keys=True, separators=(",", ":")).encode()
    receipt = {"version": 1, "run_id": run_id, "plan_digest": hashlib.sha256(raw_plan).hexdigest(),
               "worktree": str(root), "started_at": time.time(), "finished_at": None,
               "ok": False, "jobs": {}}
    _receipt(output / "plan.json", asdict(plan))
    _receipt(output / "receipt.json", receipt)
    waiting = {job.name: job for job in plan.jobs}
    running = {}

    def one(job, cpus):
        began = time.monotonic()
        path = output / f"{job.name}.log"
        with path.open("wb", buffering=0) as log:
            try:
                code = executor(job, root, f"{run_id}-{job.name}", cpus,
                                plan.cgroup_parent, log, stop)
                error = None
            except Exception as failure:
                code, error = process.NOT_STARTED, str(failure)
                log.write(f"job execution failed: {failure}\n".encode())
        cancelled = job.name in stop.cancelled and code != 0
        return {"status": "stopped" if cancelled else "passed" if code == 0 else "failed",
                "exit_code": process.STOPPED if cancelled else code,
                "wall_seconds": round(time.monotonic() - began, 3), "cpus": list(cpus),
                "log": str(path), "error": error}

    def interrupt(signum, _frame):
        stop.trip(f"signal-{signum}")

    handlers = {}
    import threading
    if threading.current_thread() is threading.main_thread():
        for signum in (signal.SIGINT, signal.SIGTERM):
            handlers[signum] = signal.signal(signum, interrupt)
    try:
        with ThreadPoolExecutor(max_workers=min(len(plan.jobs), len(usable))) as pool:
            while waiting or running:
                passed = {name for name, result in receipt["jobs"].items() if result["status"] == "passed"}
                ready = [job for job in waiting.values() if set(job.after) <= passed]
                if not stop.tripped and ready and free:
                    assigned = allocations({job.name: job.cpus for job in ready}, free)
                    for job in ready:
                        if job.name not in assigned:
                            continue
                        cpus = assigned[job.name]
                        free.difference_update(cpus)
                        waiting.pop(job.name)
                        receipt["jobs"][job.name] = {"status":"running", "exit_code":None,
                            "cpus":list(cpus),"log":str(output / f"{job.name}.log"),"started_at":time.time()}
                        _receipt(output / "receipt.json",receipt)
                        print(f"{job.name}: running on CPUs {','.join(map(str, cpus))}; log {output / (job.name + '.log')}", flush=True)
                        running[pool.submit(one, job, cpus)] = (job, cpus)
                if not running:
                    break
                completed, _ = wait(running, return_when=FIRST_COMPLETED)
                for future in completed:
                    job, cpus = running.pop(future)
                    result = future.result()
                    free.update(cpus)
                    receipt["jobs"][job.name] = result
                    print(f"{job.name}: {result['status']} in {result['wall_seconds']}s", flush=True)
                    if result["status"] != "passed":
                        stop.trip(job.name)
                    _receipt(output / "receipt.json", receipt)
        for name in waiting:
            receipt["jobs"][name] = {"status": "not_started", "exit_code": process.NOT_STARTED,
                                     "wall_seconds": 0, "cpus": [], "log": None, "error": "a dependency failed or the plan was cancelled"}
        receipt["ok"] = len(receipt["jobs"]) == len(plan.jobs) and all(result["status"] == "passed" for result in receipt["jobs"].values())
        receipt["finished_at"] = time.time()
        _receipt(output / "receipt.json", receipt)
        return receipt
    finally:
        stop.trip("runner-exit")
        for signum, handler in handlers.items():
            signal.signal(signum, handler)


def main(argv=None):
    parser = argparse.ArgumentParser(description="execute an explicit Shunt project job plan")
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--worktree", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        plan = Plan.parse(json.loads(args.plan.read_text()))
        receipt = execute(plan, args.worktree, args.output)
        return 0 if receipt["ok"] else 1
    except (ValueError, OSError, TypeError) as error:
        print(f"shunt-ci: {error}", file=sys.stderr)
        return process.NOT_STARTED


if __name__ == "__main__":
    sys.exit(main())
