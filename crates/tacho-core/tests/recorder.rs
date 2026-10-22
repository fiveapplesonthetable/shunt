//! Integration and lifecycle tests for tacho_core::Recorder.

use std::collections::BTreeMap;
use tacho_core::{
    CustomOp, FileEditKind, Input, Kind, MessageDirection, Recorder, SessionKey, TaskStatus,
    ToolInput, WaitOn,
};
use tacho_proto::tacho;

#[test]
fn test_recorder_basic_lifecycle() {
    let mut rec = Recorder::new();
    let mut out = Vec::new();

    let session = SessionKey::new("claude", "sess-1234");
    rec.apply(
        Input::RunStart {
            provider: "claude".to_string(),
            session_id: "sess-1234".to_string(),
            agent: Some("agent-worker".to_string()),
            task: Some("Build feature X".to_string()),
            pid: Some(12345),
            cwd: Some("/workspace".to_string()),
            model: Some("claude-3-5-sonnet".to_string()),
            parent_session: None,
            source_ts_ns: Some(1_000_000),
        },
        1_000_000,
        &mut out,
    );

    assert!(!out.is_empty());
    assert!(out.iter().any(|e| e.kind == Kind::Run));
    assert!(out.iter().any(|e| e.kind == Kind::AgentState));

    // Start Turn
    rec.apply(
        Input::TurnStart {
            session: session.clone(),
            model: Some("claude-3-5-sonnet".to_string()),
            source_ts_ns: Some(2_000_000),
        },
        2_000_000,
        &mut out,
    );

    // Shell Tool Call
    rec.apply(
        Input::ToolBegin {
            session: session.clone(),
            tool_use_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo test -- -p core".to_string(),
                cwd: None,
            },
            source_ts_ns: Some(3_000_000),
        },
        3_000_000,
        &mut out,
    );

    assert!(out.iter().any(|e| e.kind == Kind::Command));

    // Shell Tool End
    rec.apply(
        Input::ToolEnd {
            session: session.clone(),
            tool_use_id: "call-1".to_string(),
            error: false,
            result_bytes: Some(1024),
            exit_code: Some(0),
            source_ts_ns: Some(4_000_000),
        },
        4_000_000,
        &mut out,
    );

    assert!(out.iter().any(|e| e.kind == Kind::CommandEnded));

    // Turn End
    rec.apply(
        Input::TurnEnd {
            session: session.clone(),
            input_tokens: Some(500),
            output_tokens: Some(100),
            cached_tokens: Some(50),
            text_bytes: Some(256),
            cost_micros: Some(120),
            source_ts_ns: Some(5_000_000),
        },
        5_000_000,
        &mut out,
    );

    assert!(out.iter().any(|e| e.kind == Kind::TurnEnded));
    assert!(out.iter().any(|e| e.counter_value == Some(500)));

    // Run End
    rec.apply(
        Input::RunEnd {
            session,
            exit_code: Some(0),
            reason: Some("Completed successfully".to_string()),
            source_ts_ns: Some(6_000_000),
        },
        6_000_000,
        &mut out,
    );

    assert!(out.iter().any(|e| e.kind == Kind::RunEnded));
    let run_ended = out.iter().find(|e| e.kind == Kind::RunEnded).unwrap();
    if let Some(tacho::event::Body::RunEnded(ref r)) = run_ended.body.body {
        assert_eq!(r.turns, Some(1));
        assert_eq!(r.tool_calls, Some(1));
    } else {
        panic!("Expected RunEnded body, got: {:?}", run_ended.body.body);
    }
}

