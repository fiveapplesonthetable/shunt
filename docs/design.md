# tachograph: design

A flight recorder for coding agents. One small daemon per machine records what
every agent on it does into a single SQLite WAL event log, and answers two kinds
of reader: a person opening a Perfetto trace in ui.perfetto.dev, and an agent
asking "what are my workers doing" in a few hundred tokens.

The binary is `tacho`. The crate is `tachograph`.

This document is the contract the code is built against. Where code and this
document disagree, one of them is a bug.

## 0. Goals and non-goals

Goals:

1. One source of truth. The SQLite WAL event log holds every event at full
   resolution; every trace is rendered from those rows through one encoder.
2. A snapshot at any moment, without stopping anything: `tacho trace` writes a
   `.perfetto-trace` that opens in ui.perfetto.dev and loads in
   `trace_processor` with zero import errors — even if the daemon is wedged or
   dead (by reading `events.sqlite` directly).
3. Everything an agent does is typed: turns, tool calls, commands (classified as
   build, test, push, ...), file reads and edits, waits, messages, stalls,
   tokens and cost. Durations are slices, points are instants, quantities are
   counters, causes are flows.
4. Agent-agnostic. Claude Code, Codex, Gemini CLI, Jetski and any script feed
   the same schema. No agent needs code changes: hooks, wrapping, or OTLP.
5. Useful to a supervising agent. `tacho status`, `tacho report`, `tacho watch`
   answer from the daemon's live state in compact text or JSON. Stall, loop and
   spin detectors run in the daemon.
6. Optional system tracing. If the user runs Perfetto's `traced` (with root for
   ftrace, or without root for /proc-based data), `tacho trace --system` clones
   the system session and writes one file with both: agent slices beside
   scheduling, CPU frequency and per-process memory.
7. Nothing secret leaves the machine unintentionally. Prose, tool output, file
   content, diffs and prompts never enter the trace. Default-on regex/shape
   scrubbing (`scrub = true` by default; opt-out via `scrub = false` in
   `config.toml` or `TACHO_SCRUB=0`) redacts credentials and tokens from
   command lines, titles, status pages, issues, owner emails and log tails.
8. Pure, replayable fleet orchestration and worker kit. `tacho` provides the
   worker kit (`start`, `prove`, `pr`, `job`, `note`), guardrail hooks, a
   configurable provider interface (`start`, `resume`, `tell`, `alive`, `stop`,
   `cost`), speculative verification batches that run CI's own workflow jobs and
   bisect failures, verified merge trains, and an event-driven orchestrator loop
   (`decide`) whose every action carries its `because` reason into the SQLite
   WAL log.

Non-goals: hosted cloud SaaS dashboards, MCP servers.
Operators and agents interact through the `tacho` CLI and the SQLite WAL log.

## 1. Shape

```
 claude hooks ─┐
 gemini hooks ─┤   api.sock (NDJSON)
 jetski hooks ─┼──────────────┐
 tacho emit   ─┘              ▼
                     ┌─────────────────┐        WAL (single writer,
 tacho run -- cmd ──►│ appender thread │──────► SQLite event log ◄───────┐
 tacho tail (.jsonl)►│ stamp, classify,│        (events.sqlite)          │
 OTLP :4318 ────────►│ lane, log, fold │               │                 │
 sampler (/proc+cg)─►└─────────────────┘               ├──► snapshot ────► tacho trace
                                                       │    (trace.sock    ▲
                                                       │     or direct RO) │ (clone)
                                                       │             traced (optional)
                                                       └──► state fold ──► tacho status / report / watch
```

One process, `tacho daemon`, started on demand by any client that finds no
socket (like sccache), or by a user service. No async runtime: plain OS
threads and blocking sockets. Threads:

- **acceptors**: one per listening source (api.sock, OTLP, each `tacho run` or
  `tacho tail` connection). They parse input into `Input` values and send them
  on one bounded channel. They never touch the log or state.
- **appender**: the only consumer of the channel and the only writer of the
  SQLite WAL log (`$TACHO_HOME/events.sqlite`). It batches inserts into one
  transaction per channel drain (up to 256 events or 50 ms) with
  `synchronous = NORMAL` and a prepared statement, and folds the event stream
  for status/report/watch readers.
- **snapshot**: owns `trace.sock`. Opens its own read-only SQLite connection
  (WAL readers never block the writer and the writer never blocks readers),
  reconstructs the track registry from `TrackCreated` rows, and streams rows
  through `SnapshotProjector` and the core encoder. Answers even when the
  appender is stuck; and if the daemon is dead (`SIGKILL`), `tacho trace`,
  `tacho status`, and `tacho report` open `events.sqlite` read-only in the CLI
  process itself.
