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
- `flags`: Active anomaly detector flags (`LOOP`, `SPIN`, `SILENT`, `BURN`, `FAILING`, `OFF_BRIEF`).

For programmatic consumption:
```bash
tacho status --json
```

### 2. Block on State Changes (Zero Polling)
Block your orchestrator until a worker triggers a condition, instead of polling in a loop:
```bash
# Block until any worker stalls (loop, spin, timeout, failing command)
tacho watch --until stalled

# Block until all workers become idle or exit
tacho watch --until idle
tacho watch --until exit
```

### 3. Diagnose a Specific Worker
When an agent encounters an anomaly or error, pull its execution timeline:
```bash
tacho report <agent-id> --since 30m
```
This prints a concise ~180-token summary of recent turns, tool calls, exit codes, modified files, and stall evidence.

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
```

### 5. Export Trace for Visual Inspection
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
3. **Handle Stalls Proactively:**
   - If `flags` contains `LOOP`: Worker is calling the same tool repeatedly. Interrupt or send corrective feedback.
   - If `flags` contains `FAILING`: The worker has failed the same build/test 3 times. Provide explicit test failure details.
   - If `flags` contains `SILENT`: Worker hung or crashed without an exit hook.
