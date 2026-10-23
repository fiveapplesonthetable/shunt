---
name: tacho
description: Flight recorder, worker kit, and fleet orchestrator for coding agents — worktrees, proof preflight, speculative batch verification, merge trains, stall detection, and Perfetto timeline tracing.
---

# tacho: Agent Flight Recorder, Worker Kit & Fleet Orchestrator Skill

Use `tacho` both as a **worker agent** (isolated worktrees, background jobs, CPU starvation reproduction, scoped proof checks, and PR preflight guardrails) and as a **fleet orchestrator & supervisor** (work queue, area serialization, speculative batch verification with binary bisection, verified merge trains, progress/stall detection, and Perfetto timeline tracing).

---

## Part 1: Worker Agent Kit (`start`, `job`, `wait`, `kill`, `starve`, `prove`, `pr`, `note`)

Every worker agent follows this lifecycle:

### 1. Create an Isolated Worktree (`tacho start`)
```bash
tacho start <slug> --base main
# or stack on an upstream PR:
tacho start <slug> --base 42
```
- Validates `<slug>` (`3..80` lowercase alphanumeric + single hyphens).
- Resolves `--base` to an **exact commit SHA** (never a mutable branch reference).
- Allocates an isolated 20-port block (`20000..32000`, verified free in both fleet state and TCP bind).
- Writes `.tachoenv` (`SLUG`, `BASE_SHA`, `BASE_PR`, `PORT`, `PORT_LAST`, `CARGO_TARGET_DIR`) and adds `.tachoenv`, `.tachoproof/`, and `.tachorun/` to `.git/info/exclude`.

### 2. Record Phase Notes (`tacho note`)
Keep the supervisor informed as you move between phases (`read`, `write`, `test`, `fix`, `push`):
```bash
tacho note <slug> "read: inspecting WAL recovery and lock ordering"
tacho note <slug> "write: implementing pure bisection state machine"
tacho note <slug> "test: reproducing race under CPU starvation"
```

### 3. Manage Background Processes (`tacho job`, `tacho wait`, `tacho kill`)
Never leave orphan background processes or block a tool call past its 10-minute timeout:
```bash
# Launch a detached process group under .tachorun/<name>
tacho job full-suite -- cargo test --workspace

# Stream output for up to 9 minutes (exits 0 on pass, non-zero on failure, 3 if still running)
tacho wait full-suite

# Terminate the job's entire process group (verifies worktree ownership first)
tacho kill full-suite --now
```

### 4. Reproduce Timing & Concurrency Flakes (`tacho starve`)
```bash
tacho starve -- cargo test -p tacho-core flaky_test
```
- Allocates at most 1/8th of host CPUs (highest-numbered cores first), spawns 4 `yes` burners per core in that slice, and pins the command to the starved cores with a 5-minute timeout.
- Automatically refuses if a verification batch is running and yields immediately if a batch starts.

### 5. Prove Your Changes (`tacho prove`)
Before opening or updating a PR, run `tacho prove`:
```bash
tacho prove
```
- Computes the exact git tree hash of the working tree (including unstaged and untracked files via a temporary index) and maps changed files to scoped checks (`git diff --check`, `bash -n`, `cargo clippy`, `cargo test`).
- Writes `.tachoproof/summary.json` recording `head`, `tree`, `base`, `ok`, and per-check durations.

### 6. Open or Update a Pull Request (`tacho pr`)
```bash
tacho pr "Fix WAL checkpoint race" -m "Eliminates lock inversion between appender and snapshot reader."
```
- Preflight refuses if: title/description is empty, diff is empty, branch has merge commits, worktree has uncommitted changes, `.tachoproof/summary.json` is missing/failed/stale (`proof.tree != HEAD^{tree}`), or any commit/title/description contains `Co-Authored-By`, `Generated with/by`, or robot emoji.
- Pushes with `--force-with-lease`, opens/updates a draft PR with a formatted `## Proof` table, and prints `PR: #<n>`.
- Always end your final worker output with `PR: #<n>` (or `PR: none` if no code changes were needed).

---

## Part 2: Fleet Orchestration, Verification & Landing

### 1. Enqueue & Dispatch Work (`tacho todo`, `tacho next`, `tacho tick`)
```bash
# Queue a task in an area chain (tasks in the same area run one at a time and stack on each other)
tacho todo auth-step-1 "Extract token verifier" --area auth
tacho todo auth-step-2 "Add rotation support" --area auth --after auth-step-1

# Side-by-side multi-model comparison (expands into parallel unscoped runs)
tacho todo parser-opt "Optimize wire varint decoder" --compare opus,codex,gemini

# Preview what the next tick will dispatch and verify
tacho next

# Execute one orchestrator & supervisor pass
tacho tick
```

### 2. Control Workers, Holds & Retirement (`tacho hold`, `tacho release`, `tacho retire`, `tacho attach`, `tacho tell`, `tacho stop`)
```bash
tacho hold pr:42 --reason "waiting on schema migration"
tacho hold area:auth --reason "holding auth chain"
tacho release pr:42
tacho attach my-task 42        # or: tacho attach my-task none
tacho tell my-task "Use BTreeMap for deterministic ordering"
tacho stop my-task
tacho retire my-task
```

### 3. Speculative Batch Verification & Flake Ledger (`tacho batch`, `tacho replay`, `tacho flakes`)
```bash
# Verify open PRs in batches of up to 6 (taking stacks whole; CI changes run solo)
tacho batch

# Replay a specific batch
tacho replay b001

# Inspect the persistent flake ledger (auto-queues a fix task when a test flakes >= 2 times)
tacho flakes
```
- When a batch fails, binary bisection probes `main` (`k = 0`) before blaming a single or first PR, and the failure classifier always classifies named failing tests or `"minimal failing input"` counterexamples as `"code"` even beside infrastructure noise.

### 4. Verified Merge Trains & Ready Queue (`tacho queue`, `tacho train`, `tacho release-train`, `tacho land`)
```bash
tacho queue
tacho train --from-batch b001
tacho release-train --approve
tacho land
```
- Verifies that `main`'s post-merge `git_tree` equals the verified train `tree`.

### 5. Supervision, Progress, Janitor & Audit (`tacho top`, `tacho progress`, `tacho events`, `tacho tidy`, `tacho watchdog`)
```bash
tacho top                 # Compact dashboard + NEEDS YOU attention items
tacho progress            # MOVING / STALLED (45m) / HUNG (30m) verdicts + area chain velocity
tacho events --limit 30   # Audit log of every decision and its 'because' reason
tacho tidy --dry-run      # Clean up stale worktrees (rescuing dirty/unpushed work)
tacho watchdog            # Alert if orchestrator tick is stale while work is pending
```

---

## Part 3: Flight Recorder, Claim Verification & Perfetto Tracing

### 1. Monitor Fleet Status & Block on State Changes
```bash
tacho status
tacho status --json
tacho watch --until stalled
tacho watch --until idle
tacho watch --until exit
```

### 2. Diagnose a Specific Worker & Verify Claims
```bash
tacho report <agent-id> --since 30m
tacho report <agent-id> --diff-claims
```

### 3. Run Analytical PerfettoSQL Queries & Export Traces
```bash
tacho query agents-now
tacho query open-waits
tacho query failing-commands
tacho query time-by-class
tacho query edits-by-file
tacho query stalls
tacho query verified-trees
tacho trace -o trace.perfetto-trace --since 1h
```