- **sampler**: every `sample_interval` (default 2 s) reads `/proc` and cgroup v2
  leaf metrics and sends process-tree slot slices and step-deduplicated counter
  samples to the appender.
- **query**: serves `status`/`report`/`watch` on `api.sock` from the WAL-derived
  state fold (`Recorder::restore` on daemon startup, updated on each committed
  WAL batch, with direct read-only SQLite fallback in the CLI when the daemon
  is down).

State lives in `$TACHO_HOME` (default `$XDG_STATE_HOME/tacho`, else
`~/.local/state/tacho`). Sockets live in `$TACHO_RUNTIME` (default
`$XDG_RUNTIME_DIR/tacho`, else `$TACHO_HOME/run`). Directories are created 0700.

## 2. Format

### 2.1 Perfetto native, written with prost

A trace file is a sequence of `Trace.packet` entries (tag `0x0a`, varint
length, `TracePacket` bytes). We vendor Perfetto's merged
`perfetto_trace.proto` (with its source revision and SHA-256 recorded beside
it) and our own `tacho.proto`, and generate Rust with prost-build. No C++, no
Perfetto SDK, no unsafe code outside dependencies.

### 2.2 Typed fields through a TrackEvent extension

`tacho.proto` extends `perfetto.protos.TrackEvent` in the range 4000-4099
(unallocated in Perfetto's `extensions.json`; verified in phase 0). Each event
family has one extension field whose name is the args prefix in Trace
Processor: `command.argv`, `tool_call.name`, `turn_ended.output_tokens`.
Every trace starts with an `ExtensionDescriptor` packet carrying the
`FileDescriptorSet` of `tacho.proto`, so Trace Processor decodes the fields
into the args table by name, with enums by name.

The open set (custom spans from scripts, plugin-defined details) uses
`debug_annotations`. Closed sets are typed; open sets are annotated.

### 2.3 Common extension fields on every event

```
optional uint64 seq          = 4000;  // log sequence number, unique per daemon install
optional uint64 cause_seq    = 4001;  // the event this one was caused by, if known
optional string agent        = 4002;  // agent id (track key), for SQL without joins
```

## 3. Identity: agents, runs, tasks

- **agent**: a long-lived worker identity. Key `agent:<id>`. The id comes from
  `TACHO_AGENT` if set, else `<provider>-<first 8 of session id>`. An
  orchestrator sets `TACHO_AGENT=worker-3` in a child's environment and every
  hook the child fires inherits it.
- **run**: one provider session of an agent, from start to exit. Key
  `agent:<id>/run:<n>`, n from 1. A new `session_id` for the same agent starts a
  new run. A run may have a `parent_run` (a subagent spawned by a tool call);
  the child run's track is placed under the parent agent and joined by a flow
  from the spawning tool call.
- **task**: optional unit of work an orchestrator declares (`tacho emit task`),
  with a slug, a brief digest and the paths the brief names. `TACHO_TASK` in
  the environment binds a run to a task.

Hook inputs that carry a provider session id (Claude Code, Gemini CLI, Jetski)
are routed by `(provider, session_id)` to their run. The daemon keeps the map in
live state, rebuilt from the log on start.

## 4. Track hierarchy

```
tacho                                   root; description: host, version
├── messages                            duration slices: MessageSent -> MessageDelivered
├── machine                             counters: load1, cpu_busy, mem_used, active_builds, active_tests, disk:<mount>
├── daemon                              lifecycle duration slice (DaemonStarted -> Stop/Snapshot)
│   └── counters                        events, trace_us, queue_depth
├── agents
│   └── agent:<id>                      description: provider, model, cwd
│       ├── state                       TYPE_STATE: Idle, Thinking, Tool, Waiting, Stalled, Exited
│       ├── stalls                      duration slices: StallDetected -> recovery / run end
│       └── run:<n>                     slice: Run
│           ├── turns                   Turn > ToolCall | Build | Test | Read | Edit | ...
│           ├── lane:1 .. lane:k        overlapping tool calls (section 4.2)
│           ├── waits                   Wait slices
│           ├── worktree                duration slices: dirty worktree -> clean commit
│           ├── procs/slot 0 .. k       child process lifetime slices (rustc, cargo-nextest, ...)
│           ├── files_edited / commits  step-function counters
│           ├── tokens_in / tokens_out / tokens_cached / cost
│           └── proc_count / cpu / rss  /proc + cgroup v2 process-tree counters
├── tasks
│   └── task:<slug>
│       ├── state                       TYPE_STATE: Queued, InProgress, Review, Done, Failed
│       └── review                      Review duration slices with aggregated findings
├── providers
│   └── provider:<name>                 error_count, rate_limits, tokens_out, cost (summed)
└── custom
    └── custom:<name>                   tracks created by `tacho emit span|instant|counter|state`
```

### 4.1 Keys and uuids

Every track has a textual key (above). Its uuid is `xxh3_64(key)` with the top
bit cleared. The registry records each track once as a `TrackCreated` log row
before the first event on it; on a hash collision the second key takes
`xxh3_64(key + "#2")` and the row says so. Snapshots reconstruct descriptors
from the `TrackCreated` rows in `events.sqlite` and prune descriptors for empty
tracks so Perfetto UI only displays populated tracks. Children use
`child_ordering = EXPLICIT` and `sibling_order_rank` by creation order, so
agents appear in the order they started.

### 4.2 Lanes: overlapping slices on one run

Perfetto draws begin/end slices on a track as a stack and requires proper
nesting. Agents break nesting: Claude Code issues parallel tool calls; a
subagent's tool calls overlap the parent's. The appender therefore owns an
open-slice table per run and assigns each new tool-level slice to the first
track whose top of stack it nests inside:

1. `turns` if the slice begins inside the currently open Turn and no sibling
   tool slice is open there;
2. otherwise the lowest `lane:k` with nothing open;
3. otherwise a new `lane:k+1`.

Lane assignment is a pure function of the event sequence in `events.sqlite`, so
any snapshot or rebuild produces the exact same tracks. An end whose begin is
unknown increments the `trace_error` count and never emits an unbalanced end.
When a run ends, every slice still open on its tracks is closed at the run's
end with `abandoned = true`. Result: for any interleaving of input, every slice
track in every trace is properly nested. A property test holds this.

### 4.3 State tracks

`agent:<id>/state` and `task:<slug>/state` are TYPE_STATE tracks (verified in
phase 0 against the pinned Trace Processor; if unsupported, they fall back to
slices-per-state with the same names and the fallback is recorded in this
section). Agent state is derived by the appender, not reported:

| state     | entered when                                     |
|-----------|--------------------------------------------------|
| Thinking  | a Turn begins, or a tool slice ends inside a turn |
| Tool      | a tool-level slice begins                         |
| Waiting   | a Wait begins, or the agent is idle for input     |
| Stalled   | a detector fires (section 7)                      |
| Idle      | a Turn ends                                       |
| Exited    | the Run ends                                      |

## 5. Event catalogue

Every event has a `Kind` from one closed enum, a track, a timestamp, an
optional cause, and a typed body. Slices are two events (`X` and `XEnded`),
two log rows, two packets; the end carries the outcome and Trace Processor
merges its args into the slice.

### 5.1 Slices

| name (as shown)                                        | track            | begin body   | end body          |
|--------------------------------------------------------|------------------|--------------|-------------------|
| Run                                                    | run:<n>          | Run          | RunEnded          |
| Turn                                                   | turns            | Turn         | TurnEnded         |
| ToolCall                                               | turns / lane:k   | ToolCall     | ToolCallEnded     |
| Command, Build, Test, Lint, Format, Install, Commit, Push, Fetch, Rebase, Package, Run* | turns / lane:k | Command | CommandEnded |
| Read, Search, List                                     | turns / lane:k   | FileAccess   | FileAccessEnded   |
| Subagent                                               | turns / lane:k   | ToolCall     | ToolCallEnded     |
| Wait                                                   | waits            | Wait         | WaitEnded         |
| Review                                                 | task/review      | Review       | ReviewEnded       |
| Snapshot                                               | daemon           | Snapshot     | SnapshotEnded     |
| Span                                                   | custom:<name>    | Custom       | CustomEnded       |

`Run*`: the command class `RUN` (executing a built program) is shown as
`Exec` to avoid confusion with the Run slice.

The command's slice name is its class, from `classify(argv) -> CommandClass`, a
pure function over the words with a built-in table (`cargo build`,
`npm run build`, `make`, `ninja`, `bazel build`, `go build` are BUILD; `cargo
test`, `pytest`, `go test`, `npm test`, `jest`, `vitest` are TEST; `git push`
is PUSH, ...) and user rules in `$TACHO_HOME/classify.toml` that take
precedence. Compound shell lines (`cd x && cargo test`) classify by the
highest-ranked word group; the rank order is fixed and documented in code.

### 5.2 Instants

| name                         | track          | body          |
|------------------------------|----------------|---------------|
| DaemonStarted, DaemonStopping| daemon         | Daemon        |
| ClockStepped                 | daemon         | ClockStep     |
| TrackCreated                 | daemon         | TrackCreated  |
| TraceError                   | daemon         | TraceError    |
| Prompt                       | turns          | Prompt        |
| FileEdited                   | turns / lane:k | FileEdited    |
| MessageSent, MessageDelivered| turns          | Message       |
| Compacted                    | turns          | Compacted     |
| StallDetected                | agent:<id>     | Stall         |
| ProviderError                | provider:<name>| ProviderError |
| TaskQueued                   | task:<slug>    | TaskQueued    |
| Milestone                    | task:<slug>    | Milestone     |
| ReviewFinding                | task/review    | ReviewFinding |
| Instant                      | custom:<name>  | Custom        |
| Unmatched<Kind>              | as the kind    | (the end body)|

State changes are TYPE_STATE events whose name is the new state and whose body
(`AgentState`, `TaskState`) says why.

### 5.3 Counters

All integer. Units in the CounterDescriptor's `unit_name`.

| track                         | unit            | source                        | mode        |
|-------------------------------|-----------------|-------------------------------|-------------|
| machine/load1                 | load x 1000     | /proc/loadavg                 | sampled     |
| machine/cpu_busy              | per mille       | /proc/stat delta              | sampled     |
| machine/mem_used              | bytes           | /proc/meminfo                 | sampled     |
| machine/disk:<mount>          | bytes used      | statvfs, mounts from config   | sampled     |
| run:<n>/cpu                   | us, cumulative  | /proc/<pid>/stat over pid tree| sampled     |
| run:<n>/rss                   | bytes           | /proc/<pid>/status over tree  | sampled     |
| run:<n>/tokens_in, _out, _cached | tokens, cumulative | usage from hooks/stream/OTLP | event-driven |
| run:<n>/cost                  | micro-USD, cumulative | price table, only if configured | event-driven |
| provider:<name>/tokens_out, /cost | as above, summed | the fold                    | event-driven |
| daemon/events, /trace_us, /queue_depth | count, us, count | the appender         | sampled     |

Cost is never guessed. Subscription plans have no marginal cost; cost tracks
exist only when `$TACHO_HOME/prices.toml` prices the model, or the source
reports cost itself (Claude Code's OTLP `cost_usd`).

### 5.4 Flows

Flows are explicit. A flow id is `seq_of_origin`, placed in `flow_ids` on the
origin's packet and in `terminating_flow_ids` on the destination's packet.
Origins are always events written before the destination exists:

- `MessageSent` -> `MessageDelivered` (the message id).
- a `Subagent` tool call's begin -> the child `Run` begin.
- `StallDetected` -> the next `Run` of the same agent, if it starts with
  `resume = true` within the run's agent.
- a custom origin: `tacho emit ... --flow-out NAME` then `--flow-in NAME`;
  the daemon maps names to ids.

Anything linked after the fact (an OTLP span that names a parent already
written) carries `cause_seq` and no arrow. SQL joins on it.

### 5.5 Schema

`proto/tacho.proto` is the full catalogue. Rules the code must keep:

- Every string field has a maximum length, documented beside it, enforced by
  the appender after scrubbing. Truncation is at a UTF-8 boundary and appends
  `…`.
- Digests are the first 8 bytes of BLAKE3 over the original text. They let two
  events be compared (same tool input repeated) without carrying the text.
- The largest encoded packet of any kind is under 2 KiB; a test holds it.
- `Kind::ALL` in Rust, the `Event.body` oneof and the extension field list
  agree; a test holds it.

## 6. The durable log and packet sequences

### 6.1 One SQLite WAL table

`$TACHO_HOME/events.sqlite` is opened in `WAL` mode with `synchronous = NORMAL`.
One table, `events(seq INTEGER PRIMARY KEY, ts_ns INTEGER, kind INTEGER,
track_key TEXT, cause_seq INTEGER, agent TEXT, body BLOB)`, indexed on `ts_ns`.
Every event — including sampled machine and per-run process-tree counters — is a
row in this table.

Retention is bounded by `retention_days` (default 7 in `$TACHO_HOME/config.toml`),
pruned hourly by the appender in small batches so pruning never stalls ingest.
`tacho status` reports the database size.

### 6.2 Packet sequences and interning

Event names come from the closed `Kind` enum; interned id = discriminant + 1,
constant per build. Each trace header writes the full name dictionary per
packet sequence with `SEQ_INCREMENTAL_STATE_CLEARED`; every event packet carries
`SEQ_NEEDS_INCREMENTAL_STATE` and `name_iid`. Nothing else is interned.
Custom span names (open set) are written inline as `name`.

Events are partitioned across three Perfetto packet sequences by class (`core`,
`agents`, `machine`) with sequence ids `0x7AC0_0001..=0x7AC0_0003`, far from
the small integers `traced` assigns, so a merged system trace cannot collide.

## 7. Detectors

Pure functions over the live state, run by the appender after each event and on
a 1 s tick. Each firing is a `StallDetected` instant with evidence and flips the
agent's state to `Stalled` until the next event that is not itself evidence.

| kind      | fires when (defaults, all configurable)                              |
|-----------|----------------------------------------------------------------------|
| SILENT    | a run is open and no event for 300 s while not in a Wait (CPU-aware: suppressed while child process tree consumes CPU) |
| LOOP      | the same tool input digest 4 times in a row                          |
| SPIN      | 8 turns with no FileEdited and no Command of class BUILD/TEST/COMMIT |
| BURN      | tokens_out over 200k in 10 minutes (or per-task token/cost budget)   |
| FAILING   | the same test/build command failed 3 times in a row                  |
| OFF_BRIEF | a FileEdited outside the task's declared paths (when declared)       |
| CLAIM     | a Run exits 0 after FileEdited without a subsequent passing TEST     |

## 8. Ingestion

All sources produce `Input` values; the appender turns them into events. The
appender stamps time: `ts = max(source_ts or now, last ts on that track)`.
Sources that carry their own time (OTLP, `tacho emit --ts`, the CLI's own
invocation time for hooks) are trusted within ±10 minutes of now; outside it,
now is used and a `ClockStepped` is recorded. Clamping per track keeps
nesting valid even when batches arrive late.

### 8.1 Hooks (Claude Code, Gemini CLI, Jetski)

`tacho hook <provider>` reads one hook payload as JSON on stdin, maps it, sends
it to api.sock, and exits 0. It never blocks the agent: 200 ms total budget,
connect failure or daemon absence are silent (and counted in
`$TACHO_HOME/hook-drops`), and it never writes to stdout (stdout is
interpreted by some agents as a hook decision). `tacho install <provider>`
merges the hook entries into the provider's settings file idempotently and
`tacho uninstall` removes exactly them.

Mapping (Claude Code): `SessionStart` -> Run begin; `UserPromptSubmit` ->
Prompt (digest, bytes) and Turn begin; `PreToolUse` -> tool slice begin by
`tool_use_id`, classified (Bash -> Command, Read/Grep/Glob -> FileAccess,
Edit/Write/MultiEdit -> ToolCall + FileEdited with line counts on end, Task ->
Subagent); `PostToolUse` -> end; `Stop` -> Turn end; `SubagentStop` ->
subagent run end; `PreCompact` -> Compacted; `SessionEnd` -> Run end. Exact
field names are verified in phase 0 against current docs and pinned in test
fixtures.

### 8.2 Wrap: `tacho run -- <cmd>`

Spawns the command with inherited stdio except stdout, which is teed: bytes go
to our stdout unchanged and, when the format is recognised
(`claude --output-format stream-json`, `codex exec --json`,
`gemini --output-format stream-json`), parsed into Inputs. Exit code is
passed through. Recognition is by the output's first lines, never by the
program's name: `codex` on some machines is an unrelated tool. The run's pid is known, so cpu/rss counters are sampled over
its process tree. Unrecognised output is passed through and only Run, counters
and RunEnded are recorded.

### 8.3 OTLP

`tacho daemon --otlp 127.0.0.1:4318` accepts OTLP/HTTP (protobuf and JSON) for
logs, traces and metrics. Mapped: Claude Code and Gemini CLI log events
(`tool_result`, `api_request`, `user_prompt`, ...), their token and cost
metrics, and generic spans as custom spans on `custom:<service.name>`. Unknown
records are counted, not stored.

### 8.4 `tacho emit`

For scripts and orchestrators. One subcommand per public kind family:

```
tacho emit task  <slug> --paths src/a.rs,src/b.rs --brief-file brief.md
tacho emit state <task-slug> <State> [--because TEXT]
tacho emit span  begin <track> <name> [--id ID] [--arg k=v ...]
tacho emit span  end   <track> [--id ID] [--arg k=v ...]
tacho emit instant <track> <name> [--arg k=v ...]
tacho emit counter <track> <value> [--unit NAME]
tacho emit message --from A --to B [--bytes N]
tacho emit finding <task> --severity major --path p --line 3 --title T
```

Also a `--json` form reading one Input per line on stdin for bulk use.

## 9. Snapshot

`tacho trace [-o FILE] [--since 2h] [--until TIME] [--system] [--stdout]` asks
the snapshot thread on `trace.sock` (falling back to `api.sock`, and if the
daemon is unreachable or unresponsive within 2 s, opening
`$TACHO_HOME/events.sqlite` read-only in the CLI process itself). Written, in
order:

1. `ExtensionDescriptor` with `tacho.proto`'s descriptor set.
2. `ClockSnapshot` (REALTIME and BOOTTIME now) with
   `primary_trace_clock = REALTIME`.
3. Per sequence: `TracePacketDefaults` (`timestamp_clock_id = REALTIME`),
   full `InternedData`, `SEQ_INCREMENTAL_STATE_CLEARED`.
4. Every `TrackDescriptor` in the registry (folded from `TrackCreated` rows),
   in creation order.
5. Every matching row from `events` in `seq` order, streamed through the core
   encoder without buffering all rows in memory.
6. A `Snapshot` begin/end on `daemon` with row and byte counts.

Timestamps are CLOCK_REALTIME in nanoseconds (microsecond resolution). The
daemon clamps a backwards wall-clock step and records `ClockStepped`.

`--since` and `--until` (or `--from-log SINCE [UNTIL]` as an alias) filter via
the `ts_ns` index in SQLite and run through the exact same encoder.

On SIGTERM and SIGINT the daemon writes `$TACHO_HOME/trace/last.perfetto-trace`
before exiting.

### 9.1 System tracing

Perfetto's `traced` keeps its own ring buffer. `tacho system start [--profile
default|full|PATH]` starts a ring-buffer session named `tacho-system` with a
config we ship:

- `default` (no root needed): `linux.process_stats` (process tree, per-process
  RSS and CPU time), `linux.sys_stats` (meminfo, vmstat, stat, cpufreq).
- `full` (root, or tracefs permissions): `default` plus `linux.ftrace` with
  sched_switch, sched_waking, task_newtask, task_rename, sched_process_exit,
  cpu_frequency, cpu_idle, power, and `atrace`-free settings.

`tacho system setup --sudo` documents (prints) and, if run with sudo, runs
`traced` and `traced_probes` (from a `tracebox` binary on PATH or one the
user points at). We never invoke sudo ourselves.

`tacho trace --system` runs `perfetto --clone-by-name tacho-system -o -`,
writes that file's packets first, then our snapshot's packets. Trace Processor
joins the two through their clock snapshots. Our sequence ids are disjoint
(section 6.3). Run pids recorded in `Run.pid` match `process.pid` in the system
part, which is what lets SQL attribute scheduling and memory to an agent and
its child commands (section 10).

## 10. Reading it

### 10.1 For agents: status, report, watch

From live state, no Trace Processor needed:

```
$ tacho status
agent      state     for    run  turn  last action                tokens out  flags
worker-1   Tool      12s    1    14    Test cargo test -p core     41k
worker-2   Stalled   6m     2    31    Read src/lib.rs (x4)        88k         LOOP
worker-3   Waiting   40s    1    3     Wait on message             2k
```

`--json` gives the same as one object per agent with stable field names.
`tacho report <agent> [--since 30m]` gives one run as a short timeline:
turns, commands with exit codes, files edited with line counts, failures,
detector firings, tokens. `tacho watch [--json]` streams state changes and
detector firings, one line each, until interrupted; `--until stalled|idle|exit`
returns at the first match, which is how a supervisor blocks on its workers.

The output is designed for a model reader: fixed columns, no colour unless a
TTY, no banners, nothing over 120 columns, and every number has a unit.

### 10.2 For SQL: shipped queries

`queries/` holds named PerfettoSQL files; `tacho query <name|file|-> [--trace
FILE]` runs one through `trace_processor` (found on PATH or via
`$TACHO_TRACE_PROCESSOR`) on a fresh snapshot. Each query exists in standard
syntax; where Trace Processor supports pipe syntax, a `.pipe.sql` twin exists
and a test holds that both return identical rows.

Required queries (each with a test over a scripted fleet):

- `agents-now`: every agent's current state and time in it.
- `open-waits`: runs waiting, on what, for how long.
- `failing-commands`: commands with non-zero exit, by agent and class.
- `time-by-class`: wall time per agent per command class.
- `edits-by-file`: files edited, by how many agents, lines added/removed.
- `tokens-by-agent`: token totals and rate per agent.
- `stalls`: every detector firing with its evidence.
- `verified-trees`: git tree hashes (`git_tree`) verified by passing BUILD/TEST
  commands.
- `system-during-builds` (needs `--system`): CPU time and peak RSS of each
  agent's process tree during its Build and Test slices, using interval
  intersection of the slices with process and sched data.
- `contention`: periods where more than N Build/Test slices overlapped across
  agents, and machine load during them (interval intersection).

## 11. Privacy

Never stored (content firewall): prompt text, model prose, tool output, file
content, diffs, message text, or raw environment values. Represented by 8-byte
BLAKE3 digest and byte size, or a count, or nothing. Paths are made relative to
the run's cwd; absolute paths outside it keep their first two components and
become `<outside>/...`.

Default-on secret scrubbing (`scrub = true` by default in `$TACHO_HOME/config.toml` or
`TACHO_SCRUB=1`; opt-out via `scrub = false` or `TACHO_SCRUB=0`): command argv, review finding
titles, custom args, and custom names pass through regex and shape scrubbers
before persistence. The scrubber replaces with `<redacted>`: `Authorization:`
and `Bearer` values, `--token|--password|--secret|--api-key[= ]value`,
`key=|secret=|token=|password=` values, URL userinfo, AWS/GCP/GitHub/
Anthropic/OpenAI/Slack token shapes, PEM blocks, and base64 or hex runs of 32+
characters (while preserving 40-character hexadecimal Git commit SHAs); plus
user patterns from `$TACHO_HOME/scrub.toml`. Scrub happens before truncation.

A canary test seeds one secret of each shape through every source with
`scrub = true` and asserts no snapshot, rebuild, log row, status output or
report contains any of their bytes.

## 12. Testing

The oracle is `trace_processor_shell`, pinned (phase 0 records the version
and checksum). A trace is valid when it loads with no `stats` rows of severity
`error` or `data_loss` and the expected rows exist in `slice`, `counter`,
`track`, `flow` and `args`.

Required tests, named by the sentence they prove:

1. every kind renders: each Kind with all fields set appears once on its
   track with the expected args keys (generated from the descriptor); every
   packet is under 2 KiB.
2. snapshots under load are whole: one writer at full speed, four concurrent
   snapshot readers in a loop; every snapshot loads clean in the oracle and
   seqs strictly increase.
3. the encoder does not allocate after warm-up (counting allocator).
4. a dead or wedged daemon still produces a trace: with the appender blocked
   (`trace.sock`) and with the daemon `SIGKILL`ed (direct read-only SQLite
   fallback), `tacho trace` returns a loadable file.
5. a restart keeps the tracks: same uuid per key across restart, and
   `Recorder::restore(events)` followed by more inputs produces the exact same
   events as never restarting.
6. any interleaving nests: proptest over random begin/end/instant streams
   from many runs, including ends without begins and begins without ends;
   Trace Processor reports zero nesting errors and every input slice appears.
7. hooks never hurt the agent: daemon absent, daemon wedged, malformed JSON,
   10 MB payload; the hook exits 0 within budget and prints nothing.
8. sources fuzz clean: cargo-fuzz targets for hook payloads, stream-json
   lines, OTLP bodies and `emit --json` lines; no panics.
9. status agrees with SQL: for scripted fleets, `tacho status --json` equals
   `agents-now` over a snapshot taken at the same seq.
10. secrets stay out (the canary).
11. the system merge loads: with an unprivileged traced running the default
    profile, `tacho trace --system` loads clean, and `system-during-builds`
    attributes a scripted `cargo build`-like child's CPU to its agent.
12. the enum, the oneof and the extension list agree.

Benchmarks (reported not asserted): encode per kind, hook round trip (process
start to exit), ingest throughput, snapshot time and peak RSS at 100k, 1M and
10M rows, and ingest throughput while a 10M-row snapshot streams.

## 13. Compromises

- Late causes draw no arrow; `cause_seq` and SQL cover them.
- State tracks need a recent viewer; ui.perfetto.dev is recent.
- The extension range is unregistered upstream; only our own descriptor
  interprets it.
- Hooks see what the agent reports. A command an agent runs outside its tool
  interface is invisible unless the run is wrapped and system tracing is on.

## 14. Autonomous `decide` Engine and Pluggable Interfaces

The orchestrator decision loop (`tacho_core::decide::decide`) is a pure,
deterministic, replayable function `DecideInput -> DecisionPlan` split into five
domain engines:

- `decide::dispatch`: queue scheduling, hold evaluation, model/provider routing,
  cooldowns, worktree start/resume planning, and multi-provider scorecards.
- `decide::verify`: strict `.github/workflows/ci.yaml` parsing (refusing any
  unknown `${{ }}` expression), area + affected-units check selection
  (`Check { area, units }`), presubmit/postsubmit/release-gate batch execution,
  flake ledger with counterexample exclusion, and failing-PR bisection.
- `decide::land`: verified merge-train assembly, regenerable artifact vs. source
  conflict classification, `drop_breakers`, post-merge tree verification, and
  head-SHA-bound release notices (`Pending` / `Sending` / `Sent` / `Uncertain`).
- `decide::watch`: child-process-tree CPU + commit + mtime progress verdicts,
  `(pid, starttime)` and cgroup identity-verified rogue burner reaping, `starve`
  CPU-share budgeting, and `PreToolUse` command guardrails.
- `decide::ops`: tell-once operator notification ledger, versioned redacted
  snapshots, self-update step machine, watchdog issue lifecycle, safe tidy
  rescue (`refs/tacho/rescue/<key>-<ts>`, never `git stash`), and fleet-scoped
  `TMPDIR`/`TMP`/`TEMP`/`JAVA_TOOL_OPTIONS` isolation so nothing writes to `/`.

The daemon autonomously observes world events (agents, PRs, CI, `main`,
worktrees, burners, and host resources) and invokes `decide` on events,
recording each input and resulting `DecisionPlan` (including `tell_actions` and
`WatchAction::RemoveWorktree`) to `decision_ticks` for deterministic replay via
`tacho replay`.

### 14.1 Daemon decide loop

`decide` runs on the sampler thread, but not on every sampler tick. A tick runs
it only when one of these holds:

- **A decide-relevant event was committed.** After each commit the appender
  bumps `AppenderHandle::decide_seq` and wakes the sampler if the batch held
  agent lifecycle or state (`Run`, `RunEnded`, `AgentState`), a `Stall`, a task
  change (`TaskQueued`, `TaskState`), a `Milestone` (commit/push), or progress
  (`FileEdited`, `TurnEnded`). Tool calls, commands, reads, messages and counter
  samples do not count, so the sampler's own samples never retrigger `decide`.
- **A pass was requested explicitly**: `SamplerWaker::request_decide`, or the
  `decide_tick` API command, which runs `execute_decide_tick` directly.
- **The coarse refresh elapsed**: `[sampler] decide_refresh_secs` (default 60 s),
  checked on the sampler's existing `timerfd` tick. It exists only for rules
  that depend on elapsed time alone (stall age, tidy age).

`execute_decide_tick` writes a `decision_ticks` row only when something
changed. It fingerprints the inputs with wall-clock fields zeroed (`now`,
sample timestamps, worktree ages, burner ages, `last_tick_ts`) and the plan
without its `last_tick_ts`. If both match the latest row, it records nothing,
executes nothing, and returns that row's id. A time-based rule that fires
changes the plan, so its tick is still recorded.

None of these loops runs on a fixed cadence:

- The appender blocks on its channel. The only deadline it keeps is the
  earliest moment an agent could turn SILENT: the latest event of each agent
  plus `silent_s`, rescheduled one full timeout later after each detector
  pass. With no such agent it blocks indefinitely.
- Retention pruning checks for shutdown between batches without waiting.
- CLI clients read until they get a reply or the daemon hangs up (EOF). Only
  single-reply interactive requests carry one overall cap
  (`INTERACTIVE_REPLY_CAP`, 30 s), because a human is waiting at the terminal.

**Executors.** The daemon is the authoritative executor of autonomous plans:
`execute_decide_tick` delivers `tell_actions`, removes tidied worktrees and
kills burners. Burner kills follow the same identity rule as the CLI's
`verify_agent_pid`: a recorded `starttime > 0` is required, or nothing is
signalled. The process is pinned with a pidfd, its starttime is checked again,
and `SIGKILL` goes through `pidfd_send_signal`. `ESRCH` means the process is
already gone. Any other failure, such as `EPERM`, is recorded as
`kill_burner_failed` and returned as an error once the tick's state is saved.
CLI paths that still act on plans (`tacho fleet watch` and session commands)
are legacy and must apply the same checks. New executors belong in the daemon.

All external side effects and project-specific rules implement the six synchronous
traits in `tacho_core::decide::plugin`:

- `ProviderPlugin`: model/provider routing, auth/quota readiness, argv synthesis.
- `ForgePlugin`: issues, PRs, labels, comments, merge execution, tree checks.
- `NotifierPlugin`: operator alerts, daily digests, and release notices.
- `ProvisionerPlugin`: worktree creation/removal, cgroup slices, and host probes.
- `WatcherPlugin`: `/proc` process trees, `(pid, starttime)` / cgroup identity,
  and burner reaping.
- `ProjectPolicy`: area classification, proof plans, conflict rules, and
  `PreToolUse` command guardrails.

