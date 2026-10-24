use super::*;
use crate::log::EventLog;
use tacho_core::{Encoder, Event, Kind};
use tacho_proto::tacho;

#[test]
fn test_extract_packet_timestamp() {
    let registry = TrackRegistry::new();
    let mut encoder = Encoder::new();
    let mut out = Vec::new();

    let ev = Event::new(
        1,
        123456789,
        Kind::Run,
        "agent:1/run:1",
        None,
        None,
        tacho::Event { body: None },
    );
    encoder.encode_event(&ev, &registry, &mut out);

    let (ts, total_len) = extract_framed_packet_timestamp(&out).unwrap();
    assert_eq!(ts, 123456789);
    assert_eq!(total_len, out.len());
}

#[test]
fn test_write_snapshot_streaming() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("events.sqlite");
    let mut log = EventLog::open(&db_path).unwrap();

    let ev = Event::new(
        1,
        1_000_000,
        Kind::Daemon,
        "daemon",
        None,
        None,
        tacho::Event { body: None },
    );
    log.insert_batch(&[ev]).unwrap();

    let mut output = Vec::new();
    let stats = write_snapshot(&db_path, None, None, &mut output).unwrap();

    assert_eq!(stats.events, 1);
    assert!(stats.bytes > 0);
    assert!(!output.is_empty());
}

#[test]
fn test_write_snapshot_and_rebuild_from_log_byte_identical() {
    use tacho_core::{Input, MessageDirection, SessionKey, TaskStatus, ToolInput};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("events.sqlite");
    let mut log = EventLog::open(&db_path).unwrap();

    let mut rec = Recorder::new();
    let mut events = Vec::new();
    let t0 = 1_000_000_000u64;
    let sess = SessionKey::new("claude", "sess-rebuild");

    rec.apply(
        Input::Task {
            source_ts_ns: Some(t0),
            slug: "rebuild-task".into(),
            brief_digest: None,
            brief_bytes: Some(128),
            paths: vec!["src/main.rs".into()],
            agent: Some("worker-1".into()),
            base_commit: None,
            token_budget: None,
            cost_budget_micros: None,
        },
        t0,
        &mut events,
    );
    rec.apply(
        Input::RunStart {
            source_ts_ns: Some(t0 + 100),
            provider: "claude".into(),
            session_id: "sess-rebuild".into(),
            agent: Some("worker-1".into()),
            task: Some("rebuild-task".into()),
            pid: Some(1234),
            cwd: Some("/workspace".into()),
            model: Some("claude-sonnet-4-6".into()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        t0 + 100,
        &mut events,
    );
    rec.apply(
        Input::Prompt {
            source_ts_ns: Some(t0 + 200),
            session: sess.clone(),
            text_len: 50,
            digest: None,
        },
        t0 + 200,
        &mut events,
    );
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(t0 + 300),
            session: sess.clone(),
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            input: ToolInput::Shell {
                argv: "cargo test".into(),
                cwd: None,
                git_tree: None,
            },
        },
        t0 + 300,
        &mut events,
    );
    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(t0 + 400),
            session: sess.clone(),
            tool_use_id: "t1".into(),
            error: false,
            result_bytes: Some(42),
            exit_code: Some(0),
        },
        t0 + 400,
        &mut events,
    );
    rec.apply(
        Input::Message {
            source_ts_ns: Some(t0 + 500),
            from: "orch".into(),
            to: "worker-1".into(),
            id: Some("m-1".into()),
            bytes: Some(16),
            dir: Some(MessageDirection::Sent),
        },
        t0 + 500,
        &mut events,
    );
    rec.apply(
        Input::Message {
            source_ts_ns: Some(t0 + 600),
            from: "orch".into(),
            to: "worker-1".into(),
            id: Some("m-1".into()),
            bytes: Some(16),
            dir: Some(MessageDirection::Delivered),
        },
        t0 + 600,
        &mut events,
    );
    rec.apply(
        Input::TaskState {
            source_ts_ns: Some(t0 + 700),
            slug: "rebuild-task".into(),
            state: TaskStatus::Done,
            because: Some("all green".into()),
        },
        t0 + 700,
        &mut events,
    );

    log.insert_batch(&events).unwrap();

    let until = Some(t0 + 1_000);
    let mut snap_bytes = Vec::new();
    let mut rebuild_bytes = Vec::new();
    let s1 = write_snapshot(&db_path, None, until, &mut snap_bytes).unwrap();
    let s2 = rebuild_from_log(&db_path, None, until, &mut rebuild_bytes).unwrap();

    assert_eq!(s1, s2);
    assert_eq!(
        snap_bytes, rebuild_bytes,
        "write_snapshot and rebuild_from_log must be byte-for-byte identical at the same until"
    );
}

