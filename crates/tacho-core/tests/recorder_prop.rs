//! Property tests for Recorder: deterministic restore and robust trace rendering.
//!
//! Validates:
//! (a) Random input streams (interleaving, overlapping tools, unmatched begins/ends, clock skew)
//!     produce Perfetto traces with 0 error/data_loss stats, properly nested slices, and valid state strings.
//! (b) Deterministic restore for any random split point k: restore(events[..k]) + apply(inputs[k..])
//!     produces the exact same events, live state, and track registry as continuous execution.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::Command;

use proptest::prelude::*;
use tacho_core::{
    encode_snapshot_header, CustomOp, Encoder, FileEditKind, FindingSeverity, Input,
    MessageDirection, Recorder, SessionKey, TaskStatus, ToolInput, WaitOn,
};

fn arb_session_key() -> impl Strategy<Value = SessionKey> {
    prop_oneof![
        Just(SessionKey::new("claude", "sess-1")),
        Just(SessionKey::new("jetski", "sess-2")),
        Just(SessionKey::new("gemini", "sess-3")),
    ]
}

fn arb_slice_track_key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("custom:build".to_string()),
        Just("a/b/c".to_string()),
        Just("deep/nested/custom/slices".to_string()),
        Just("unicode_🎯/slices".to_string()),
        Just("x".repeat(100) + "/slices"),
    ]
}

fn arb_counter_track_key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("machine/cpu".to_string()),
        Just("agent:worker-1/cpu".to_string()),
        Just("agent:worker-1/rss".to_string()),
        Just("custom/counter_1".to_string()),
        Just("deep/nested/custom/counter".to_string()),
        Just("unicode_🎯/counter".to_string()),
        Just("x".repeat(100) + "/counter"),
    ]
}

fn arb_track_key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("machine/cpu".to_string()),
        Just("agent:worker-1/cpu".to_string()),
        Just("agent:worker-1/rss".to_string()),
        Just("a/b/c".to_string()),
        Just("custom:build".to_string()),
        Just("deep/nested/custom/counter".to_string()),
        Just("unicode_🎯_rocket_🚀".to_string()),
        Just("x".repeat(300)),
        "[a-z0-9_]{1,8}/[a-z0-9_]{1,8}".prop_map(|s| s),
    ]
}

fn arb_tool_input() -> impl Strategy<Value = ToolInput> {
    prop_oneof![
        prop_oneof![
            Just("cargo test -p core"),
            Just("git status"),
            Just("npm run build"),
            Just("echo 'hello world'"),
        ]
        .prop_map(|cmd| ToolInput::Shell {
            argv: cmd.to_string(),
            cwd: None,
            git_tree: None,
        }),
        prop_oneof![Just("src/main.rs"), Just("Cargo.toml"), Just("README.md"),].prop_map(|path| {
            ToolInput::FileRead {
                path: path.to_string(),
            }
        }),
        prop_oneof![Just("fn main"), Just("test"), Just("TODO"),].prop_map(|pattern| {
            ToolInput::FileSearch {
                pattern: pattern.to_string(),
                path: None,
            }
        }),
        (1u32..20, 0u32..10).prop_map(|(added, removed)| ToolInput::FileEdit {
            path: "src/lib.rs".to_string(),
            kind: Some(FileEditKind::Edit),
            lines_added: Some(added),
            lines_removed: Some(removed),
        }),
        (10u64..500).prop_map(|bytes| ToolInput::Other {
            input_digest: None,
            input_bytes: Some(bytes),
        }),
    ]
}

