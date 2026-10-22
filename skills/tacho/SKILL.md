---
name: tacho
description: Flight recorder for agent supervision, timeline tracing, stall detection, and telemetry querying.
---

# tacho: Agent Flight Recorder & Supervisor Skill

Use `tacho` to monitor, supervise, and diagnose fleets of coding agents without polluting your context window with megabytes of raw transcripts.

## When to Use

- **Supervising Subagents:** When running background workers, use `tacho status` and `tacho watch` instead of parsing raw stdout or log files.
- **Diagnosing Stalls & Loops:** When a worker is unresponsive, stuck in a retry loop, or failing builds, use `tacho report <agent>` to inspect the root cause.
- **Performance & Bottlenecks:** When investigating slow builds, test contention, or token burn, use `tacho query <query-name>` or export a trace with `tacho trace`.
- **Emitting Custom Spans:** When writing orchestrator pipelines, use `tacho emit` to trace stages, tasks, and state transitions.

---

## Quick Reference Commands

### 1. Monitor Fleet Status
Get a compact, single-line overview of every active agent:
```bash
tacho status
```
Output columns:
- `agent`: Agent identifier (from `TACHO_AGENT` or provider session).
- `state`: Current state (`Thinking`, `Tool`, `Waiting`, `Stalled`, `Idle`, `Exited`).
- `for`: Duration in the current state.
- `run`: Current session run number.
- `turn`: Turn count in this run.
- `last action`: Current command, tool call, or wait target.
- `tokens out`: Cumulative output tokens.
- `flags`: Active anomaly detector flags (`LOOP`, `SPIN`, `SILENT`, `BURN`, `FAILING`, `OFF_BRIEF`, `CLAIM`).

For programmatic consumption:
```bash
tacho status --json
```

### 2. Block on State Changes (Zero Polling)
Block your orchestrator until a worker triggers a condition, instead of polling in a loop:
```bash
# Block until any worker stalls (loop, spin, timeout, failing command, false claim)
tacho watch --until stalled

# Block until all workers become idle or exit
tacho watch --until idle
tacho watch --until exit
```

### 3. Diagnose a Specific Worker & Verify Claims
When an agent encounters an anomaly, finishes a task, or makes completion claims:
```bash
# Recent turns, tool calls, exit codes, modified files, and stall evidence
tacho report <agent-id> --since 30m

# Verify agent completion claims against verifiable ground truth
tacho report <agent-id> --diff-claims
```
`--diff-claims` asserts that:
- File modifications were recorded (`FileEdited`).
- A passing `TEST` command (`exit 0`) completed *after* the final file modification.
- Commits or pushes were performed and the git working tree is clean.

### 4. Run Analytical Queries
Execute pre-packaged PerfettoSQL queries against the live database:
```bash
# Show active blocking waits across runs
tacho query open-waits

# Show failing commands and non-zero exit codes
tacho query failing-commands

# Wall-clock time spent in Build, Test, Lint, and Format
tacho query time-by-class

# Files modified, line deltas, and modifying agents
tacho query edits-by-file

# Active anomaly detector firings with evidence
tacho query stalls

# Git trees with passing builds, tests, and lints for CI reuse
tacho query verified-trees
```

### 5. Task Orchestration & Budgets (`tacho emit task`)
Declare tasks to bound scope, enforce budgets, and link retries:
```bash
tacho emit task auth-refactor \
  --paths src/auth.rs,src/token.rs \
  --agent worker-1 \
  --base-commit abc1234 \
  --token-budget 100000 \
  --cost-budget 500000 \
  --brief-file brief.md
```
- Sets declared `--paths` for `OFF_BRIEF` detection.
- Sets per-task token and cost ceilings for `BURN` detection.
- Links subsequent runs of the same task with a Perfetto flow arrow (`task_run_flow:<slug>`).

### 6. Export Trace for Visual Inspection
Export a `.perfetto-trace` file to open in [ui.perfetto.dev](https://ui.perfetto.dev):
```bash
tacho trace -o trace.perfetto-trace --since 1h
```

---

## Best Practices for Orchestrators

1. **Tag Child Processes:** Always export `TACHO_AGENT=<name>` when spawning worker subagents so all hooks and commands attribute to that worker:
   ```bash
   export TACHO_AGENT=worker-1
   ```
2. **Never Ingest Raw Transcripts:** Raw agent logs easily exceed 50-100 MB. Always use `tacho status` or `tacho report` to maintain a lean context window.
3. **Verify Claims Before Acceptance:** Run `tacho report <agent> --diff-claims` when an agent claims completion to confirm that tests were actually run and passed after file edits.
4. **Handle Stalls Proactively:**
   - If `flags` contains `CLAIM`: Agent reported completion without verifying tests. Reject and instruct agent to run test suite.
   - If `flags` contains `LOOP`: Worker is calling the same tool repeatedly. Interrupt or send corrective feedback.
   - If `flags` contains `FAILING`: The worker has failed the same build/test 3 times. Provide explicit test failure details.
   - If `flags` contains `OFF_BRIEF`: Worker modified files outside task's `--paths`. Revert changes and re-anchor prompt.
   - If `flags` contains `SILENT`: Worker process is quiet. If not consuming CPU, process may be wedged or dead.