mod structure {
    //! Asserts the shape of a realistic trace as a human sees it in ui.perfetto.dev.
    use super::*;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use tacho_core::{
        CustomOp, FindingSeverity, Input, MessageDirection, SessionKey, TaskStatus, ToolInput,
    };
    use tacho_proto::perfetto::protos::{self as pb, trace_packet::Data, track_event};

    const T0: u64 = 1_700_000_000_000_000_000;
    const MS: u64 = 1_000_000;

    fn sess(id: &str) -> SessionKey {
        SessionKey::new("claude", id)
    }

    fn start(id: &str, agent: &str, task: Option<&str>, pid: u32, parent: Option<&str>) -> Input {
        Input::RunStart {
            source_ts_ns: None,
            provider: "claude".into(),
            session_id: id.into(),
            agent: Some(agent.into()),
            task: task.map(Into::into),
            pid: Some(pid),
            cwd: Some(format!("/work/{agent}")),
            model: Some("claude-sonnet-4-6".into()),
            parent_session: parent.map(Into::into),
            git_tree: None,
            role: None,
        }
    }

    fn tool(id: &str, name: &str, input: ToolInput, s: &str) -> Input {
        Input::ToolBegin {
            source_ts_ns: None,
            session: sess(s),
            tool_use_id: id.into(),
            tool_name: name.into(),
            input,
        }
    }

    fn tool_end(id: &str, s: &str, error: bool) -> Input {
        Input::ToolEnd {
            source_ts_ns: None,
            session: sess(s),
            tool_use_id: id.into(),
            error,
            result_bytes: Some(10),
            exit_code: Some(if error { 1 } else { 0 }),
        }
    }

    fn custom(track: &str, op: CustomOp, name: Option<&str>, args: &[(&str, &str)]) -> Input {
        Input::Custom {
            source_ts_ns: None,
            track: track.into(),
            op,
            name: name.map(Into::into),
            id: None,
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            value: None,
            flow_out: None,
            flow_in: None,
        }
    }

