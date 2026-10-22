# tachograph: design

A flight recorder for coding agents. One small daemon per machine records what
every agent on it does, as a typed Perfetto trace held in memory, and answers
two kinds of reader: a person opening the trace in ui.perfetto.dev, and an agent
asking "what are my workers doing" in a few hundred tokens.

The binary is `tacho`. The crate is `tachograph`.

This document is the contract the code is built against. Where code and this
document disagree, one of them is a bug.

## 0. Goals and non-goals

Goals:

1. Always on, bounded memory. Recording costs a fixed amount of RAM and a few
   microseconds per event, forever.
2. A snapshot at any moment, without stopping anything: `tacho trace` writes a
   `.perfetto-trace` that opens in ui.perfetto.dev and loads in
   `trace_processor` with zero import errors.
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
7. Nothing secret leaves the machine. Prose, tool output, file content, diffs
   and prompts never enter the trace. Command lines and titles are scrubbed.

Non-goals: controlling agents (kill, resume, schedule), hosted dashboards, MCP.
A supervisor with a shell calls the CLI; that is the integration surface.

## 1. Shape

```
 claude hooks ─┐
 gemini hooks ─┤   api.sock (NDJSON)          ┌─ ring: core     8 MiB ─┐
 jetski hooks ─┼──────────────┐               │                        │
 tacho emit   ─┘              ▼               ├─ ring: agents  64 MiB ─┼─► snapshot thread ─► trace.sock ─► tacho trace
                     ┌─────────────────┐      │                        │        ▲
 tacho run -- cmd ──►│ appender thread │──────┼─ ring: machine  8 MiB ─┘        │ (clone)
  (stream-json)      │ stamp, scrub,   │      │                                 │
 OTLP :4318 ────────►│ classify, lane, │──────┴─► SQLite event log        traced (optional)
 sampler (/proc) ───►│ log, encode,    │              │
                     │ ring, fold      │              └─► tacho trace --from-log
                     └─────────────────┘
                              │
                              └─► live state (fold) ─► tacho status / report / watch
```

One process, `tacho daemon`, started on demand by any client that finds no
socket (like sccache), or by a user service. No async runtime: plain OS
threads and blocking sockets. Threads:

- **acceptors**: one per listening source (api.sock, OTLP, each `tacho run`
  connection). They parse input into `Input` values and send them on one
  bounded channel. They never touch the log, rings or state.
- **appender**: the only consumer of the channel and the only writer of the
  log, the rings, the track registry and the live state. Single writer makes
  every ring single-producer and the seqlock lock-free.
- **snapshot**: owns `trace.sock`. Copies rings. Shares only the rings and the
  registry snapshot with the appender. Answers even when the appender is stuck.
- **sampler**: every `sample_interval` (default 2 s) reads /proc and sends
  counter samples to the appender.
- **query**: serves status/report/watch on api.sock from a read-mostly copy of
  the live state the appender publishes after each batch.

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
├── machine                             counters: load1, cpu_busy, mem_used, disk:<mount>
├── daemon                              lifecycle, snapshots, clock steps, errors
│   └── counters                        events, trace_us, queue_depth
├── agents
│   └── agent:<id>                      description: provider, model, cwd
│       ├── state                       TYPE_STATE: Idle, Thinking, Tool, Waiting, Stalled, Exited
│       └── run:<n>                     slice: Run
│           ├── turns                   Turn > ToolCall | Build | Test | Read | ...
│           ├── lane:1 .. lane:k        overlapping tool calls (section 4.2)
│           ├── waits                   Wait slices
│           ├── tokens_in / tokens_out / tokens_cached / cost
│           └── cpu / rss               wrap mode or pid known
├── tasks
│   └── task:<slug>
│       ├── state                       TYPE_STATE: Queued .. Done
│       └── review                      Review, ReviewFinding
├── providers
│   └── provider:<name>                 errors, rate limits; tokens, cost (summed)
└── custom
    └── custom:<name>                   tracks created by `tacho emit span|instant|counter|state`