#[test]
fn test_recorder_abandoned_slices_drained() {
    let mut rec = Recorder::new();
    let mut out = Vec::new();

    let session = SessionKey::new("claude", "sess-abandon");
    rec.apply(
        Input::RunStart {
            provider: "claude".to_string(),
            session_id: "sess-abandon".to_string(),
            agent: Some("agent-abandon".to_string()),
            task: None,
            pid: None,
            cwd: None,
            model: None,
            parent_session: None,
            source_ts_ns: Some(1_000_000),
        },
        1_000_000,
        &mut out,
    );

    rec.apply(
        Input::TurnStart {
            session: session.clone(),
            model: None,
            source_ts_ns: Some(2_000_000),
        },
        2_000_000,
        &mut out,
    );

    rec.apply(
        Input::ToolBegin {
            session: session.clone(),
            tool_use_id: "call-open".to_string(),
            tool_name: "bash".to_string(),
            input: ToolInput::Shell {
                argv: "sleep 100".to_string(),
                cwd: None,
            },
            source_ts_ns: Some(3_000_000),
        },
        3_000_000,
        &mut out,
    );

    // RunEnd without ToolEnd or TurnEnd
    rec.apply(
        Input::RunEnd {
            session,
            exit_code: Some(1),
            reason: Some("killed".to_string()),
            source_ts_ns: Some(4_000_000),
        },
        4_000_000,
        &mut out,
    );

    let abandoned_cmds: Vec<_> = out
        .iter()
        .filter(|e| e.kind == Kind::CommandEnded)
        .collect();
    assert_eq!(abandoned_cmds.len(), 1);
    if let Some(tacho::event::Body::CommandEnded(ref c)) = abandoned_cmds[0].body.body {
        assert_eq!(c.abandoned, Some(true));
    } else {
        panic!("Expected CommandEnded");
    }

    let abandoned_turns: Vec<_> = out.iter().filter(|e| e.kind == Kind::TurnEnded).collect();
    assert_eq!(abandoned_turns.len(), 1);
}

#[test]
fn test_recorder_deterministic_restore() {
    let mut rec1 = Recorder::new();
    let mut out1 = Vec::new();

    let session = SessionKey::new("gemini", "sess-restore");
    rec1.apply(
        Input::RunStart {
            provider: "gemini".to_string(),
            session_id: "sess-restore".to_string(),
            agent: Some("agent-gemini".to_string()),
            task: Some("Task restore".to_string()),
            pid: None,
            cwd: Some("/app".to_string()),
            model: Some("gemini-1.5-pro".to_string()),
            parent_session: None,
            source_ts_ns: Some(10_000),
        },
        10_000,
        &mut out1,
    );

    rec1.apply(
        Input::TurnStart {
            session: session.clone(),
            model: None,
            source_ts_ns: Some(20_000),
        },
        20_000,
        &mut out1,
    );

    rec1.apply(
        Input::ToolBegin {
            session: session.clone(),
            tool_use_id: "t1".to_string(),
            tool_name: "edit".to_string(),
            input: ToolInput::Shell {
                argv: "echo hi".to_string(),
                cwd: None,
            },
            source_ts_ns: Some(30_000),
        },
        30_000,
        &mut out1,
    );

    rec1.apply(
        Input::ToolEnd {
            session: session.clone(),
            tool_use_id: "t1".to_string(),
            error: false,
            result_bytes: Some(10),
            exit_code: Some(0),
            source_ts_ns: Some(40_000),
        },
        40_000,
        &mut out1,
    );

    rec1.apply(
        Input::TurnEnd {
            session: session.clone(),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cached_tokens: None,
            text_bytes: None,
            cost_micros: None,
            source_ts_ns: Some(50_000),
        },
        50_000,
        &mut out1,
    );

    // Restore state machine from recorded events
    let mut rec2 = Recorder::restore(out1.iter());

    // Both machines receive next input: TurnStart
    let next_input = Input::TurnStart {
        session,
        model: None,
        source_ts_ns: Some(60_000),
    };

    let mut out1_next = Vec::new();
    rec1.apply(next_input.clone(), 60_000, &mut out1_next);

    let mut out2_next = Vec::new();
    rec2.apply(next_input, 60_000, &mut out2_next);

    assert_eq!(out1_next.len(), out2_next.len());
    for (e1, e2) in out1_next.iter().zip(out2_next.iter()) {
        assert_eq!(e1.seq, e2.seq);
        assert_eq!(e1.kind, e2.kind);
        assert_eq!(e1.track_key, e2.track_key);
        assert_eq!(e1.ts_ns, e2.ts_ns);
    }
}