    /// An orchestrator hands a task to a worker, which edits, tests, commits, gets stuck in
    /// a loop, is messaged, recovers and finishes; a reviewer files a finding.
    pub(super) fn scenario() -> Vec<Input> {
        let shell = |argv: &str| ToolInput::Shell {
            argv: argv.into(),
            cwd: None,
            git_tree: None,
        };
        vec![
            start("orch", "orchestrator", None, 100, None),
            Input::Task {
                source_ts_ns: None,
                slug: "fix-parser".into(),
                brief_digest: None,
                brief_bytes: Some(512),
                paths: vec!["src/parser.rs".into()],
                agent: Some("worker-1".into()),
                base_commit: None,
                token_budget: None,
                cost_budget_micros: None,
            },
            start("w1", "worker-1", Some("fix-parser"), 200, Some("orch")),
            Input::TaskState {
                source_ts_ns: None,
                slug: "fix-parser".into(),
                state: TaskStatus::InProgress,
                because: None,
            },
            Input::Prompt {
                source_ts_ns: None,
                session: sess("w1"),
                text_len: 300,
                digest: None,
            },
            Input::TurnStart {
                source_ts_ns: None,
                session: sess("w1"),
                model: None,
            },
            tool(
                "t1",
                "Read",
                ToolInput::FileRead {
                    path: "src/parser.rs".into(),
                },
                "w1",
            ),
            tool_end("t1", "w1", false),
            tool(
                "t1e",
                "Edit",
                ToolInput::FileEdit {
                    path: "src/parser.rs".into(),
                    kind: None,
                    lines_added: Some(5),
                    lines_removed: Some(2),
                },
                "w1",
            ),
            tool_end("t1e", "w1", false),
            tool("t2", "Bash", shell("cargo test -p parser"), "w1"),
            custom(
                "agent:worker-1/run:1/procs/slot:1",
                CustomOp::Begin,
                Some("cargo [pid 201]: cargo test -p parser"),
                &[("pid", "201")],
            ),
            Input::CounterSample {
                source_ts_ns: None,
                track_key: "agent:worker-1/run:1/cpu".into(),
                value: 1_500_000,
                unit: Some("us".into()),
            },
            custom(
                "agent:worker-1/run:1/procs/slot:1",
                CustomOp::End,
                None,
                &[("cpu_us", "1500000")],
            ),
            tool_end("t2", "w1", true),
            Input::TurnEnd {
                source_ts_ns: None,
                session: sess("w1"),
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cached_tokens: None,
                text_bytes: None,
                cost_micros: Some(3000),
            },
            custom(
                "agent:worker-1",
                CustomOp::Instant,
                Some("WorktreeCommit"),
                &[
                    ("sha", "abc123"),
                    ("commits", "1"),
                    ("cwd", "/work/worker-1"),
                ],
            ),
            tool("t3", "Bash", shell("cat missing.txt"), "w1"),
            tool_end("t3", "w1", true),
            tool("t4", "Bash", shell("cat missing.txt"), "w1"),
            tool_end("t4", "w1", true),
            tool("t5", "Bash", shell("cat missing.txt"), "w1"),
            tool_end("t5", "w1", true),
            tool("t6", "Bash", shell("cat missing.txt"), "w1"),
            tool_end("t6", "w1", true),
            Input::Message {
                source_ts_ns: None,
                from: "orchestrator".into(),
                to: "worker-1".into(),
                id: Some("m1".into()),
                bytes: Some(64),
                dir: Some(MessageDirection::Sent),
            },
            Input::Message {
                source_ts_ns: None,
                from: "orchestrator".into(),
                to: "worker-1".into(),
                id: Some("m1".into()),
                bytes: Some(64),
                dir: Some(MessageDirection::Delivered),
            },
            Input::TurnStart {
                source_ts_ns: None,
                session: sess("w1"),
                model: None,
            },
            tool("t7", "Bash", shell("cargo test -p parser --lib"), "w1"),
            tool_end("t7", "w1", false),
            Input::TurnEnd {
                source_ts_ns: None,
                session: sess("w1"),
                input_tokens: Some(500),
                output_tokens: Some(100),
                cached_tokens: None,
                text_bytes: None,
                cost_micros: Some(1000),
            },
            Input::Finding {
                source_ts_ns: None,
                task: "fix-parser".into(),
                severity: FindingSeverity::Major,
                path: Some("src/parser.rs".into()),
                line: Some(42),
                title: "unchecked index".into(),
            },
            Input::TaskState {
                source_ts_ns: None,
                slug: "fix-parser".into(),
                state: TaskStatus::Done,
                because: Some("tests green".into()),
            },
            Input::RunEnd {
                source_ts_ns: None,
                session: sess("w1"),
                exit_code: Some(0),
                reason: None,
            },
        ]
    }

    pub(super) fn scenario_db(dir: &Path) -> std::path::PathBuf {
        let db = dir.join("events.sqlite");
        let mut log = EventLog::open(&db).unwrap();
        let mut rec = Recorder::new();
        let mut events = Vec::new();
        for (i, input) in scenario().into_iter().enumerate() {
            rec.apply(input, T0 + i as u64 * 1_000 * MS, &mut events);
        }
        log.insert_batch(&events).unwrap();
        db
    }

    /// A decoded trace, reduced to what a human sees: named tracks and slices.
    pub(super) struct TraceModel {
        /// uuid -> (name, parent uuid)
        pub tracks: HashMap<u64, (String, Option<u64>)>,
        pub events: Vec<TraceEvent>,
    }

    #[derive(Debug, Clone)]
    pub(super) struct TraceEvent {
        pub track: u64,
        pub ty: i32,
        pub ts: u64,
        pub name: String,
        pub flow_ids: Vec<u64>,
        pub terminating_flow_ids: Vec<u64>,
    }