```

### 4.1 Keys and uuids

Every track has a textual key (above). Its uuid is `xxh3_64(key)` with the top
bit cleared. The registry records each track once as a `TrackCreated` log row
before the first event on it; on a hash collision the second key takes
`xxh3_64(key + "#2")` and the row says so. Snapshots write descriptors from the
registry; a `--from-log` rebuild writes them from the rows. Tracks are never
renamed. Children use `child_ordering = EXPLICIT` and `sibling_order_rank` by
creation order, so agents appear in the order they started.

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

Lane assignment is a pure function of the event sequence, so ring and log
rebuild agree. An end whose begin is unknown becomes an instant
`Unmatched<Kind>` plus a `trace_error` count, never an unbalanced end. When a
run ends, every slice still open on its tracks is closed at the run's end with
`abandoned = true`. Result: for any interleaving of input, every slice track in
every trace is properly nested. A property test holds this.

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

## 6. The rings

### 6.1 Three rings by class

| ring    | holds                                                     | default |
|---------|-----------------------------------------------------------|---------|
| core    | daemon, tasks, agent state, milestones, providers, custom | 8 MiB   |
| agents  | runs, turns, tool calls, commands, files, waits, messages | 64 MiB  |
| machine | every sampled counter                                     | 8 MiB   |

Configurable in `$TACHO_HOME/config.toml`. A kind's ring is a function of its
kind in the same `match` that maps it to a packet type.

### 6.2 Chunks

A ring is n chunks of 4096 bytes: `gen: AtomicU32` (odd while being written),
`len: AtomicU32`, `words: [AtomicU64; 511]`. A chunk holds whole framed
packets; a packet never straddles chunks. All safe Rust over atomics.

Write: `gen += 1` (Release), store words (Relaxed), `len = new` (Release),
`gen += 1` (Release). Read: `g1 = gen` (Acquire), if odd retry later; copy
`len` then `words[..len]`; `fence(Acquire)`; `g2 = gen`; equal means whole.
After 3 failed attempts the chunk is counted skipped. Wrap overwrites the
oldest chunk whole and increments `overwritten`.

loom checks one writer and one reader over every interleaving.

### 6.3 Interning

Event names come from the closed `Kind` enum; interned id = discriminant + 1,
constant per build. Each snapshot header writes the full name dictionary per
sequence with `SEQ_INCREMENTAL_STATE_CLEARED`; every ring packet carries
`SEQ_NEEDS_INCREMENTAL_STATE` and `name_iid`. Nothing else is interned.
Custom span names (open set) are written inline as `name`.

Sequences: one per ring. Sequence ids are `0x7AC0_0001..=0x7AC0_0003`, far from
the small integers traced assigns, so a merged system trace cannot collide.

## 7. Detectors

Pure functions over the live state, run by the appender after each event and on
a 1 s tick. Each firing is a `StallDetected` instant with evidence and flips the
agent's state to `Stalled` until the next event that is not itself evidence.

| kind      | fires when (defaults, all configurable)                              |
|-----------|----------------------------------------------------------------------|
| SILENT    | a run is open and no event for 300 s while not in a Wait             |
| LOOP      | the same tool input digest 4 times in a row                          |
| SPIN      | 8 turns with no FileEdited and no Command of class BUILD/TEST/COMMIT |
| BURN      | tokens_out over 200k in 10 minutes                                   |
| FAILING   | the same test/build command failed 3 times in a row                  |
| OFF_BRIEF | a FileEdited outside the task's declared paths (when declared)       |

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
passed through. The run's pid is known, so cpu/rss counters are sampled over
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

`tacho trace [-o FILE] [--since 2h] [--system] [--stdout]` asks the snapshot
thread on trace.sock (falling back from api.sock if needed). Written, in order:

1. `ExtensionDescriptor` with `tacho.proto`'s descriptor set.
2. `ClockSnapshot` (REALTIME and BOOTTIME now) with
   `primary_trace_clock = REALTIME`.
3. Per ring sequence: `TracePacketDefaults` (`timestamp_clock_id = REALTIME`),
   full `InternedData`, `SEQ_INCREMENTAL_STATE_CLEARED`.
4. Every `TrackDescriptor` in the registry, in creation order.
5. Chunks of core, agents, machine, oldest to newest.
6. A `Snapshot` begin/end on `daemon` with bytes per ring, chunks overwritten
   and skipped (also written to the core ring).

Timestamps are CLOCK_REALTIME in nanoseconds (microsecond resolution). The
daemon clamps a backwards wall-clock step and records `ClockStepped`.

`--since` filters during the copy by reading each packet's timestamp field
without decoding the rest. `--from-log SINCE [UNTIL]` rebuilds from rows with
the same encoder; the result equals a ring snapshot of the same window byte for
byte, except the ClockSnapshot and the final Snapshot slice (section 12).

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
- `system-during-builds` (needs `--system`): CPU time and peak RSS of each
  agent's process tree during its Build and Test slices, using interval
  intersection of the slices with process and sched data.
- `contention`: periods where more than N Build/Test slices overlapped across
  agents, and machine load during them (interval intersection).

## 11. Privacy

Enforced in the appender, before encoding, so log and trace are clean together.

Never stored: prompt text, model prose, tool output, file content, diffs,
message text, environment values, tokens or keys. Represented by digest and
size, or a count, or nothing.

Stored after scrubbing: command argv, review finding titles, custom args,
custom names. The scrubber replaces with `<redacted>`: `Authorization:` and
`Bearer` values, `--token|--password|--secret|--api-key[= ]value`,
`key=|secret=|token=|password=` values, URL userinfo, AWS/GCP/GitHub/
Anthropic/OpenAI/Slack token shapes, PEM blocks, and base64 or hex runs of 32+
characters; plus user patterns from `$TACHO_HOME/scrub.toml`. Scrub happens
before truncation. Paths are made relative to the run's cwd; absolute paths
outside it keep their first two components and become `<outside>/...`.

A canary test seeds one secret of each shape through every source and asserts
no snapshot, rebuild, log row, status output or report contains any of their
bytes.

## 12. Testing

The oracle is `trace_processor_shell`, pinned (phase 0 records the version
and checksum). A trace is valid when it loads with no `stats` rows of severity
`error` or `data_loss` and the expected rows exist in `slice`, `counter`,
`track`, `flow` and `args`.

Required tests, named by the sentence they prove:

1. every kind renders: each Kind with all fields set appears once on its
   track with the expected args keys (generated from the descriptor); every
   packet is under 2 KiB.
2. the chunk seqlock is correct under every interleaving (loom).
3. overwrite keeps the newest and counts what it lost.
4. snapshots under load are whole: one writer at full speed for 1M events,
   four snapshot threads in a loop; every snapshot loads clean, seqs within a
   sequence strictly increase, and form a contiguous suffix minus reported
   skips.
5. the write path does not allocate after warm-up (counting allocator).
6. a ring snapshot equals the log rebuild of the same window.
7. a restart keeps the tracks: same uuid per key across restart.
8. any interleaving nests: proptest over random begin/end/instant streams
   from many runs, including ends without begins and begins without ends;
   Trace Processor reports zero nesting errors and every input slice appears.
9. hooks never hurt the agent: daemon absent, daemon wedged, malformed JSON,
   10 MB payload; the hook exits 0 within budget and prints nothing.
10. sources fuzz clean: cargo-fuzz targets for hook payloads, stream-json
    lines, OTLP bodies and `emit --json` lines; no panics.
11. status agrees with SQL: for scripted fleets, `tacho status --json` equals
    `agents-now` over a snapshot taken at the same seq.
12. secrets stay out (the canary).
13. the system merge loads: with an unprivileged traced running the default
    profile, `tacho trace --system` loads clean, and `system-during-builds`
    attributes a scripted `cargo build`-like child's CPU to its agent.
14. the wedged daemon still answers: with the appender blocked, `tacho trace`
    via trace.sock returns a loadable file.
15. the enum, the oneof and the extension list agree.

Benchmarks (criterion, reported not asserted): encode+write per kind, hook
round trip (process start to exit), emit throughput, snapshot of full rings,
`--from-log` rebuild rate.

## 13. Compromises

- Late causes draw no arrow; `cause_seq` and SQL cover them.
- Sampled counters exist only in the machine ring; the log keeps one sample a
  minute, so a `--from-log` rebuild is coarser for counters. Test 6 excludes
  sampled counters.
- State tracks need a recent viewer; ui.perfetto.dev is recent.
- The extension range is unregistered upstream; only our own descriptor
  interprets it.
- Chunk slack (under 5% at typical packet sizes) is the price of chunks
  readable on their own.
- Hooks see what the agent reports. A command an agent runs outside its tool
  interface is invisible unless the run is wrapped and system tracing is on.
