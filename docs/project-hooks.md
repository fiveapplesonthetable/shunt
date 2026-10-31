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

`notify` receives a scrubbed subject and body from both CLI and daemon owner
notifications. An unsuccessful delivery remains pending. Credentials come
from the process environment, independently of the checked-in contract.