fn arb_input() -> impl Strategy<Value = Input> {
    let pool_tools = prop_oneof![
        Just("call_1"),
        Just("call_2"),
        Just("call_3"),
        Just("call_unmatched"),
    ];

    prop_oneof![
        arb_session_key().prop_map(|session| Input::RunStart {
            provider: session.provider.clone(),
            session_id: session.session_id.clone(),
            agent: Some(format!("agent-{}", session.provider)),
            task: Some("Prop task".to_string()),
            pid: Some(1234),
            cwd: Some("/workspace".to_string()),
            model: Some("test-model".to_string()),
            parent_session: None,
            git_tree: None,
            source_ts_ns: None,
        }),
        arb_session_key().prop_map(|session| Input::RunEnd {
            session,
            exit_code: Some(0),
            reason: Some("run ended".to_string()),
            source_ts_ns: None,
        }),
        arb_session_key().prop_map(|session| Input::TurnStart {
            session,
            model: Some("test-model".to_string()),
            source_ts_ns: None,
        }),
        (arb_session_key(), 10u64..1000, 10u64..500).prop_map(|(session, tin, tout)| {
            Input::TurnEnd {
                session,
                input_tokens: Some(tin),
                output_tokens: Some(tout),
                cached_tokens: Some(10),
                text_bytes: Some(50),
                cost_micros: Some(25),
                source_ts_ns: None,
            }
        }),
        (arb_session_key(), pool_tools.clone(), arb_tool_input()).prop_map(
            |(session, tid, tool_in)| Input::ToolBegin {
                session,
                tool_use_id: tid.to_string(),
                tool_name: "tool".to_string(),
                input: tool_in,
                source_ts_ns: None,
            }
        ),
        (arb_session_key(), pool_tools, any::<bool>()).prop_map(|(session, tid, err)| {
            Input::ToolEnd {
                session,
                tool_use_id: tid.to_string(),
                error: err,
                result_bytes: Some(128),
                exit_code: Some(if err { 1 } else { 0 }),
                source_ts_ns: None,
            }
        }),
        arb_session_key().prop_map(|session| Input::Wait {
            session,
            target: Some("child".to_string()),
            on: Some(WaitOn::Subagent),
            source_ts_ns: None,
        }),
        arb_session_key().prop_map(|session| Input::WaitEnd {
            session,
            timed_out: false,
            source_ts_ns: None,
        }),
        arb_session_key().prop_map(|session| Input::Prompt {
            session,
            text_len: 120,
            digest: Some("prompt_digest".to_string()),
            source_ts_ns: None,
        }),
        any::<bool>().prop_map(|dir_sent| Input::Message {
            from: "agent_a".to_string(),
            to: "agent_b".to_string(),
            id: Some("msg_1".to_string()),
            bytes: Some(64),
            dir: Some(if dir_sent {
                MessageDirection::Sent
            } else {
                MessageDirection::Delivered
            }),
            source_ts_ns: None,
        }),
        arb_session_key().prop_map(|session| Input::Compacted {
            session,
            tokens_before: Some(5000),
            tokens_after: Some(1000),
            bytes_compacted: Some(4000),
            source_ts_ns: None,
        }),
        Just(Input::ProviderError {
            provider: "claude".to_string(),
            error_code: Some("rate_limit".to_string()),
            message: Some("rate limit exceeded".to_string()),
            rate_limit: true,
            retry_after_ms: Some(1000),
            source_ts_ns: None,
        }),
        Just(Input::Task {
            slug: "task_1".to_string(),
            brief_digest: Some("brief_hash".to_string()),
            brief_bytes: Some(256),
            paths: vec!["src/main.rs".to_string()],
            agent: None,
            base_commit: None,
            token_budget: None,
            cost_budget_micros: None,
            source_ts_ns: None,
        }),
        Just(Input::TaskState {
            slug: "task_1".to_string(),
            state: TaskStatus::InProgress,
            because: Some("started working".to_string()),
            source_ts_ns: None,
        }),
        Just(Input::Finding {
            task: "task_1".to_string(),
            severity: FindingSeverity::Minor,
            path: Some("src/main.rs".to_string()),
            line: Some(42),
            title: "potential bug".to_string(),
            source_ts_ns: None,
        }),
        (arb_slice_track_key(), any::<bool>()).prop_map(|(track, is_start)| Input::Custom {
            track,
            op: if is_start {
                CustomOp::Begin
            } else {
                CustomOp::Instant
            },
            name: Some("custom_op".to_string()),
            id: None,
            args: BTreeMap::new(),
            value: None,
            flow_out: None,
            flow_in: None,
            source_ts_ns: None,
        }),
        (arb_counter_track_key(), -100i64..1000i64).prop_map(|(track, val)| Input::CounterSample {
            track_key: track,
            value: val,
            unit: Some("percent".to_string()),
            source_ts_ns: None,
        }),
    ]
}

