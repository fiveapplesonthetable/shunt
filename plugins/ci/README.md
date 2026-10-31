# CI job execution

`shunt-ci` runs an explicit version-1 JSON job plan. A project's verification
hook supplies the commands and environment. The executor owns parallel admission,
disjoint CPU sets, dependencies, service health waits, process cancellation,
container networks, logs and complete receipts. It has no project path rules or
language-specific commands, and needs only Python's standard library.

Install from the checked-out source with `pip install ./plugins/ci`, then run:

```
shunt-ci --plan plan.json --worktree "$PWD" --output .tachoproof/jobs
```

A job has a distinct `name`, a nonempty `steps` array, optional `after` dependencies,
a CPU budget and a timeout. A step has `run`, an optional workspace-relative
`directory`, and literal `environment` values. `resource_environment` formats
`{cpus}`, `{name}`, `{worktree}`, `{service}` and `{network}` with the admitted
execution resources. Cycles, unknown fields, invalid environment
names and directories that escape the workspace are errors before execution.

An `image` selects a container job. `mounts` declare host and container paths and
whether they are read-only. Writable mounts and the workspace return to the
caller's ownership. A `service` declares an image, network alias, environment,
literal Docker options and command arguments. Its health check must succeed within
`health_timeout`; a running process alone does not establish readiness. `tmpfs`
declares temporary container filesystems. `cgroup_parent` assigns job and service
containers to the machine's configured resource slice.

A host job uses process affinity. Set `host_scope` for jobs that start daemons:
its systemd user scope contains those processes through cancellation and cleanup.
`owned_containers` lists additional containers with a `{name}-` prefix, keeping
their cleanup inside the job's unique resource namespace.
Independent ready jobs run together; a completed dependency returns its CPUs
before its dependents are admitted.

Project adapters can hold `shunt_ci.resources.cache_slot(root)` around plan
construction and execution. Its file lease makes persistent compiler and package
caches exclusive across processes, retains them between runs and bounds the wait
when all slots are occupied.

Logs are written while a job runs. `receipt.json` is replaced atomically as jobs
start and finish, and identifies the exact plan, assigned CPUs, timings and log
paths. A failed, stopped or unstarted job prevents a successful receipt. A failure
cancels other admitted work and leaves every job accounted for.

Runner environment and path channels are parsed as data, including multiline
values. They never execute shell content or replace reserved runner variables.
The job's shell commands execute under the project's existing runner-admission
policy; the executor does not decide which repositories the machine trusts.

Run the behavior tests with:

```
PYTHONPATH=plugins/ci python3 -m unittest discover -s plugins/ci/tests
```
