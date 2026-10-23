# tachograph

**A flight recorder for coding agents.**

One lightweight daemon records what every coding agent does into a single SQLite WAL event log, and answers two readers:
1. **Supervising agents** asking *"what are my workers doing?"* in under 200 tokens via CLI.
2. **Engineers** inspecting nanosecond execution traces in [ui.perfetto.dev](https://ui.perfetto.dev).

<p align="center">
  <img src="docs/img/architecture.svg" alt="tachograph architecture" width="100%" />
</p>

---

## Key Highlights

- **Single Source of Truth:** Everything is stored in `$TACHO_HOME/events.sqlite` with WAL mode. Snapshots are streamed directly from SQLite rows through a zero-allocation O(1) memory encoder.
- **Resilient Fallback & Graceful Degradation:** If the daemon process crashes, `tacho trace` opens the SQLite WAL file directly in read-only mode. If SQLite encounters a read-only or full disk, in-memory `LiveState` continues updating so agent hooks never block. Dead/zombie worker processes (`ESRCH` / `'Z'`) are automatically reaped, closing open slices with `abandoned = true`.
- **High-Throughput Ingestion:** Sustains a release-mode floor of **&ge; 50,000 inputs/sec** on `api.sock` (up to **511,195 inputs/sec** peak on an unloaded workstation, and over **51,800 inputs/sec** during concurrent 10-million-row snapshot exports).
- **Fast Streaming Snapshots:** Exports 100,000 rows in **96.3 ms** (1.04M rows/s), 1,000,000 rows in **790.6 ms** (1.26M rows/s), and 10,000,000 rows (641 MB) in **7.87 s** (1.27M rows/s) with only a **+4.0 MB RSS delta** from 1M to 10M rows.
- **Supervisor Control Loop & 7 Detectors:** Turns 97 MB of messy worker transcripts into ~180 tokens. Detects `LOOP`, `SPIN`, `BURN`, `FAILING`, `OFF_BRIEF`, CPU-aware `SILENT` (which checks `/proc` CPU counters so long compiles never false-positive), and `CLAIM` (flags runs exiting 0 after edits without passing tests).
- **Claim Verification (`--diff-claims`):** `tacho report <agent> --diff-claims` cross-references recorded edits, tests, commits, and worktree clean status against agent claims.
- **Git Tree Verification:** Every build, test, and run records `git_tree` hash (`git rev-parse HEAD^{tree}`), queryable via `tacho query verified-trees` for CI reuse and merge trains.
- **Properly Nested Lanes:** Parallel tool calls and concurrent subagents are routed into deterministic virtual tracks (`turns`, `lane:1`, `lane:2`), avoiding Perfetto slice stack collisions.
- **Host Corroboration:** Integrates with Perfetto `traced` to overlay kernel scheduler slices, CPU frequency, and per-process memory alongside agent turns.
- **Zero Leaks:** Strict privacy firewall scrubs secrets, tokens, API keys, credentials, and userinfo before persistence or display.

---

## Quickstart

```bash
# 1. Build and install tacho binary
cargo install --path crates/tacho-cli

# 2. Start the daemon (or let CLI auto-spawn it on first command)
tacho daemon &

# 3. Check status of running agents
tacho status

# 4. Export trace snapshot and open in Perfetto UI
tacho trace -o trace.perfetto-trace
# Open trace.perfetto-trace in https://ui.perfetto.dev

# Optional: Install pinned Perfetto trace_processor_shell for `tacho query` and oracle tests
./scripts/oracle.sh
```

> **Path Isolation:** By default, persistent state is stored in `$TACHO_HOME` (`${XDG_STATE_HOME:-~/.local/state}/tacho`) and sockets/locks in `$TACHO_RUNTIME` (`${XDG_RUNTIME_DIR:-/tmp}/tacho-$UID`). When `TACHO_HOME` is explicitly set and `TACHO_RUNTIME` is unset, runtime sockets automatically isolate inside `$TACHO_HOME/run` (falling back to `/tmp/tacho-<hash>` if the socket path would exceed 100 bytes).

---

## Supervisor Control Loop

When an orchestrator drives multiple worker agents, piping raw terminal output into the supervisor rapidly exhausts context windows. `tacho` compresses worker execution into structured, high-density status:

<p align="center">
  <img src="docs/img/supervisor-loop.svg" alt="Supervisor control loop" width="100%" />
</p>

### Blocking on Worker State Changes

```bash
# Block until any worker stalls, becomes idle, or exits
tacho watch --until stalled

# Check compact status across the entire fleet
tacho status
```

Example status output:
```text
agent      state     for    run  turn  last action                tokens out  flags
worker-1   Tool      12s    1    14    Test cargo test -p core     41k
worker-2   Stalled   6m     2    31    Read src/lib.rs (x4)        88k         LOOP
worker-3   Waiting   40s    1    3     Wait on message             2k
```

### Inspecting an Agent Timeline & Claim Verification

```bash
# Detailed timeline report of turns, commands, and stall evidence
tacho report worker-2 --since 30m

# Verify agent claims against actual ground truth (edits, tests, commits, pushes, clean worktree)
tacho report worker-2 --diff-claims
```

Example timeline output:
```text
Turn 28: Read src/lib.rs (820 lines) [ok]
Turn 29: Read src/lib.rs (820 lines) [ok]
Turn 30: Read src/lib.rs (820 lines) [ok]
Turn 31: Read src/lib.rs (820 lines) [ok]
STALL DETECTED: LOOP (identical input digest 4x in a row)
```

Example `--diff-claims` output when an agent claims all tests pass without running them:
```text
=== Claim Verification: worker-2 ===
Claimed:  "Task complete, all tests passing"
Actual:   CLAIM STALL DETECTED (STALL_CLAIM)
  - FileEdits: 4 files modified (src/auth.rs, src/token.rs, ...)
  - Last Test: None after last FileEdited! (exit code 0 required)
  - Worktree:  Dirty (uncommitted changes remain)
Result:   REJECTED — Agent exited without running tests after modifying code.
```

### Seven Anomaly Detectors

| Detector | Trigger Threshold | Diagnostic Meaning |
|---|---|---|
| `SILENT` | 300s no event while not in Wait | Wedged/hung agent. **CPU-aware**: suppresses if child process tree is consuming CPU (e.g. 25-minute `cargo clippy`). |
| `LOOP` | Same tool input digest 4x consecutively | Agent trapped in an automated retry or tool failure loop. |
| `SPIN` | 8 turns without `FileEdited` or `BUILD/TEST/COMMIT` | Agent wandering across read/search calls without making code progress. |
| `BURN` | Output tokens exceed 200k in 10m (or per-task budget) | Runaway generation or prompt inflation exhausting token/cost limits. |
| `FAILING` | Same build/test command failed 3x in a row | Agent repeatedly running failing commands without fixing the defect. |
| `OFF_BRIEF` | `FileEdited` outside task's declared `--paths` | Agent hallucinating edits to out-of-scope files. |
| `CLAIM` | Run exits 0 after edits without passing `TEST` | Agent prematurely claiming task completion without verifying tests. |

---

## Visualizing in Perfetto UI

Open any exported snapshot in [ui.perfetto.dev](https://ui.perfetto.dev) to inspect turns, tool calls, subagents, and host telemetry:

<p align="center">
  <img src="docs/img/timeline.svg" alt="Perfetto timeline visualization" width="100%" />
</p>

Features rendered in the trace:
- **State Band:** `Thinking`, `Tool`, `Waiting`, `Stalled`, `Idle`, and `Exited`.
- **Turns & Tools:** Hierarchical nested slices for every tool call, build, test, and file edit.
- **Flow Arrows:** Explicit causal arrows linking tool invocations to subagent runs and messages.
- **Host Telemetry:** Machine load, memory usage, and kernel scheduler slices aligned by `ClockSnapshot`.

---

## Virtual Lanes for Parallel Tool Calls

Perfetto requires slice tracks to nest like a strict call stack. LLMs frequently execute multiple tool calls in parallel or interleave subagent operations. `tacho` uses a deterministic `LaneAllocator`:

<p align="center">
  <img src="docs/img/lanes.svg" alt="Virtual lane allocation" width="100%" />
</p>

1. If a tool slice begins inside the currently open `Turn` and no sibling tool slice is open, it assigns to `turns`.
2. Otherwise, it routes to the lowest available `lane:k`.
3. Slices left open when a run terminates are automatically closed with `abandoned = true`.

---

## Write Path vs Read Path

<p align="center">
  <img src="docs/img/write-vs-read.svg" alt="Write path vs Read path" width="100%" />
</p>

- **Hook Ingest:** Completes in &lt; 5 ms. Non-blocking; drops silently after 200 ms timeout to protect agent execution.
- **Appender Batch:** Single-threaded writer commits up to 256 events per transaction to SQLite WAL.
- **Live State:** Atomically publishes updated `ArcSwap<LiveState>` pointer; answers `tacho status` in &lt; 100 &mu;s with zero disk I/O.
- **Snapshot Reader:** Streams SQLite rows directly into Perfetto protobuf packets. If the daemon is dead, the CLI falls back to direct read-only WAL access.

---

## Ingestion Modes

### 1. Provider Hooks

Injects zero-latency hooks into agent lifecycle configurations:

```bash
# Configure hooks for installed agent runtimes
tacho install claude
tacho install gemini
tacho install jetski
```

### 2. Process Wrapping (`tacho run`)

Runs an agent inside a supervisor process that tees stdout, parses stream-json output, and samples CPU/RSS over the process tree:

```bash
tacho run -- claude --output-format stream-json
tacho run -- gemini --output-format stream-json
tacho run -- codex exec --json "fix build"
```

### 3. OpenTelemetry OTLP Receiver

Accepts standard OTLP/HTTP traces and logs on port `4318`:

```bash
tacho daemon --otlp 127.0.0.1:4318
```

### 4. Custom Spans & CLI Events

Orchestrator scripts can emit custom events, tasks, budgets, and state transitions:

```bash
# Declare a task with declared paths, bound agent, base commit, and budgets
tacho emit task refactor-auth \
  --paths src/auth.rs,src/token.rs \
  --agent worker-1 \
  --base-commit abc1234 \
  --token-budget 100000 \
  --cost-budget 500000 \
  --brief-file brief.md

# Emit spans
tacho emit span begin custom:pipeline "codegen" --arg schema=v2
tacho emit instant custom:pipeline "checkpoint" --arg status=ok
tacho emit span end custom:pipeline
```

Tasks bound with `tacho emit task` enforce `--paths` scope on `OFF_BRIEF`, enforce per-task token/cost budgets on `BURN`, and link restarted runs of the same task with a Perfetto flow arrow (`task_run_flow:<slug>`).

### 5. Live Transcript Tailing (`tacho tail`)

Tails on-disk Claude Code `.jsonl` transcripts via `inotify`, tracking per-file `(dev, inode, offset)` records atomically in `$TACHO_HOME/tail-offsets.json`:

- **Atomic Offset Persistence:** Safely writes temporary files in the same directory, fsyncs, and renames to `$TACHO_HOME/tail-offsets.json`.
- **Robust Line Processing:** Reads complete lines with `read_until(b'\n')`, keeping trailing partial lines until terminated, and decodes with `from_utf8_lossy` without stalling on invalid UTF-8.
- **Rotation and Truncation Detection:** Tracks `(dev, inode, offset)` per file and restarts from byte 0 whenever file rotation or truncation is detected.
- **Resilient Delivery:** Daemon send errors keep offsets un-advanced and retry with capped backoff (250ms..5s) in watch mode, returning the error in `--once` mode.
- **Targeted inotify Rescans:** Wakeups are driven by `IN_MODIFY`, `IN_CLOSE_WRITE`, `IN_CREATE`, and `IN_MOVED_TO` events, watching newly created project directories on `IN_CREATE`.

```bash
# Continuously watch and ingest transcripts under ~/.claude/projects
tacho tail ~/.claude/projects

# Ingest new lines once and exit
tacho tail ~/.claude/projects --once --agent worker-1
```

---

## Shipped PerfettoSQL Queries

Execute pre-packaged analytics queries directly using `tacho query <name>`:

| Query | Description |
|---|---|
| `agents-now` | Current execution state and duration per agent |
| `open-waits` | Active blocking waits across runs |
| `failing-commands` | Non-zero exit code commands grouped by class |
| `time-by-class` | Wall-clock time spent in Build, Test, Lint, and Format |
| `edits-by-file` | Modifed files, agent counts, and lines added/removed |
| `tokens-by-agent` | Input, output, and cached token consumption |
| `stalls` | Recorded anomaly detector firings with evidence |
| `verified-trees` | Git tree hashes (`git_tree`) with passing builds/tests/lints for CI reuse |
| `system-during-builds` | CPU time and peak RSS intersected with build slices |
| `contention` | Intervals with concurrent build/test runs and host load |

---

## Privacy Firewall

tachograph ensures confidential code and tokens never enter traces:

- **Dropped by Default:** Prompts, model prose, tool outputs, file diffs, and raw message bodies are never stored. Only Blake3 digests and sizes are retained.
- **Pattern Redaction:** Flags (`--token`, `--password`, `-p`), headers (`Authorization: Bearer`), and key shapes (AWS `AKIA`, GCP `ya29`, GitHub `ghp_`, Anthropic `sk-ant-`, OpenAI `sk-`, JWTs, PEM blocks) are replaced with `<redacted>`.
- **SHA Preservation:** 40-character hexadecimal Git commit hashes are explicitly exempted from hex secret redaction.
- **Relative Paths:** Workspace paths are made relative to the project root. External paths are masked as `<outside>/...`.

---

## System Tracing (`traced`)

Correlate agent slices with kernel-level scheduling and system metrics:

```bash
# Setup Perfetto system daemon (prints instructions or runs with sudo)
tacho system setup --sudo

# Start background system ring buffer (process stats, load, CPU freq)
tacho system start --profile default

# Export combined trace containing both agent actions and kernel data
tacho trace --system -o full-system.perfetto-trace
```

---

## Counter Semantics & Deduplication

Perfetto counter tracks are step-functions (each sample holds its value until the next sample):
- **Event-driven counters:** `active_builds`, `active_tests`, `tokens_in`, `tokens_out`, `tokens_cached`, and `cost_micros` emit strictly on event occurrence or value change.
- **Polled host/process counters:** Machine counters (`load1`, `cpu_busy`, `mem_used`, `disk:*`) and per-agent process metrics (`cpu`, `rss`) are polled every 2s from `/proc`, and step-function deduplicated in the sampler: written to SQLite WAL only when the value actually changes.

---

## Crate Layout

```text
crates/
├── tacho-proto/     # Protobuf schemas and prost bindings for Perfetto & tachograph extensions
├── tacho-core/      # Event model, SQLite WAL storage, TrackEvent encoder, scrubber, classifier, LaneAllocator
├── tacho-ingest/    # Provider hooks (Claude/Gemini/Jetski), stream-json tee reader, OTLP HTTP receiver
├── tacho-system/    # Perfetto traced integration, dual-clock sync, system session controller
├── tacho-daemon/    # Background daemon, appender thread, /proc sampler, query service, snapshot server
├── tacho-cli/       # CLI commands, daemon auto-spawner, status/report formatters, SQL query runner
└── tacho-e2e/       # End-to-end integration and black-box verification test suite
```

## Documentation

For the comprehensive system architecture guide, design specifications, and query guides, open [docs/index.html](docs/index.html).