#[test]
fn test_repro_deterministic_restore() {
    let timed_inputs = vec![
        (
            Input::Wait {
                source_ts_ns: Some(1003000000),
                session: SessionKey::new("gemini", "sess-3"),
                target: Some("child".to_string()),
                on: Some(tacho_core::WaitOn::Subagent),
            },
            1003000000u64,
        ),
        (
            Input::ToolEnd {
                source_ts_ns: None,
                session: SessionKey::new("jetski", "sess-2"),
                tool_use_id: "call_3".to_string(),
                error: true,
                result_bytes: Some(128),
                exit_code: Some(1),
            },
            1006000000u64,
        ),
        (
            Input::Message {
                source_ts_ns: Some(1009000000),
                from: "agent_a".to_string(),
                to: "agent_b".to_string(),
                id: Some("msg_1".to_string()),
                bytes: Some(64),
                dir: Some(tacho_core::MessageDirection::Delivered),
            },
            1009000000u64,
        ),
        (
            Input::WaitEnd {
                source_ts_ns: None,
                session: SessionKey::new("jetski", "sess-2"),
                timed_out: false,
            },
            1007000000u64,
        ),
        (
            Input::Task {
                source_ts_ns: Some(1010000000),
                slug: "task_1".to_string(),
                brief_digest: Some("brief_hash".to_string()),
                brief_bytes: Some(256),
                paths: vec!["src/main.rs".to_string()],
            },
            1010000000u64,
        ),
        (
            Input::Wait {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                target: Some("child".to_string()),
                on: Some(tacho_core::WaitOn::Subagent),
            },
            701010000000u64,
        ),
        (
            Input::WaitEnd {
                source_ts_ns: Some(701013000000),
                session: SessionKey::new("gemini", "sess-3"),
                timed_out: false,
            },
            701013000000u64,
        ),
        (
            Input::TurnStart {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                model: Some("test-model".to_string()),
            },
            701016000000u64,
        ),
        (
            Input::Task {
                source_ts_ns: Some(701019000000),
                slug: "task_1".to_string(),
                brief_digest: Some("brief_hash".to_string()),
                brief_bytes: Some(256),
                paths: vec!["src/main.rs".to_string()],
            },
            701019000000u64,
        ),
        (
            Input::RunEnd {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                exit_code: Some(0),
                reason: Some("run ended".to_string()),
            },
            701022000000u64,
        ),
    ];

    for k in 1..timed_inputs.len() {
        let mut rec1 = Recorder::new();
        let mut events1 = Vec::new();
        for (inp, now) in &timed_inputs {
            rec1.apply(inp.clone(), *now, &mut events1);
        }

        let mut rec_prefix = Recorder::new();
        let mut events_prefix = Vec::new();
        for (inp, now) in &timed_inputs[..k] {
            rec_prefix.apply(inp.clone(), *now, &mut events_prefix);
        }

        let mut rec2 = Recorder::restore(events_prefix.iter());
        let mut events_restored = Vec::new();
        for (inp, now) in &timed_inputs[k..] {
            rec2.apply(inp.clone(), *now, &mut events_restored);
        }

        let continuous_suffix = &events1[events_prefix.len()..];
        assert_eq!(events_restored.len(), continuous_suffix.len(), "k={}", k);
        for (i, (r, c)) in events_restored
            .iter()
            .zip(continuous_suffix.iter())
            .enumerate()
        {
            assert_eq!(r.seq, c.seq, "k={}, i={}", k, i);
            assert_eq!(r.cause_seq, c.cause_seq, "k={}, i={}", k, i);
        }
    }
}