    pub(super) const SLICE_BEGIN: i32 = 1;
    pub(super) const SLICE_END: i32 = 2;
    pub(super) const INSTANT: i32 = 3;

    impl TraceModel {
        pub fn decode(bytes: &[u8]) -> Self {
            let trace = pb::Trace::decode(bytes).expect("trace must decode");
            let mut tracks = HashMap::new();
            let mut names: HashMap<u64, String> = HashMap::new();
            let mut events = Vec::new();
            for packet in trace.packet {
                if let Some(interned) = &packet.interned_data {
                    for n in &interned.event_names {
                        names.insert(n.iid.unwrap_or(0), n.name.clone().unwrap_or_default());
                    }
                }
                match packet.data {
                    Some(Data::TrackDescriptor(d)) => {
                        let name = match d.static_or_dynamic_name {
                            Some(pb::track_descriptor::StaticOrDynamicName::Name(n))
                            | Some(pb::track_descriptor::StaticOrDynamicName::StaticName(n)) => n,
                            _ => String::new(),
                        };
                        tracks.insert(d.uuid.unwrap_or(0), (name, d.parent_uuid));
                    }
                    Some(Data::TrackEvent(te)) => {
                        let name = match te.name_field {
                            Some(track_event::NameField::Name(n)) => n,
                            Some(track_event::NameField::NameIid(i)) => {
                                names.get(&i).cloned().unwrap_or_default()
                            }
                            None => String::new(),
                        };
                        events.push(TraceEvent {
                            track: te.track_uuid.unwrap_or(0),
                            ty: te.r#type.unwrap_or(0),
                            ts: packet.timestamp.unwrap_or(0),
                            name,
                            flow_ids: te.flow_ids,
                            terminating_flow_ids: te.terminating_flow_ids,
                        });
                    }
                    _ => {}
                }
            }
            Self { tracks, events }
        }

        /// Slash-joined display path of a track, e.g. `tacho/agents/worker-1/stalls`.
        pub fn path(&self, uuid: u64) -> String {
            let mut parts = Vec::new();
            let mut cur = Some(uuid);
            while let Some(u) = cur {
                let Some((name, parent)) = self.tracks.get(&u) else {
                    break;
                };
                parts.push(name.clone());
                cur = *parent;
            }
            parts.reverse();
            parts.join("/")
        }

        pub fn track_by_path(&self, path: &str) -> Option<u64> {
            self.tracks.keys().copied().find(|&u| self.path(u) == path)
        }

        /// Complete slices `(track path, name, begin, end)`; panics on an unmatched end.
        pub fn slices(&self) -> Vec<(String, String, u64, Option<u64>)> {
            let mut stacks: HashMap<u64, Vec<usize>> = HashMap::new();
            let mut out: Vec<(String, String, u64, Option<u64>)> = Vec::new();
            for ev in &self.events {
                match ev.ty {
                    SLICE_BEGIN => {
                        stacks.entry(ev.track).or_default().push(out.len());
                        out.push((self.path(ev.track), ev.name.clone(), ev.ts, None));
                    }
                    SLICE_END => {
                        let idx = stacks
                            .get_mut(&ev.track)
                            .and_then(|s| s.pop())
                            .unwrap_or_else(|| panic!("unmatched end on {}", self.path(ev.track)));
                        out[idx].3 = Some(ev.ts);
                    }
                    _ => {}
                }
            }
            out
        }
    }

    fn scenario_trace() -> TraceModel {
        let temp = tempfile::tempdir().unwrap();
        let db = scenario_db(temp.path());
        let mut out = Vec::new();
        write_snapshot(&db, None, Some(T0 + 60_000 * MS), &mut out).unwrap();
        TraceModel::decode(&out)
    }

