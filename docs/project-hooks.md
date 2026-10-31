# Project hooks

`shunt project validate shunt.json` checks a repository contract without executing
its hooks. `shunt project apply shunt.json` installs it in the fleet store. Applying
configuration preserves worker limits, holds, queued work and proof records.
The CLI and daemon read the same installed contract.

The contract has version `1`, a repository's `area_rules`, an optional forge
`repository` in `owner/name` form, optional workflow settings, and `hooks`.
Unknown fields and unsupported versions are errors. A hook is an explicit
`program`, an array of literal `args`, and an optional `timeout_secs` between
one and 3,600, defaulting to 60. Request values pass through JSON stdin.

`proof_plan` receives the changed paths, base, head and input tree. It returns
the JSON check array accepted by `shunt prove --plan-file`. Editing source
during planning invalidates the plan, including during a proof dry run.

`runner_trust` admits work from the trusted repository checkout. `prepare`
then runs in the isolated worktree and can return an `environment` object.
Its values pass directly to the worker or verification command. Preparation
cannot redirect Shunt's controller state or home directory. A configured
`verify` hook requires a runner-admission hook. Git hooks and filesystem
monitors are disabled while assembling a proposed batch checkout.

`verify` runs on the proposed merged tree and receives its exact membership,
base, tree, selected areas and units. A successful command cannot certify a
tree that it changes. Native workflow execution remains available when a
repository does not install a verification hook.

`generated_outputs` lists regexes of derived files. Changed matching files
are archived with their before and after Git object IDs, then restored to
their input bytes. New derived files are collected and removed from the
checkout. Worker artifacts live in `.tachoproof/artifacts`; batch artifacts
live under the fleet home's `runs/<batch>/artifacts/<probe>` directory.
Collection never changes a branch head. Changes to any other tracked or
untracked source file invalidate verification. A generated output must be
a regular file.

`input_lists` contains objects with a repository-relative `file` and `root`.
Each list names required reference files, one per line, relative to that
root. Blank lines and `#` comments are ignored. Listed files remain inputs
even inside a generated-output directory. The list is read from the input
Git tree, so a check cannot weaken it while it runs.

`notify` receives a scrubbed subject and body from both CLI and daemon owner
notifications. An unsuccessful delivery remains pending. Credentials come
from the process environment, independently of the checked-in contract.
