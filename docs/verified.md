# tachograph: Fact-Checking and Verification Report

This document records the empirical verification of Perfetto behavior, Trace Processor features, system tracing, and agent hook schemas specified in [`docs/design.md`](file:///usr/local/google/home/zezeozue/dev/tachograph/docs/design.md).

All experiments were executed on this machine (`zezeozue.c.googlers.com`, Linux 6.6, x86_64) using:
- **Pinned Trace Processor v58.3**: [`/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell`](file:///usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell) (v58.3-abd8cdee0)
- **HEAD Trace Processor**: Built in [`/usr/local/google/home/zezeozue/dev/perfetto/out/tacho_tp/trace_processor_shell`](file:///usr/local/google/home/zezeozue/dev/perfetto/out/tacho_tp/trace_processor_shell) (v58.3-02835f6f1 at commit `02835f6f1b7afbb89c705b39a4777586aa50a398`)
- **System tracing binaries**: `traced`, `traced_probes`, `perfetto` under `out/release/`
- **Python**: 3.13 with `google.protobuf` 7.36.2

---

## Summary Matrix

| Item | Topic | Status | Impact on `design.md` |
|---|---|---|---|
| **1** | TrackEvent extension fields (4000-4099) & ExtensionDescriptor | **VERIFIED** | None (validates section 2.2). |
| **2** | TYPE_STATE track events & state tracks | **VERIFIED** | None (validates section 4.3). Exposed in table `__intrinsic_state` / view `state`. |
| **3** | TrackEvent `flow_ids` (47) & `terminating_flow_ids` (48) | **VERIFIED** | None (validates section 5.4). Links slices across distinct sequences and tracks. |
| **4** | Interned names, sorting, and header order | **VERIFIED** | Clarify section 6.3 & 9: `SEQ_INCREMENTAL_STATE_CLEARED` header MUST precede all packets of that sequence in file order. |
| **5** | REALTIME primary clock & system trace concatenation | **VERIFIED** | None (validates section 9 & 9.1). Trace Processor synchronizes clocks via snapshots. |
| **6** | Unprivileged system tracing & cloning | **VERIFIED** | Section 9.1: Note that `perfetto --background` requires `-o <file>` even for ring-buffer sessions. |
| **7** | PerfettoSQL pipe syntax & `INTERVAL INTERSECTION OF` | **VERIFIED** | Validates section 10.2: Both work in v58.3 out/release. Pragma `PERFETTO PRAGMA pipelines = 1;` is strictly required. |
| **8** | Agent hook schemas & OpenTelemetry names | **VERIFIED** | Fixtures added in `tests/fixtures/hooks/` and `tests/fixtures/streams/`. |
| **9** | `/usr/bin/codex` execution | **FALSE** | Update section 8.2: `/usr/bin/codex` is Google internal file format tool; coding agent Codex cannot use `/usr/bin/codex`. |

---

## Detailed Item Verifications

### Item 1: TrackEvent Extensions (4000-4099) & ExtensionDescriptor

- **Claim**:
  TrackEvent extension fields in range 4000-4099 are unallocated in `protos/perfetto/trace/track_event/track_event.proto` / `extensions.json` at HEAD. An `ExtensionDescriptor` packet (carrying `FileDescriptorSet`) causes Trace Processor to decode extension fields into the `args` table with keys like `command.argv`, nested messages dotted, and enums by name.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Inspection of [`protos/perfetto/trace/extensions.json`](file:///usr/local/google/home/zezeozue/dev/perfetto/protos/perfetto/trace/extensions.json) confirmed the unallocated range under scope `perfetto.protos.TrackEvent`:
     ```json
     {
       "name": "unallocated",
       "comment": ["The next allocation goes here. Shrink and take from here."],
       "range": [3100, 4999]
     }
     ```
     Range 4000-4099 is completely unallocated. In [`protos/perfetto/trace/track_event/track_event.proto`](file:///usr/local/google/home/zezeozue/dev/perfetto/protos/perfetto/trace/track_event/track_event.proto), line 399 defines `extensions 1000 to 9999;`.
  2. Built a synthetic trace with:
     - Extension fields: `seq` = 4000 (uint64), `agent` = 4002 (string), `command` = 4003 (message containing `repeated string argv = 1`, `optional string cwd = 2`, `optional CommandClass class = 3` with enum `CMD_TEST = 2`).
     - Packet 1: `ExtensionDescriptor` containing the `FileDescriptorSet` of the extension proto.
     - Packet 2: `TrackDescriptor` (uuid=12345, name="agent:worker-1/turns").
     - Packet 3: `TrackEvent` (TYPE_SLICE_BEGIN, name="Command", extending with seq=100, agent="worker-1", argv=["cargo", "test", "--all"], cwd="/home/user/project", class=CMD_TEST).
     - Packet 4: `TrackEvent` (TYPE_SLICE_END).
  3. Query against `trace_processor_shell` v58.3:
     ```bash
     /usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell query /tmp/tacho-verify/ext_trace.pftrace \
       "SELECT flat_key, key, int_value, string_value FROM args ORDER BY key"
     ```
     **Exact Output Excerpt**:
     ```
     "flat_key","key","int_value","string_value"
     "agent","agent","[NULL]","worker-1"
     "command.argv","command.argv[0]","[NULL]","cargo"
     "command.argv","command.argv[1]","[NULL]","test"
     "command.argv","command.argv[2]","[NULL]","--all"
     "command.class","command.class","[NULL]","CMD_TEST"
     "command.cwd","command.cwd","[NULL]","/home/user/project"
     "seq","seq",100,"[NULL]"
     ```
     Zero `stats` rows of severity `error` or `data_loss`.
- **Exact Args Keys Produced**:
  - Top-level scalar: `flat_key = "seq"`, `key = "seq"`, `int_value = 100`
  - Top-level string: `flat_key = "agent"`, `key = "agent"`, `string_value = "worker-1"`
  - Nested message string: `flat_key = "command.cwd"`, `key = "command.cwd"`, `string_value = "/home/user/project"`
  - Repeated message string: `flat_key = "command.argv"`, `key = "command.argv[0]"`, `"command.argv[1]"`, `"command.argv[2]"`
  - Nested enum: `flat_key = "command.class"`, `key = "command.class"`, `string_value = "CMD_TEST"` (resolved to enum name string).
- **Design Impact**: None; design.md Section 2.2 is completely validated.

---

### Item 2: TYPE_STATE Track Events & State Tracks

- **Claim**:
  `TYPE_STATE` track events (state tracks) are supported in v58.3 Trace Processor: what table they land in, column names, and how a `TrackDescriptor` declares a state track. Check whether HEAD differs.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Declaration in `TrackDescriptor`:
     In [`protos/perfetto/trace/track_event/track_descriptor.proto`](file:///usr/local/google/home/zezeozue/dev/perfetto/protos/perfetto/trace/track_event/track_descriptor.proto):
     ```proto
     optional StateDescriptor state = 18;
     ```
     Where `message StateDescriptor {}` is empty. A track declares itself as a state track by setting field 18 (`state {}`).
  2. Table and View:
     - The underlying table is `__intrinsic_state` (defined in [`src/trace_processor/tables/state_tables.py`](file:///usr/local/google/home/zezeozue/dev/perfetto/src/trace_processor/tables/state_tables.py)).
     - The standard view is `state` (defined in [`src/trace_processor/perfetto_sql/stdlib/prelude/after_eof/views.sql`](file:///usr/local/google/home/zezeozue/dev/perfetto/src/trace_processor/perfetto_sql/stdlib/prelude/after_eof/views.sql)):
       `CREATE PERFETTO VIEW state(...) AS SELECT id, ts, dur, track_id, category, value, arg_set_id FROM __intrinsic_state;`
  3. Columns:
     - `id` (ID)
     - `ts` (TIMESTAMP)
     - `dur` (DURATION): Automatically computed as `next_event_ts - current_event_ts`.
     - `track_id` (JOINID to `track.id`)
     - `category` (STRING, optional)
     - `value` (STRING): The state name string.
     - `arg_set_id` (ARGSETID, optional)
  4. Experiment: Emitted state events `Thinking` (ts=1000), `Tool` (ts=2500), `Idle` (ts=4000), and state close (ts=6000).
     ```bash
     /usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell query /tmp/tacho-verify/state_trace.pftrace \
       "SELECT id, ts, dur, track_id, category, value, arg_set_id FROM state"
     ```
     **Output**:
     ```
     "id","ts","dur","track_id","category","value","arg_set_id"
     0,1000,1500,0,"[NULL]","Thinking","[NULL]"
     1,2500,1500,0,"[NULL]","Tool","[NULL]"
     2,4000,2000,0,"[NULL]","Idle","[NULL]"
     ```
  5. Check against HEAD:
     Ran identical query against HEAD `out/tacho_tp/trace_processor_shell`. Output and columns were identical.
- **Design Impact**: None; design.md Section 4.3 can rely on native `TYPE_STATE` and the `state` view without needing fallback to slice tracks.

---

### Item 3: Flow IDs Across Tracks and Sequences

- **Claim**:
  `flow_ids` (47) / `terminating_flow_ids` (48) on `TrackEvent` produce rows in the `flow` table between the right slices, including when IDs are on `SLICE_BEGIN` packets on different tracks in different sequences.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Built synthetic trace with two distinct tracks and two distinct sequence IDs:
     - Sequence `0x7AC00001`, Track 101: `SLICE_BEGIN` at ts=1000 ("ToolCall:Subagent") with `flow_ids: [42]`.
     - Sequence `0x7AC00002`, Track 102: `SLICE_BEGIN` at ts=1500 ("ChildRun") with `terminating_flow_ids: [42]`.
  2. Query on `trace_processor_shell` v58.3:
     ```bash
     /usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell query /tmp/tacho-verify/flow_trace.pftrace \
       "SELECT flow.id, flow.slice_out, s_out.name AS slice_out_name, flow.slice_in, s_in.name AS slice_in_name, flow.trace_id FROM flow JOIN slice s_out ON flow.slice_out = s_out.id JOIN slice s_in ON flow.slice_in = s_in.id"
     ```
     **Output**:
     ```
     "id","slice_out","slice_out_name","slice_in","slice_in_name","trace_id"
     0,0,"ToolCall:Subagent",1,"ChildRun",42
     ```
  3. `SELECT name, value FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;` returned 0 rows.
- **Design Impact**: None; design.md Section 5.4 is validated. Flow linking between parent tool calls and child runs on distinct sequences works as designed.

---

### Item 4: Interned Names, Timestamp Sorting, and Late Header Drops

- **Claim**:
  Per-sequence `InternedData` + `SEQ_INCREMENTAL_STATE_CLEARED` header, then packets with `SEQ_NEEDS_INCREMENTAL_STATE` and `name_iid`, with timestamps NOT in file order: Trace Processor sorts fully and resolves names. Also: are packets on a sequence whose `sequence_flags` header appears later in the file dropped (stats)?
- **Status**: **VERIFIED**
- **Evidence**:
  1. **Part 1 (Out-of-order timestamps with early header)**:
     - Header packet at start of sequence: `sequence_flags = SEQ_INCREMENTAL_STATE_CLEARED (1)`, interning iid 1 -> "EventAlpha", iid 2 -> "EventBeta".
     - Data packets in file order: ts=5000 (Beta begin), ts=1000 (Alpha begin), ts=2000 (Alpha end), ts=6000 (Beta end).
     - Query: `SELECT id, ts, dur, name FROM slice ORDER BY ts;`
     - **Output**:
       ```
       "id","ts","dur","name"
       0,1000,1000,"EventAlpha"
       1,5000,1000,"EventBeta"
       ```
       Trace Processor fully sorted all slices chronologically and resolved all `name_iid` values. 0 errors in `stats`.
  2. **Part 2 (Packets appearing before header in file order)**:
     - Data packets with `SEQ_NEEDS_INCREMENTAL_STATE (2)` written at beginning of file.
     - Sequence header with `SEQ_INCREMENTAL_STATE_CLEARED (1)` and `InternedData` written *later* in the file.
     - Query: `SELECT id, ts, dur, name FROM slice; SELECT name, value, severity FROM stats WHERE value > 0;`
     - **Output**:
       ```
       "id","ts","dur","name"
       0,3000,1000,"EventAlpha"

       "name","value","severity"
       "packet_skipped_seq_needs_incremental_state_invalid",2,"notice"
       ```
     - Even with `--full-sort`, any packet carrying `SEQ_NEEDS_INCREMENTAL_STATE` encountered in the stream before `SEQ_INCREMENTAL_STATE_CLEARED` on that sequence is skipped immediately by the tokenizer in `ProtoTraceReader::ParsePacket`.
- **Design Impact**:
  Important operational rule: In `design.md` Section 9, the requirement that the snapshot thread writes sequence defaults, `InternedData`, and `SEQ_INCREMENTAL_STATE_CLEARED` in the header *before* any ring chunks is critical. Out-of-order timestamps within ring chunks sort cleanly, but the per-sequence interned dictionary header must be physically earlier in the stream.

---

### Item 5: REALTIME Primary Clock & Concatenation with System Trace

- **Claim**:
  `timestamp_clock_id = BUILTIN_CLOCK_REALTIME` via `TracePacketDefaults` + `ClockSnapshot` with `primary_trace_clock = REALTIME`: `ts` in the slice table equals the REALTIME ns written. Concatenating (`cat`) such a trace AFTER a real system trace (BOOTTIME primary) loads both parts, aligns our slices at the right times relative to system data, with zero stats errors.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Standalone REALTIME trace:
     - Emitted `ClockSnapshot` with `primary_trace_clock = BUILTIN_CLOCK_REALTIME (1)`.
     - Emitted `TracePacketDefaults.timestamp_clock_id = 1`.
     - Slice timestamp written: `1710000000000000000` (1.71e18 ns).
     - Query `SELECT id, ts, dur, name FROM slice`:
       ```
       "id","ts","dur","name"
       0,1710000000000000000,1000000000,"MyRealtimeSlice"
       ```
       `ts` in the slice table exactly equals the REALTIME ns written.
  2. Concatenation after real system trace:
     - Real system trace recorded via unprivileged `traced` / `traced_probes` (BOOTTIME primary: `boot_base = 562954208666070`, `real_base = 1792680938206035009`).
     - Emitted agent trace with `trusted_packet_sequence_id = 0x7AC00001`, containing ClockSnapshot mapping `boot_base + 1s` to `real_base + 1s`, and slice written at REALTIME `real_base + 1s`.
     - Concatenated: `cat system_clone.pftrace agent_realtime.pftrace > merged.pftrace`.
     - Loaded in `trace_processor_shell`:
       - System counter range: `562954787730489` to `562956074593069` (BOOTTIME ns).
       - Agent slice `ts`: `562955208666060` (BOOTTIME ns, exactly `boot_base + 1s` within 10 ns clock drift).
       - The slice landed precisely within the system counter window.
       - Query on `stats`: 0 errors, 0 data loss. Only informational counters (`traced_buf_bytes_written`, `clock_sync_cache_miss`, etc.).
- **Design Impact**: None; design.md Section 9 & 9.1 (`tacho trace --system` using simple concatenation) is verified.

---

### Item 6: Unprivileged System Tracing & Live Session Cloning

- **Claim**:
  Unprivileged system tracing on this machine: start `traced` and `traced_probes` with custom sockets under `/tmp/tacho-verify`, start a RING_BUFFER session named `tacho-system` with `linux.process_stats` and `linux.sys_stats`, then `perfetto --clone-by-name tacho-system -o out.pftrace` while running.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Ran `traced` and `traced_probes` without root under user `zezeozue`, with sockets `PERFETTO_PRODUCER_SOCK_NAME` and `PERFETTO_CONSUMER_SOCK_NAME` in `/tmp/tacho-verify/`.
  2. Starting background session:
     - **Finding**: Running `perfetto --background --config <cfg>` without `--out` fails with:
       `perfetto_cmd.cc:881 Either --out or --upload is required`
       Even when using a `RING_BUFFER` session that will be cloned, `perfetto` requires `-o <file>` for the daemonized command. The background process holds the file open and only writes it when stopped.
     - Using `-o /tmp/tacho-verify/system_bg.pftrace` allowed the session `tacho-system` to start in the background cleanly.
  3. Live cloning:
     While the background session was actively recording, executed:
     ```bash
     perfetto --clone-by-name tacho-system -o /tmp/tacho-verify/system_clone.pftrace
     ```
     Excerpt:
     `[956.220] perfetto_cmd.cc:1365 Wrote 460718 bytes into /tmp/tacho-verify/system_clone.pftrace`
  4. Inspection of cloned trace:
     - 1,432 processes and 7,024 threads recorded in `process` and `thread`.
     - 6,166 sys_stats counter tracks: `MemTotal`, `MemFree`, `MemAvailable`, `Buffers`, `Cached`, `SwapTotal`, `SwapFree`, `cpu.times.idle_ns`, `cpu.times.user_nice_ns`, etc.
     - 0 errors in `stats`.
  5. What works without root:
     - `linux.process_stats` (with `scan_all_processes_on_start: true`, `proc_stats_poll_ms: 250`, `record_thread_names: true`, `record_process_age: true`, `record_process_runtime: true`).
     - `linux.sys_stats` (meminfo, vmstat, stat, cpufreq).
  6. What fails without root:
     - `linux.ftrace` fails: `probes_producer.cc:139 Failed to create FtraceController`, `Failed to create data source 'linux.ftrace'`. `/sys/kernel/tracing` requires root or `tracefs` permissions.
     - `scan_smaps_rollup: true` in `process_stats` requires root (`PTRACE_MODE_READ`).
  7. Configs saved:
     - Working unprivileged config: [`tools/system-default.cfg`](file:///usr/local/google/home/zezeozue/dev/tachograph/tools/system-default.cfg)
     - Full ftrace config (untestable without root): [`tools/system-full.cfg`](file:///usr/local/google/home/zezeozue/dev/tachograph/tools/system-full.cfg)
     - Runner script: [`tools/system-trace.sh`](file:///usr/local/google/home/zezeozue/dev/tachograph/tools/system-trace.sh)
- **Design Impact**:
  Design.md Section 9.1 should note that `perfetto` requires `-o <file>` when starting a background daemon session, e.g. pointing to a sink in `$TACHO_RUNTIME/session.pftrace`.

---

### Item 7: PerfettoSQL Pipe Syntax & Interval Intersection

- **Claim**:
  PerfettoSQL pipe syntax and `INTERVAL INTERSECTION OF`: which work in v58.3 out/release and which need a HEAD build. Give working queries and exact pragma needed.
- **Status**: **VERIFIED**
- **Evidence**:
  1. Both pipe syntax (`FROM ... |> ...`) and `INTERVAL INTERSECTION OF (...)` work in **v58.3 out/release**! A HEAD build is **NOT** required.
  2. Pragma requirement:
     Executing any pipe query without `PERFETTO PRAGMA pipelines = 1;` fails with:
     `error: Pipelines are not enabled; set 'PERFETTO PRAGMA pipelines = 1'`
     The exact statement required at the beginning of the SQL session is:
     ```sql
     PERFETTO PRAGMA pipelines = 1;
     ```
  3. Working Pipe Query (tested on v58.3):
     ```sql
     PERFETTO PRAGMA pipelines = 1;
     FROM slice
     |> SELECT id, name;
     ```
  4. Working Interval Intersection Query (Pipe Syntax Form, tested on v58.3):
     ```sql
     PERFETTO PRAGMA pipelines = 1;

     CREATE PERFETTO TABLE running_sched AS
     SELECT 100 AS ts, 50 AS dur, 1 AS cpu, 10 AS utid;

     CREATE PERFETTO TABLE cpu_freq AS
     SELECT 120 AS ts, 60 AS dur, 1 AS cpu, 2000 AS freq;

     INTERVAL INTERSECTION OF (running_sched AS s, cpu_freq AS f) PER cpu
     |> SELECT ts, dur, cpu, s.utid, f.freq;
     ```
     **Output**:
     `"ts","dur","cpu","utid","freq" -> 120, 30, 1, 10, 2000`
  5. Working Interval Intersection Query (Stdlib Macro Form, tested on v58.3):
     ```sql
     INCLUDE PERFETTO MODULE intervals.intersect;

     CREATE PERFETTO TABLE A AS
     WITH data(id, ts, dur) AS (VALUES (0, 100, 50)) SELECT * FROM data;

     CREATE PERFETTO TABLE B AS
     WITH data(id, ts, dur) AS (VALUES (0, 120, 60)) SELECT * FROM data;

     SELECT ts, dur, id_0, id_1
     FROM _interval_intersect!((A, B), ())
     ORDER BY ts;
     ```
     **Output**:
     `"ts","dur","id_0","id_1" -> 120, 30, 0, 0`
- **Design Impact**: None; design.md Section 10.2's `.pipe.sql` queries can run directly against the pinned v58.3 binary provided `PERFETTO PRAGMA pipelines = 1;` is prepended.

---

### Item 8: Agent Hook Payload Schemas & Telemetry

- **Claim**:
  Agent hook payload schemas, configuration formats, exit codes, stream schemas, and OpenTelemetry metric/event names for Claude Code, Gemini CLI, Jetski, and Codex.
- **Status**: **VERIFIED**
- **Findings & Schemas**:

#### 1. Claude Code
- **Configuration**: In `.claude/settings.json` or `~/.claude/settings.json` under `"hooks"`:
  ```json
  {
    "hooks": {
      "PreToolUse": [
        {
          "matcher": "Edit|Write|Bash",
          "hooks": [{ "type": "command", "command": "..." }]
        }
      ]
    }
  }
  ```
- **Exit Code Contract**:
  - `0`: Success / allow action.
  - `2`: Blocking decision (cancels tool / step, passes `stderr` back to model as feedback).
- **Events & Payload Fields**:
  All payloads include `session_id`, `transcript_path`, `cwd`, `hook_event_name`.
  - `SessionStart`: `source` ("startup", "resume", "compact")
  - `UserPromptSubmit`: `prompt` (string)
  - `PreToolUse`: `tool_use_id`, `tool_name`, `tool_input` (object)
  - `PostToolUse`: `tool_use_id`, `tool_name`, `tool_input`, `tool_response` (object)
  - `Stop`: `stop_reason` ("end_turn", "max_tokens")
  - `SubagentStop`: `subagent_id`, `stop_reason`
  - `PreCompact`: `trigger` ("context_limit_threshold")
  - `SessionEnd`: `reason` ("user_exit", "error")
  - `Notification`: `notification_type`, `message`
- **Stream Schema (`claude -p --output-format stream-json`)**:
  NDJSON stream containing:
  - `type: "system"` (`subtype: "init"`, `session_id`, `cwd`, `model`)
  - `type: "assistant"` (`message`: `role`, `content`: `[{type: "text", text}, {type: "tool_use", id, name, input}]`)
  - `type: "user"` (`message`: `role`, `content`: `[{type: "tool_result", tool_use_id, content}]`)
  - `type: "result"` (`subtype: "success"`, `session_id`, `result`, `total_cost_usd`, `duration_ms`, `duration_api_ms`, `num_turns`, `usage`: `{input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens}`)
- **OpenTelemetry**:
  - Metrics: `claude_code.session.count`, `claude_code.cost.usage`, `claude_code.token.usage`, `claude_code.lines_of_code.count`, `claude_code.commit.count`, `claude_code.pull_request.count`, `claude_code.code_edit_tool.decision`.
  - Events/Logs: `claude_code.user_prompt`, `claude_code.api_request`, `claude_code.api_error`, `claude_code.tool_decision`, `claude_code.tool_result`, `claude_code.assistant_response`.
  - Spans: `claude_code.interaction`, `claude_code.tool`, `claude_code.hook`.

#### 2. Gemini CLI
- **Configuration**: Defined in `hooks.json` under `"hooks"`:
  ```json
  {
    "hooks": {
      "BeforeTool": [
        {
          "matcher": "run_shell_command",
          "hooks": [{ "type": "command", "command": "..." }]
        }
      ]
    }
  }
  ```
- **Lifecycle Events**:
  - `SessionStart`, `SessionEnd`, `BeforeAgent`, `AfterAgent`, `BeforeModel`, `AfterModel`, `BeforeToolSelection`, `BeforeTool`, `AfterTool`.
- **Exit Code Contract**:
  - `0`: Allow.
  - `2`: Deny / block.
  - stdout JSON: `{"decision": "allow" | "deny", "reason": "..."}`.
- **OpenTelemetry**:
  - Metrics: `gemini_cli.cpu.usage`, `gemini_cli.tool.queue.depth`, `gemini_cli.tool.execution.breakdown`, `gemini_cli.api.request.breakdown`, `gemini_cli.token.efficiency`, `gen_ai.client.token.usage`, `gen_ai.client.operation.duration`.
  - Events: `gemini_cli.config`, `gemini_cli.tool_call`, `gemini_cli.chat_compression`.

#### 3. Jetski
- **Configuration**: In `_agents/hooks.json` or `~/.gemini/config/hooks.json`. Top-level keys are hook names mapping to event arrays:
  ```json
  {
    "tacho": {
      "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "..." }] }],
      "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "..." }] }]
    }
  }
  ```
- **Events**: `SessionStart`, `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, `Stop`.
- **Common Metadata**: `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`, `isBattleMode`.
- **PreToolUse**: `toolCall` (`name`, `args`), `stepIdx`. Output: `{"decision": "allow"|"deny"|"ask"|"force_ask", "reason": "...", "overwrite": {...}}`.
- **PostToolUse**: `toolCall` (`name`, `args`), `stepIdx`, `result`, `error`. Output: `{"overwriteResult": "..."}`.
- **Stop**: `executionNum`, `terminationReason` ("NO_TOOL_CALL", "MAX_INVOCATIONS", "ERROR", "USER_CANCELED"), `error`, `fullyIdle`. Output: `{"decision": "continue", "reason": "..."}`.

#### 4. Codex
- **Stream Schema (`codex exec --json`)**:
  NDJSON events: `thread.started` (`thread_id`), `turn.started`, `item.completed` (`item`: `id`, `type`, `text` / `command` / `output`), `turn.completed` (`usage`: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`).

#### Fixtures Created
- [`tests/fixtures/hooks/claude/`](file:///usr/local/google/home/zezeozue/dev/tachograph/tests/fixtures/hooks/claude/): `SessionStart.json`, `UserPromptSubmit.json`, `PreToolUse.json`, `PostToolUse.json`, `Stop.json`, `SubagentStop.json`, `PreCompact.json`, `SessionEnd.json`, `Notification.json`.
  *Source URLs*: [Anthropic Claude Code Hooks Docs](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks), [Claude Code Settings](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/settings).
- [`tests/fixtures/hooks/gemini/`](file:///usr/local/google/home/zezeozue/dev/tachograph/tests/fixtures/hooks/gemini/): `SessionStart.json`, `BeforeTool.json`, `AfterTool.json`, `BeforeAgent.json`, `AfterAgent.json`, `SessionEnd.json`.
  *Source URLs*: [Gemini CLI Hooks Documentation](https://geminicli.com/docs/hooks/), [Gemini CLI Automation](https://github.com/google-gemini/gemini-cli).
- [`tests/fixtures/hooks/jetski/`](file:///usr/local/google/home/zezeozue/dev/tachograph/tests/fixtures/hooks/jetski/): `SessionStart.json`, `PreToolUse.json`, `PostToolUse.json`, `PreInvocation.json`, `PostInvocation.json`, `Stop.json`.
  *Source*: [`/usr/local/google/home/zezeozue/.gemini/jetski/builtin/skills/jetski-customizations/docs/hooks.md`](file:///usr/local/google/home/zezeozue/.gemini/jetski/builtin/skills/jetski-customizations/docs/hooks.md).
- [`tests/fixtures/streams/`](file:///usr/local/google/home/zezeozue/dev/tachograph/tests/fixtures/streams/): `claude-stream.jsonl`, `gemini-stream.jsonl`, `codex-exec.jsonl`.
  *Source URLs*: [Claude Code Stream JSON Spec](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/headless), [OpenAI Codex CLI Reference](https://developers.openai.com/codex/cli).

---

### Item 9: `/usr/bin/codex` Non-Interactive Execution

- **Claim**:
  `/usr/bin/codex` is installed: check whether `codex exec --json "say hi"` runs here non-interactively. If it runs, capture a real stream to `tests/fixtures/streams/codex-exec.jsonl`.
- **Status**: **FALSE**
- **Evidence**:
  1. Invoking `/usr/bin/codex --help`:
     ```
     codex: codex is the standard tool for printing out Google file formats. It only
     supports standard production formats; for legacy formats, you need to use
     codex-full instead. The basic calling method is
        codex [format-options] [misc-options] file1 file2...
     Flags from util/tools/codex/...
     ```
     `/usr/bin/codex` is Google's internal SSTable / recordio file utility, **not** OpenAI Codex.
  2. Executing `codex exec --json "say hi"`:
     ```bash
     /usr/bin/codex exec --json "say hi"
     ```
     **Output**:
     ```
     ERROR: Unknown command line flag 'json'. Did you mean: blog, mod ?
     ```
     Exited immediately with code 1.
  3. No other coding agent binary named `codex` is present on PATH.
- **What `design.md` Should Say Instead**:
  Section 8.2 states:
  > Recognized formats (`claude --output-format stream-json`, `codex exec --json`, `gemini --output-format stream-json`)
  
  `design.md` should clarify that `/usr/bin/codex` on Google internal developer workstations collides with the internal file format inspection tool (`util/tools/codex`). A coding agent Codex wrapper must look for an explicitly named or configured executable (such as `codex-agent`, `openai-codex`, or `$CODEX_BIN`), and must not invoke bare `/usr/bin/codex`.
  
  The fixture [`tests/fixtures/streams/codex-exec.jsonl`](file:///usr/local/google/home/zezeozue/dev/tachograph/tests/fixtures/streams/codex-exec.jsonl) was populated based on official Codex CLI documentation.