#[test]
fn test_repro_deterministic_restore_failure_2() {
    let timed_inputs: Vec<(Input, u64)> = vec![
        (
            Input::RunStart {
                source_ts_ns: Some(1003000000),
                provider: "claude".into(),
                session_id: "sess-1".into(),
                agent: Some("agent-claude".into()),
                task: Some("Prop task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("test-model".into()),
                parent_session: None,
            },
            1003000000,
        ),
        (
            Input::RunStart {
                source_ts_ns: None,
                provider: "claude".into(),
                session_id: "sess-1".into(),
                agent: Some("agent-claude".into()),
                task: Some("Prop task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("test-model".into()),
                parent_session: None,
            },
            1006000000,
        ),
        (
            Input::RunStart {
                source_ts_ns: Some(1009000000),
                provider: "claude".into(),
                session_id: "sess-1".into(),
                agent: Some("agent-claude".into()),
                task: Some("Prop task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("test-model".into()),
                parent_session: None,
            },
            1009000000,
        ),
        (
            Input::Message {
                source_ts_ns: None,
                from: "agent_a".into(),
                to: "agent_b".into(),
                id: Some("msg_1".into()),
                bytes: Some(64),
                dir: Some(MessageDirection::Sent),
            },
            1007000000,
        ),
        (
            Input::Custom {
                source_ts_ns: Some(1010000000),
                track: "a/b/c".into(),
                op: CustomOp::Begin,
                name: Some("custom_op".into()),
                id: None,
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            1010000000,
        ),
        (
            Input::Message {
                source_ts_ns: None,
                from: "agent_a".into(),
                to: "agent_b".into(),
                id: Some("msg_1".into()),
                bytes: Some(64),
                dir: Some(MessageDirection::Sent),
            },
            701010000000,
        ),
        (
            Input::TaskState {
                source_ts_ns: Some(701013000000),
                slug: "task_1".into(),
                state: TaskStatus::InProgress,
                because: Some("started working".into()),
            },
            701013000000,
        ),
        (
            Input::Compacted {
                source_ts_ns: None,
                session: SessionKey::new("jetski", "sess-2"),
                tokens_before: Some(5000),
                tokens_after: Some(1000),
                bytes_compacted: Some(4000),
            },
            701016000000,
        ),
        (
            Input::ToolEnd {
                source_ts_ns: Some(701019000000),
                session: SessionKey::new("jetski", "sess-2"),
                tool_use_id: "call_2".into(),
                error: false,
                result_bytes: Some(128),
                exit_code: Some(0),
            },
            701019000000,
        ),
        (
            Input::ToolBegin {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                tool_use_id: "call_3".into(),
                tool_name: "tool".into(),
                input: ToolInput::FileEdit {
                    path: "src/lib.rs".into(),
                    kind: Some(FileEditKind::Edit),
                    lines_added: Some(14),
                    lines_removed: Some(1),
                },
            },
            701022000000,
        ),
        (
            Input::Prompt {
                source_ts_ns: Some(701020000000),
                session: SessionKey::new("gemini", "sess-3"),
                text_len: 120,
                digest: Some("prompt_digest".into()),
            },
            701020000000,
        ),
        (
            Input::TurnEnd {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                input_tokens: Some(171),
                output_tokens: Some(289),
                cached_tokens: Some(10),
                text_bytes: Some(50),
                cost_micros: Some(25),
            },
            701023000000,
        ),
        (
            Input::RunStart {
                source_ts_ns: Some(701026000000),
                provider: "claude".into(),
                session_id: "sess-1".into(),
                agent: Some("agent-claude".into()),
                task: Some("Prop task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("test-model".into()),
                parent_session: None,
            },
            701026000000,
        ),
        (
            Input::Custom {
                source_ts_ns: None,
                track: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/slices".into(),
                op: CustomOp::Instant,
                name: Some("custom_op".into()),
                id: None,
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            701029000000,
        ),
        (
            Input::TurnStart {
                source_ts_ns: Some(701032000000),
                session: SessionKey::new("gemini", "sess-3"),
                model: Some("test-model".into()),
            },
            701032000000,
        ),
        (
            Input::Compacted {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                tokens_before: Some(5000),
                tokens_after: Some(1000),
                bytes_compacted: Some(4000),
            },
            701035000000,
        ),
        (
            Input::Compacted {
                source_ts_ns: Some(1401035000000),
                session: SessionKey::new("gemini", "sess-3"),
                tokens_before: Some(5000),
                tokens_after: Some(1000),
                bytes_compacted: Some(4000),
            },
            1401035000000,
        ),
        (
            Input::TaskState {
                source_ts_ns: None,
                slug: "task_1".into(),
                state: TaskStatus::InProgress,
                because: Some("started working".into()),
            },
            1401033000000,
        ),
        (
            Input::ToolBegin {
                source_ts_ns: Some(1401036000000),
                session: SessionKey::new("claude", "sess-1"),
                tool_use_id: "call_1".into(),
                tool_name: "tool".into(),
                input: ToolInput::Other {
                    input_digest: None,
                    input_bytes: Some(422),
                },
            },
            1401036000000,
        ),
        (
            Input::Task {
                source_ts_ns: None,
                slug: "task_1".into(),
                brief_digest: Some("brief_hash".into()),
                brief_bytes: Some(256),
                paths: vec!["src/main.rs".into()],
            },
            1401039000000,
        ),
        (
            Input::Wait {
                source_ts_ns: Some(1401042000000),
                session: SessionKey::new("gemini", "sess-3"),
                target: Some("child".into()),
                on: Some(WaitOn::Subagent),
            },
            1401042000000,
        ),
        (
            Input::ProviderError {
                source_ts_ns: None,
                provider: "claude".into(),
                error_code: Some("rate_limit".into()),
                message: Some("rate limit exceeded".into()),
                rate_limit: true,
                retry_after_ms: Some(1000),
            },
            1401045000000,
        ),
        (
            Input::Wait {
                source_ts_ns: Some(1401048000000),
                session: SessionKey::new("gemini", "sess-3"),
                target: Some("child".into()),
                on: Some(WaitOn::Subagent),
            },
            1401048000000,
        ),
        (
            Input::Wait {
                source_ts_ns: None,
                session: SessionKey::new("jetski", "sess-2"),
                target: Some("child".into()),
                on: Some(WaitOn::Subagent),
            },
            1401051000000,
        ),
        (
            Input::Wait {
                source_ts_ns: Some(1401049000000),
                session: SessionKey::new("gemini", "sess-3"),
                target: Some("child".into()),
                on: Some(WaitOn::Subagent),
            },
            1401049000000,
        ),
        (
            Input::RunEnd {
                source_ts_ns: None,
                session: SessionKey::new("jetski", "sess-2"),
                exit_code: Some(0),
                reason: Some("run ended".into()),
            },
            1401052000000,
        ),
        (
            Input::TaskState {
                source_ts_ns: Some(1401055000000),
                slug: "task_1".into(),
                state: TaskStatus::InProgress,
                because: Some("started working".into()),
            },
            1401055000000,
        ),
        (
            Input::ToolEnd {
                source_ts_ns: None,
                session: SessionKey::new("gemini", "sess-3"),
                tool_use_id: "call_3".into(),
                error: false,
                result_bytes: Some(128),
                exit_code: Some(0),
            },
            2101055000000,
        ),
    ];

    let k = (timed_inputs.len() as f64 * 0.7361814522677251) as usize; // k = 20

    let mut rec1 = Recorder::new();
    let mut events1 = Vec::new();
    for (inp, now) in &timed_inputs {
        rec1.apply(inp.clone(), *now, &mut events1);
    }

    let mut rec_prefix = Recorder::new();
    let mut events_prefix = Vec::new();
    for (inp, now) in &timed_inputs[..k] {
        rec_prefix.apply(inp.clone(), *now, &mut events_prefix);
    }

    let mut rec2 = Recorder::restore(events_prefix.iter());
    let mut events_restored = Vec::new();
    for (inp, now) in &timed_inputs[k..] {
        rec2.apply(inp.clone(), *now, &mut events_restored);
    }

    let continuous_suffix = &events1[events_prefix.len()..];
    println!("events_prefix.len() = {}", events_prefix.len());
    println!("events1.len() = {}", events1.len());
    println!("continuous_suffix.len() = {}", continuous_suffix.len());
    println!("events_restored.len() = {}", events_restored.len());

    println!("=== CONTINUOUS SUFFIX ({}) ===", continuous_suffix.len());
    for (i, e) in continuous_suffix.iter().enumerate() {
        println!(
            "  cont[{}] seq={} kind={:?} track={} cause={:?}",
            i, e.seq, e.kind, e.track_key, e.cause_seq
        );
    }
    println!("=== RESTORED ({}) ===", events_restored.len());
    for (i, e) in events_restored.iter().enumerate() {
        println!(
            "  rest[{}] seq={} kind={:?} track={} cause={:?}",
            i, e.seq, e.kind, e.track_key, e.cause_seq
        );
    }

    assert_eq!(events_restored.len(), continuous_suffix.len());
    for (i, (r, c)) in events_restored
        .iter()
        .zip(continuous_suffix.iter())
        .enumerate()
    {
        assert_eq!(
            r.seq, c.seq,
            "index {}: r.kind={:?} c.kind={:?}",
            i, r.kind, c.kind
        );
        assert_eq!(r.kind, c.kind, "index {}", i);
        assert_eq!(r.cause_seq, c.cause_seq, "index {}", i);
    }
}

#[test]
fn test_counter_sample_consecutive_deduplication() {
    let mut rec = Recorder::new();
    let mut events = Vec::new();

    // First sample: emits TrackCreated + Counter Event
    rec.apply(
        Input::CounterSample {
            track_key: "machine/cpu_busy".to_string(),
            value: 42,
            unit: Some("percent".to_string()),
            source_ts_ns: Some(1_000_000),
        },
        1_000_000,
        &mut events,
    );
    let counter_events: Vec<_> = events
        .iter()
        .filter(|e| e.counter_value.is_some())
        .collect();
    assert_eq!(counter_events.len(), 1);
    assert_eq!(counter_events[0].counter_value, Some(42));

    // Second sample with identical value: skipped, emits nothing
    rec.apply(
        Input::CounterSample {
            track_key: "machine/cpu_busy".to_string(),
            value: 42,
            unit: Some("percent".to_string()),
            source_ts_ns: Some(2_000_000),
        },
        2_000_000,
        &mut events,
    );
    let counter_events: Vec<_> = events
        .iter()
        .filter(|e| e.counter_value.is_some())
        .collect();
    assert_eq!(
        counter_events.len(),
        1,
        "consecutive identical value must not emit"
    );

    // Third sample with changed value: emits 2nd counter Event
    rec.apply(
        Input::CounterSample {
            track_key: "machine/cpu_busy".to_string(),
            value: 55,
            unit: Some("percent".to_string()),
            source_ts_ns: Some(3_000_000),
        },
        3_000_000,
        &mut events,
    );
    let counter_events: Vec<_> = events
        .iter()
        .filter(|e| e.counter_value.is_some())
        .collect();
    assert_eq!(counter_events.len(), 2, "changed value must emit 2nd event");
    assert_eq!(counter_events[1].counter_value, Some(55));

    // Also test restore seeds last_counter_value
    let mut rec_restored = Recorder::restore(events.iter());
    let mut restored_events = Vec::new();

    // Identical value (55) to restored recorder: skipped
    rec_restored.apply(
        Input::CounterSample {
            track_key: "machine/cpu_busy".to_string(),
            value: 55,
            unit: Some("percent".to_string()),
            source_ts_ns: Some(4_000_000),
        },
        4_000_000,
        &mut restored_events,
    );
    assert_eq!(
        restored_events.len(),
        0,
        "restored recorder must skip identical counter value"
    );

    // Changed value (60) to restored recorder: emits
    rec_restored.apply(
        Input::CounterSample {
            track_key: "machine/cpu_busy".to_string(),
            value: 60,
            unit: Some("percent".to_string()),
            source_ts_ns: Some(5_000_000),
        },
        5_000_000,
        &mut restored_events,
    );
    assert_eq!(
        restored_events.len(),
        1,
        "restored recorder must emit changed counter value"
    );
    assert_eq!(restored_events[0].counter_value, Some(60));
}

#[test]
fn test_bench_restore_1m_rows() {
    use std::time::Instant;
    use tacho_core::Event;

    let num_events = 1_000_000;
    let mut events = Vec::with_capacity(num_events);

    events.push(Event {
        seq: 1,
        ts_ns: 1_000_000,
        kind: Kind::Run,
        track_key: "agent:worker-1/run:1".to_string(),
        cause_seq: None,
        agent: Some("worker-1".to_string()),
        custom_name: None,
        flow_ids: Vec::new(),
        terminating_flow_ids: Vec::new(),
        counter_value: None,
        body: tacho::Event {
            body: Some(tacho::event::Body::Run(tacho::Run {
                provider: Some("claude".to_string()),
                session_id: Some("sess-1".to_string()),
                task: Some("bench".to_string()),
                pid: Some(1234),
                cwd: Some("/workspace".to_string()),
                model: Some("claude-3-5".to_string()),
                parent_run_seq: None,
                resume: None,
            })),
        },
    });

    events.push(Event {
        seq: 2,
        ts_ns: 2_000_000,
        kind: Kind::Turn,
        track_key: "agent:worker-1/run:1/turns".to_string(),
        cause_seq: None,
        agent: Some("worker-1".to_string()),
        custom_name: None,
        flow_ids: Vec::new(),
        terminating_flow_ids: Vec::new(),
        counter_value: None,
        body: tacho::Event {
            body: Some(tacho::event::Body::Turn(tacho::Turn {
                turn_number: Some(1),
                model: Some("claude-3-5".to_string()),
            })),
        },
    });

    for i in 3..=num_events as u64 {
        let ts_ns = 2_000_000 + i * 1_000;
        let (kind, track_key, cause_seq, counter_val) = match i % 5 {
            0 => (
                Kind::Command,
                "agent:worker-1/run:1/lane:1".to_string(),
                None,
                None,
            ),
            1 => (
                Kind::CommandEnded,
                "agent:worker-1/run:1/lane:1".to_string(),
                Some(i - 1),
                None,
            ),
            2 => (
                Kind::AgentState,
                "agent:worker-1/state".to_string(),
                None,
                None,
            ),
            3 => (
                Kind::Custom,
                "machine/cpu_busy".to_string(),
                None,
                Some((i % 100) as i64),
            ),
            _ => (
                Kind::FileEdited,
                "agent:worker-1/run:1/lane:1".to_string(),
                Some(i - 1),
                None,
            ),
        };
        events.push(Event {
            seq: i,
            ts_ns,
            kind,
            track_key,
            cause_seq,
            agent: Some("worker-1".to_string()),
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: counter_val,
            body: tacho::Event { body: None },
        });
    }

    let start = Instant::now();
    let rec = Recorder::restore(events.iter());
    let elapsed = start.elapsed();
    let rate = num_events as f64 / elapsed.as_secs_f64();
    println!("============================================================");
    println!("Benchmark: Recorder::restore 1,000,000 events");
    println!("Time:      {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    println!("Rate:      {:.0} events/s", rate);
    println!("============================================================");

    assert!(rec.live_state().agents.contains_key("worker-1"));
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "1M restore must be fast (< 5s), was {:?}",
        elapsed
    );
}