    #[test]
    fn test_track_hierarchy_reads_naturally() {
        let trace = scenario_trace();
        for path in [
            "tacho/agents/orchestrator/run 1",
            "tacho/agents/worker-1/run 1/turns",
            "tacho/agents/worker-1/run 1/procs/slot 1",
            "tacho/agents/worker-1/run 1/cpu",
            "tacho/agents/worker-1/run 1/worktree",
            "tacho/agents/worker-1/run 1/commits",
            "tacho/agents/worker-1/run 1/files_edited",
            "tacho/agents/worker-1/stalls",
            "tacho/agents/worker-1/state",
            "tacho/tasks/fix-parser/state",
            "tacho/tasks/fix-parser/review",
            "tacho/messages",
        ] {
            assert!(trace.track_by_path(path).is_some(), "missing track {path}");
        }
        for (name, _) in trace.tracks.values() {
            assert!(
                !(name.starts_with("agent:")
                    || name.starts_with("task:")
                    || name.starts_with("run:")),
                "track name {name:?} still carries a key prefix"
            );
        }
        // Every track is reachable from the single root, with no unnamed tracks.
        for &uuid in trace.tracks.keys() {
            let path = trace.path(uuid);
            assert!(path.starts_with("tacho"), "orphan track {path}");
            assert!(
                !path.split('/').any(str::is_empty),
                "unnamed track in {path}"
            );
        }
    }