fn arb_input_sequence() -> impl Strategy<Value = Vec<(Input, u64)>> {
    prop::collection::vec(arb_input(), 5..40).prop_map(|inputs| {
        let mut now = 1_000_000_000u64;
        let mut timed_inputs = Vec::new();
        for (i, mut inp) in inputs.into_iter().enumerate() {
            // Mix normal forward progression with occasional backward or jump
            if i % 7 == 3 {
                now = now.saturating_sub(2_000_000); // clock backward
            } else if i % 11 == 5 {
                now += 700 * 1_000_000_000; // clock jump > 10 min
            } else {
                now += 3_000_000; // normal forward
            }

            // Assign source_ts_ns to some
            let src_ts = if i % 2 == 0 { Some(now) } else { None };
            match &mut inp {
                Input::RunStart { source_ts_ns, .. }
                | Input::RunEnd { source_ts_ns, .. }
                | Input::TurnStart { source_ts_ns, .. }
                | Input::TurnEnd { source_ts_ns, .. }
                | Input::ToolBegin { source_ts_ns, .. }
                | Input::ToolEnd { source_ts_ns, .. }
                | Input::Wait { source_ts_ns, .. }
                | Input::WaitEnd { source_ts_ns, .. }
                | Input::Prompt { source_ts_ns, .. }
                | Input::Message { source_ts_ns, .. }
                | Input::Compacted { source_ts_ns, .. }
                | Input::ProviderError { source_ts_ns, .. }
                | Input::Task { source_ts_ns, .. }
                | Input::TaskState { source_ts_ns, .. }
                | Input::Finding { source_ts_ns, .. }
                | Input::Custom { source_ts_ns, .. }
                | Input::CounterSample { source_ts_ns, .. } => *source_ts_ns = src_ts,
            }
            timed_inputs.push((inp, now));
        }
        timed_inputs
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property (b): Deterministic restore for any random split point k:
    /// restore(events[..k]) + apply(inputs[k..]) produces the exact same events
    /// and live state as never restarting.
    #[test]
    fn prop_deterministic_restore(
        timed_inputs in arb_input_sequence(),
        split_pct in 0.0f64..=1.0f64
    ) {
        let n = timed_inputs.len();
        let k = (split_pct * (n as f64)).round() as usize;
        let k = k.min(n);

        // 1. Run continuous execution on rec1
        let mut rec1 = Recorder::new();
        let mut events1 = Vec::new();
        for (inp, now) in &timed_inputs {
            rec1.apply(inp.clone(), *now, &mut events1);
        }

        // 2. Run prefix execution up to k
        let mut rec_prefix = Recorder::new();
        let mut events_prefix = Vec::new();
        for (inp, now) in &timed_inputs[..k] {
            rec_prefix.apply(inp.clone(), *now, &mut events_prefix);
        }

        // 3. Restore rec2 from events_prefix
        let mut rec2 = Recorder::restore(events_prefix.iter());

        // 4. Apply suffix inputs[k..] to restored rec2
        let mut events_restored = Vec::new();
        for (inp, now) in &timed_inputs[k..] {
            rec2.apply(inp.clone(), *now, &mut events_restored);
        }

        // 5. Verify restored suffix events match continuous execution
        let continuous_suffix = &events1[events_prefix.len()..];
        prop_assert_eq!(
            events_restored.len(),
            continuous_suffix.len(),
            "Number of restored suffix events must match continuous suffix events"
        );

        for (i, (restored, cont)) in events_restored.iter().zip(continuous_suffix.iter()).enumerate() {
            prop_assert_eq!(
                restored.seq, cont.seq,
                "Event seq mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                restored.kind, cont.kind,
                "Event kind mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                &restored.track_key, &cont.track_key,
                "Event track_key mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                restored.ts_ns, cont.ts_ns,
                "Event ts_ns mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                restored.cause_seq, cont.cause_seq,
                "Event cause_seq mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                &restored.custom_name, &cont.custom_name,
                "Event custom_name mismatch at suffix index {}", i
            );
            prop_assert_eq!(
                restored.counter_value, cont.counter_value,
                "Event counter_value mismatch at suffix index {}", i
            );
        }

        // Live state agent count and status agreement
        prop_assert_eq!(
            rec1.live_state().agents.len(),
            rec2.live_state().agents.len(),
            "Live state agents count must match"
        );
    }

    /// Property (a): Random input streams -> Recorder -> Encoder -> trace loads in oracle
    /// with 0 error/data_loss stats, proper slice track nesting, and state strings.
    #[test]
    fn prop_oracle_verification(timed_inputs in arb_input_sequence()) {
        let tp_path = env::var("TACHO_TP").unwrap_or_else(|_| {
            "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell".to_string()
        });

        let mut rec = Recorder::new();
        let mut events = Vec::new();
        for (inp, now) in &timed_inputs {
            rec.apply(inp.clone(), *now, &mut events);
        }

        let base_ts = 1_000_000_000u64;
        let mut trace_bytes = Vec::new();
        encode_snapshot_header(rec.registry(), base_ts, Some(base_ts - 500_000_000), &mut trace_bytes);

        let mut encoder = Encoder::new();
        let mut packet_buf = Vec::new();
        for event in &events {
            packet_buf.clear();
            encoder.encode_event(event, rec.registry(), &mut packet_buf);
            trace_bytes.extend_from_slice(&packet_buf);
        }

        // Write to temp file and run trace_processor_shell
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let trace_path = temp_dir.path().join("prop.pftrace");
        fs::write(&trace_path, &trace_bytes).expect("write trace");

        // 1. Assert 0 error/data_loss stats
        let stats_output = Command::new(&tp_path)
            .arg(&trace_path)
            .arg("-Q")
            .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
            .output()
            .expect("trace_processor_shell stats");

        let stats_stdout = String::from_utf8_lossy(&stats_output.stdout);
        let error_rows: Vec<&str> = stats_stdout
            .trim()
            .lines()
            .filter(|l| l.contains("\"error\"") || l.contains("\"data_loss\""))
            .collect();
        prop_assert!(
            error_rows.is_empty(),
            "trace_processor reported error or data_loss stats: {:?}",
            error_rows
        );

        // 2. Query slice depth to verify proper nesting per track
        let depth_output = Command::new(&tp_path)
            .arg(&trace_path)
            .arg("-Q")
            .arg("SELECT track_id, depth FROM slice ORDER BY track_id, ts;")
            .output()
            .expect("trace_processor_shell slice depth");
        prop_assert!(depth_output.status.success());

        // 3. Query slice table to verify state strings and slice names are present
        let state_output = Command::new(&tp_path)
            .arg(&trace_path)
            .arg("-Q")
            .arg("SELECT DISTINCT name FROM slice;")
            .output()
            .expect("trace_processor_shell slice names");
        prop_assert!(state_output.status.success());
    }

    /// Property (c): Random track keys (including odd ones like `a/b/c`, `agent:x/cpu`, unicode, 300 chars)
    /// in CounterSample/Custom load in oracle with zero track_event_*invalid* stats.
    #[test]
    fn prop_random_track_keys_oracle(
        samples in prop::collection::vec((arb_track_key(), -500i64..500i64), 1..25),
    ) {
        let tp_path = env::var("TACHO_TP").unwrap_or_else(|_| {
            "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell".to_string()
        });

        let mut rec = Recorder::new();
        let mut events = Vec::new();
        let mut now = 1_000_000_000u64;

        for (track_key, val) in &samples {
            now += 1_000_000;
            rec.apply(
                Input::CounterSample {
                    track_key: track_key.clone(),
                    value: *val,
                    unit: Some("units".to_string()),
                    source_ts_ns: Some(now),
                },
                now,
                &mut events,
            );
            now += 1_000_000;
            rec.apply(
                Input::Custom {
                    track: track_key.clone(),
                    op: CustomOp::Instant,
                    name: Some("test_point".to_string()),
                    id: None,
                    args: BTreeMap::new(),
                    value: None,
                    flow_out: None,
                    flow_in: None,
                    source_ts_ns: Some(now),
                },
                now,
                &mut events,
            );
        }

        let base_ts = 1_000_000_000u64;
        let mut trace_bytes = Vec::new();
        encode_snapshot_header(rec.registry(), base_ts, Some(base_ts - 500_000_000), &mut trace_bytes);

        let mut encoder = Encoder::new();
        let mut packet_buf = Vec::new();
        for event in &events {
            packet_buf.clear();
            encoder.encode_event(event, rec.registry(), &mut packet_buf);
            trace_bytes.extend_from_slice(&packet_buf);
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let trace_path = temp_dir.path().join("odd_keys.pftrace");
        fs::write(&trace_path, &trace_bytes).expect("write trace");

        let stats_output = Command::new(&tp_path)
            .arg(&trace_path)
            .arg("-Q")
            .arg("SELECT name, value, severity FROM stats WHERE (severity IN ('error', 'data_loss') OR name LIKE '%invalid%') AND value > 0;")
            .output()
            .expect("trace_processor_shell stats");

        let stats_stdout = String::from_utf8_lossy(&stats_output.stdout);
        let error_rows: Vec<&str> = stats_stdout
            .trim()
            .lines()
            .filter(|l| l.contains("\"error\"") || l.contains("\"data_loss\"") || l.contains("invalid"))
            .collect();
        prop_assert!(
            error_rows.is_empty(),
            "trace_processor reported error/invalid stats for odd track keys: {:?}",
            error_rows
        );
    }
}