    #[test]
    fn test_projected_facts_are_durations_on_their_own_tracks() {
        let trace = scenario_trace();
        let slices = trace.slices();
        let find = |track: &str, prefix: &str| {
            slices
                .iter()
                .find(|(t, n, _, _)| t == track && n.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix:?} slice on {track}: {slices:#?}"))
                .clone()
        };
        for (track, prefix) in [
            ("tacho/agents/worker-1/stalls", "Stall [LOOP]"),
            ("tacho/messages", "Message: orchestrator -> worker-1"),
            (
                "tacho/agents/worker-1/run 1/worktree",
                "WorktreeCommit: abc123",
            ),
            (
                "tacho/tasks/fix-parser/review",
                "ReviewFinding [MAJOR]: unchecked index",
            ),
            ("tacho/agents/worker-1/run 1/turns", "Turn 1"),
            (
                "tacho/agents/worker-1/run 1/turns",
                "Test: cargo test -p parser",
            ),
            (
                "tacho/agents/worker-1/run 1/procs/slot 1",
                "cargo [pid 201]",
            ),
            ("tacho/agents/worker-1/run 1", "Run"),
        ] {
            let (_, name, begin, end) = find(track, prefix);
            let end = end.unwrap_or_else(|| panic!("{name} on {track} never ends"));
            assert!(end > begin, "{name} on {track} must have a duration");
        }
        // The stall lasts until the worker recovers (its next activity), not a fixed blip.
        let (_, _, stall_begin, stall_end) = find("tacho/agents/worker-1/stalls", "Stall");
        let (_, _, msg_begin, _) = find("tacho/messages", "Message");
        assert!(stall_begin < msg_begin && stall_end.unwrap() > msg_begin);
        // No instant clutter on the projected tracks.
        for ev in &trace.events {
            let path = trace.path(ev.track);
            if path.ends_with("/stalls")
                || path.ends_with("/messages")
                || path.ends_with("/worktree")
            {
                assert_ne!(ev.ty, INSTANT, "instant on {path}");
            }
        }
    }

    #[test]
    fn test_flows_link_causes_to_effects() {
        let trace = scenario_trace();
        let label = |e: &TraceEvent| format!("{}::{}", trace.path(e.track), e.name);
        let mut begins: HashMap<u64, String> = HashMap::new();
        for e in &trace.events {
            for &f in &e.flow_ids {
                begins.insert(f, label(e));
            }
        }
        let links: Vec<(String, String)> = trace
            .events
            .iter()
            .flat_map(|e| {
                e.terminating_flow_ids.iter().filter_map(|f| {
                    begins
                        .get(f)
                        .map(|from| (from.clone(), format!("{}::{}", trace.path(e.track), e.name)))
                })
            })
            .collect();
        assert!(
            links
                .iter()
                .any(|(from, to)| from.starts_with("tacho/messages::Message")
                    && to.starts_with("tacho/agents/worker-1/run 1/turns::Turn 2")),
            "message must flow into the turn it triggered: {links:#?}"
        );
        // Every flow that ends somewhere starts somewhere.
        let all_flows: HashSet<u64> = begins.keys().copied().collect();
        for ev in &trace.events {
            for f in &ev.terminating_flow_ids {
                assert!(
                    all_flows.contains(f),
                    "flow {f} terminates without an origin"
                );
            }
        }
    }

    #[test]
    fn test_windowed_snapshot_has_no_unmatched_slice_ends_on_projected_tracks() {
        let temp = tempfile::tempdir().unwrap();
        let db = scenario_db(temp.path());
        // Cut the window in the middle of the stall and the message.
        for since in [T0 + 24_000 * MS, T0 + 26_000 * MS] {
            let mut out = Vec::new();
            write_snapshot(&db, Some(since), Some(T0 + 60_000 * MS), &mut out).unwrap();
            let trace = TraceModel::decode(&out);
            let mut depth: HashMap<u64, i64> = HashMap::new();
            for ev in &trace.events {
                let path = trace.path(ev.track);
                if !(path.ends_with("/stalls")
                    || path.ends_with("/messages")
                    || path.ends_with("/worktree"))
                {
                    continue;
                }
                let d = depth.entry(ev.track).or_default();
                match ev.ty {
                    SLICE_BEGIN => *d += 1,
                    SLICE_END => *d -= 1,
                    _ => {}
                }
                assert!(*d >= 0, "unmatched end on {path} with since={since}");
            }
        }
    }

    #[test]
    fn test_until_snapshot_has_a_boottime_mapping() {
        let temp = tempfile::tempdir().unwrap();
        let db = scenario_db(temp.path());
        let mut a = Vec::new();
        let mut b = Vec::new();
        write_snapshot(&db, None, Some(T0 + 60_000 * MS), &mut a).unwrap();
        write_snapshot(&db, None, Some(T0 + 60_000 * MS), &mut b).unwrap();
        assert_eq!(a, b, "bounded snapshots must be reproducible");
        let trace = pb::Trace::decode(&a[..]).unwrap();
        let clocks: Vec<u32> = trace
            .packet
            .iter()
            .filter_map(|p| match &p.data {
                Some(Data::ClockSnapshot(c)) => Some(
                    c.clocks
                        .iter()
                        .filter_map(|c| c.clock_id)
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            clocks.contains(&1) && clocks.contains(&6),
            "clocks {clocks:?}"
        );
    }
}

#[test]
fn test_atomic_write_failure_keeps_previous_file_and_leaves_no_temp() {
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("last.perfetto-trace");
    write_file_atomic(&dest, b"previous").unwrap();
    // A writer that dies half way (e.g. disk full or the daemon being killed mid-snapshot).
    let err = write_atomic_with(&dest, |f| {
        f.write_all(b"partial")?;
        Err(io::Error::other("simulated crash"))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "simulated crash");
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous");
    let leftovers: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "last.perfetto-trace")
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn test_snapshot_file_atomic_writes_a_decodable_trace() {
    let temp = tempfile::tempdir().unwrap();
    let db = structure::scenario_db(temp.path());
    let dest = temp.path().join("out.perfetto-trace");
    let stats = write_snapshot_file_atomic(&db, &dest).unwrap();
    let bytes = std::fs::read(&dest).unwrap();
    assert_eq!(bytes.len() as u64, stats.bytes);
    assert!(!structure::TraceModel::decode(&bytes).events.is_empty());
}

#[test]
fn test_snapshot_survives_corrupt_rows() {
    let temp = tempfile::tempdir().unwrap();
    let db = structure::scenario_db(temp.path());
    {
        let conn = Connection::open(&db).unwrap();
        let max: i64 = conn
            .query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))
            .unwrap();
        let ts: i64 = conn
            .query_row("SELECT MAX(ts_ns) FROM events", [], |r| r.get(0))
            .unwrap();
        // Garbage protobuf body, an unknown kind, and a hostile unicode track key.
        conn.execute(
            "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body)
             VALUES (?1, ?2, 9999, 'agent:💥/run:1/turns', NULL, '💥', x'ffffffff0102')",
            rusqlite::params![max + 1, ts + 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body)
             VALUES (?1, ?2, 0, '', 424242, NULL, x'')",
            rusqlite::params![max + 2, ts + 2],
        )
        .unwrap();
    }
    let mut a = Vec::new();
    let mut b = Vec::new();
    write_snapshot(&db, None, None, &mut a).unwrap();
    rebuild_from_log(&db, None, None, &mut b).unwrap();
    structure::TraceModel::decode(&a);
    structure::TraceModel::decode(&b);
}
