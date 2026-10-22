//! Oracle integration test running trace_processor_shell against synthetic traces.

use std::collections::HashSet;
use std::fs;
use std::process::Command;
use tacho_core::{encode_snapshot_header, Encoder, Event, Kind, TrackKind, TrackRegistry};
use tacho_proto::tacho;

/// Oracle test: builds a trace with every kind having all fields set,
/// runs trace_processor_shell, and verifies stats, tracks, args, and packet sizes.
#[test]
fn every_kind_renders() {
    let tp_path = tacho_core::find_trace_processor()
        .expect("trace_processor_shell not found; run scripts/oracle.sh or set TACHO_TP");

    let mut registry = TrackRegistry::new();
    registry.register("tacho", None, "tacho", Some("Root"), TrackKind::Group, None);
    registry.register(
        "daemon",
        Some("tacho"),
        "daemon",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "agents",
        Some("tacho"),
        "agents",
        None,
        TrackKind::Group,
        None,
    );
    registry.register(
        "agent:1",
        Some("agents"),
        "agent:1",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "agent:1/state",
        Some("agent:1"),
        "state",
        None,
        TrackKind::State,
        None,
    );
    registry.register(
        "agent:1/run:1",
        Some("agent:1"),
        "run:1",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "agent:1/run:1/turns",
        Some("agent:1/run:1"),
        "turns",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "agent:1/run:1/waits",
        Some("agent:1/run:1"),
        "waits",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "tasks",
        Some("tacho"),
        "tasks",
        None,
        TrackKind::Group,
        None,
    );
    registry.register(
        "task:test",
        Some("tasks"),
        "task:test",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "task:test/state",
        Some("task:test"),
        "state",
        None,
        TrackKind::State,
        None,
    );
    registry.register(
        "task:test/review",
        Some("task:test"),
        "review",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "providers",
        Some("tacho"),
        "providers",
        None,
        TrackKind::Group,
        None,
    );
    registry.register(
        "provider:anthropic",
        Some("providers"),
        "anthropic",
        None,
        TrackKind::Slices,
        None,
    );
    registry.register(
        "custom:test",
        None,
        "custom:test",
        None,
        TrackKind::Slices,
        None,
    );

    // Build trace packets
    let mut trace_bytes = Vec::new();
    let base_ts = 1_000_000_000u64;

    // 1. Snapshot header
    encode_snapshot_header(
        &registry,
        base_ts,
        Some(base_ts - 500_000_000),
        &mut trace_bytes,
    );

    // 2. Events: one packet per Kind with every field set
    let mut encoder = Encoder::new();
    let events = create_all_events(base_ts);

    assert_eq!(
        events.len(),
        Kind::ALL.len(),
        "Must have exactly one event per kind"
    );

    let mut single_packet_buf = Vec::new();
    for event in &events {
        single_packet_buf.clear();
        encoder.encode_event(event, &registry, &mut single_packet_buf);

        // Assert: every packet is < 2 KiB (2048 bytes)
        assert!(
            single_packet_buf.len() < 2048,
            "Packet for kind {:?} exceeded 2 KiB: {} bytes",
            event.kind,
            single_packet_buf.len()
        );

        trace_bytes.extend_from_slice(&single_packet_buf);
    }

    // Write trace to temporary file
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let trace_file = temp_dir.path().join("every_kind.pftrace");
    fs::write(&trace_file, &trace_bytes).expect("failed to write trace file");

    // Run trace_processor_shell and query stats
    let output = Command::new(&tp_path)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output()
        .expect("failed to execute trace_processor_shell");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "trace_processor_shell failed: {}\n{}",
        stdout,
        stderr
    );

    // Verify 0 error/data_loss stats
    let stats_lines: Vec<&str> = stdout.trim().lines().filter(|l| !l.is_empty()).collect();
    // Headers or Trace loaded lines might be present, but data rows must be 0
    let data_rows: Vec<&str> = stats_lines
        .iter()
        .filter(|line| line.contains("\"error\"") || line.contains("\"data_loss\""))
        .copied()
        .collect();
    assert!(
        data_rows.is_empty(),
        "trace_processor reported stats errors or data_loss: {:?}",
        data_rows
    );

    // Query args table to verify expected args keys are present
    let args_output = Command::new(&tp_path)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT DISTINCT flat_key FROM args;")
        .output()
        .expect("failed to query args");

    let args_stdout = String::from_utf8_lossy(&args_output.stdout);
    let flat_keys: HashSet<String> = args_stdout
        .lines()
        .map(|l| l.trim().trim_matches('"').to_string())
        .collect();

    // Verify critical extension flat keys are decoded
    let expected_keys = [
        "seq",
        "command.argv",
        "command.program",
        "command.class",
        "turn_ended.output_tokens",
        "tool_call.name",
        "file_edited.path",
        "stall.kind",
        "agent_state.state",
        "task_state.state",
        "review_finding.title",
        "snapshot_ended.duration_us",
    ];

    for key in &expected_keys {
        assert!(
            flat_keys.contains(*key),
            "Expected flat_key {:?} not found in args table. Found keys: {:?}",
            key,
            flat_keys
        );
    }
}

/// Tests that `Kind::ALL`, the `Event.body` oneof, and the extension field list agree.
#[test]
fn test_kind_agreement() {
    assert_eq!(Kind::ALL.len(), 33, "Kind::ALL must have 33 variants");

    for (idx, &kind) in Kind::ALL.iter().enumerate() {
        assert_eq!(
            kind as usize, idx,
            "Kind {:?} discriminant must match canonical index {}",
            kind, idx
        );
        let ext_num = kind.extension_field_number();
        assert_eq!(
            ext_num,
            4003 + idx as u32,
            "Extension field number for {:?} must be {}",
            kind,
            4003 + idx as u32
        );
    }
}

/// Helper function building all 33 event variants with all fields populated.
fn create_all_events(base_ts: u64) -> Vec<Event> {
    let mut events = Vec::with_capacity(33);
    let mut seq = 100u64;
    let mut ts = base_ts;

    // Helper macro to generate sequential timestamps
    macro_rules! next_ts {
        () => {{
            ts += 10_000_000;
            ts
        }};
    }

    // 1. Run (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Run,
        "agent:1/run:1",
        None,
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Run(tacho::Run {
                pid: Some(1234),
                provider: Some("claude".to_string()),
                model: Some("claude-3-5-sonnet".to_string()),
                session_id: Some("session-123".to_string()),
                cwd: Some("~/dev/tachograph".to_string()),
                task: Some("task-42".to_string()),
                parent_run_seq: Some(1),
                resume: Some(false),
                git_tree: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()),
            })),
        },
    ));
    seq += 1;

    // 2. Turn (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Turn,
        "agent:1/run:1/turns",
        Some(100),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Turn(tacho::Turn {
                turn_number: Some(1),
                model: Some("claude-3-5-sonnet".to_string()),
            })),
        },
    ));
    seq += 1;

    // 3. Prompt (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Prompt,
        "agent:1/run:1/turns",
        Some(100),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Prompt(tacho::Prompt {
                digest: Some("0123456789abcdef".to_string()),
                bytes: Some(256),
            })),
        },
    ));
    seq += 1;

    // 4. ToolCall (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ToolCall,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::ToolCall(tacho::ToolCall {
                name: Some("bash".to_string()),
                input_digest: Some("abcdef0123456789".to_string()),
                input_bytes: Some(64),
                tool_use_id: Some("tool_1".to_string()),
            })),
        },
    ));
    seq += 1;

    // 5. ToolCallEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ToolCallEnded,
        "agent:1/run:1/turns",
        Some(103),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::ToolCallEnded(tacho::ToolCallEnded {
                error: Some(false),
                result_bytes: Some(128),
                abandoned: Some(false),
            })),
        },
    ));
    seq += 1;

    // 6. Command (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Command,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Command(tacho::Command {
                argv: Some("cargo test -p core".to_string()),
                cwd: Some("/workspace".to_string()),
                class: Some(tacho::CommandClass::Test as i32),
                check_name: Some("unit_test".to_string()),
                via_wrap: Some(false),
                program: Some("cargo".to_string()),
                tool_use_id: Some("call-cmd-1".to_string()),
                git_tree: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()),
            })),
        },
    ));
    seq += 1;

    // 7. CommandEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::CommandEnded,
        "agent:1/run:1/turns",
        Some(105),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::CommandEnded(tacho::CommandEnded {
                exit: Some(0),
                timed_out: Some(false),
                stdout_bytes: Some(512),
                stderr_bytes: Some(0),
                tests_run: Some(12),
                tests_failed: Some(0),
                abandoned: Some(false),
            })),
        },
    ));
    seq += 1;

    // 8. FileAccess (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::FileAccess,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::FileAccess(tacho::FileAccess {
                path: Some("src/lib.rs".to_string()),
                kind: Some(tacho::FileAccessKind::Read as i32),
                query: Some("fn main".to_string()),
                tool_use_id: Some("call-file-1".to_string()),
            })),
        },
    ));
    seq += 1;

    // 9. FileAccessEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::FileAccessEnded,
        "agent:1/run:1/turns",
        Some(107),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::FileAccessEnded(
                tacho::FileAccessEnded {
                    bytes_read: Some(1024),
                    items_found: Some(1),
                    error: Some(false),
                    abandoned: Some(false),
                },
            )),
        },
    ));
    seq += 1;

    // 10. FileEdited (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::FileEdited,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::FileEdited(tacho::FileEdited {
                path: Some("src/main.rs".to_string()),
                kind: Some(tacho::FileEditKind::Edit as i32),
                lines_added: Some(15),
                lines_removed: Some(3),
                in_brief: Some(true),
            })),
        },
    ));
    seq += 1;

    // 11. Message (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Message,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Message(tacho::Message {
                from: Some("agent:1".to_string()),
                to: Some("agent:2".to_string()),
                message_id: Some("msg-1".to_string()),
                bytes: Some(64),
                direction: Some(tacho::MessageDirection::Sent as i32),
            })),
        },
    ));
    seq += 1;

    // 12. Compacted (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Compacted,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Compacted(tacho::Compacted {
                tokens_before: Some(8000),
                tokens_after: Some(1500),
                bytes_compacted: Some(6500),
            })),
        },
    ));
    seq += 1;

    // 13. TurnEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::TurnEnded,
        "agent:1/run:1/turns",
        Some(101),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::TurnEnded(tacho::TurnEnded {
                input_tokens: Some(100),
                output_tokens: Some(250),
                cached_tokens: Some(50),
                text_bytes: Some(1024),
                cost_usd_micros: Some(750),
            })),
        },
    ));
    seq += 1;

    // 14. Wait (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Wait,
        "agent:1/run:1/waits",
        Some(100),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Wait(tacho::Wait {
                target: Some("user_input".to_string()),
                on: Some(tacho::WaitOn::WaitInput as i32),
            })),
        },
    ));
    seq += 1;

    // 15. WaitEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::WaitEnded,
        "agent:1/run:1/waits",
        Some(113),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::WaitEnded(tacho::WaitEnded {
                timed_out: Some(false),
                abandoned: Some(false),
            })),
        },
    ));
    seq += 1;

    // 16. Stall (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Stall,
        "agent:1",
        Some(100),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::Stall(tacho::Stall {
                kind: Some(tacho::StallKind::Loop as i32),
                since_us: Some(4000000),
                repeats: Some(3),
                evidence: Some("repeated identical tool calls".to_string()),
            })),
        },
    ));
    seq += 1;

    // 17. ProviderError (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ProviderError,
        "provider:anthropic",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::ProviderError(tacho::ProviderError {
                provider: Some("anthropic".to_string()),
                error_code: Some("overloaded".to_string()),
                message: Some("service overloaded".to_string()),
                rate_limit: Some(false),
                retry_after_ms: Some(2000),
            })),
        },
    ));
    seq += 1;

    // 18. TaskQueued (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::TaskQueued,
        "task:test",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::TaskQueued(tacho::TaskQueued {
                slug: Some("task-test".to_string()),
                brief_digest: Some("brief12345678".to_string()),
                brief_bytes: Some(1024),
                paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            })),
        },
    ));
    seq += 1;

    // 19. TaskState (state)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::TaskState,
        "task:test/state",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::TaskState(tacho::TaskState {
                state: Some(tacho::TaskStatus::TaskInProgress as i32),
                because: Some("commencing work".to_string()),
            })),
        },
    ));
    seq += 1;

    // 20. AgentState (state)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::AgentState,
        "agent:1/state",
        None,
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::AgentState(tacho::AgentState {
                state: Some(tacho::AgentStateType::Thinking as i32),
                because: Some("turn 1 in progress".to_string()),
            })),
        },
    ));
    seq += 1;

    // 21. Milestone (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Milestone,
        "task:test",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::Milestone(tacho::Milestone {
                name: Some("initial-setup".to_string()),
                description: Some("project scaffolding completed".to_string()),
            })),
        },
    ));
    seq += 1;

    // 22. Review (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Review,
        "task:test/review",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::Review(tacho::Review {
                task: Some("task-test".to_string()),
                reviewer: Some("reviewer-agent".to_string()),
            })),
        },
    ));
    seq += 1;

    // 23. ReviewFinding (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ReviewFinding,
        "task:test/review",
        Some(121),
        None,
        tacho::Event {
            body: Some(tacho::event::Body::ReviewFinding(tacho::ReviewFinding {
                task: Some("task-test".to_string()),
                severity: Some(tacho::FindingSeverity::FindingMinor as i32),
                path: Some("src/lib.rs".to_string()),
                line: Some(25),
                title: Some("documentation typo".to_string()),
            })),
        },
    ));
    seq += 1;

    // 24. ReviewEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ReviewEnded,
        "task:test/review",
        Some(121),
        None,
        tacho::Event {
            body: Some(tacho::event::Body::ReviewEnded(tacho::ReviewEnded {
                findings_count: Some(1),
                approved: Some(true),
                abandoned: Some(false),
            })),
        },
    ));
    seq += 1;

    // 25. Snapshot (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Snapshot,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::Snapshot(tacho::Snapshot {
                trigger: Some("cli-trace".to_string()),
            })),
        },
    ));
    seq += 1;

    // 26. SnapshotEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::SnapshotEnded,
        "daemon",
        Some(124),
        None,
        tacho::Event {
            body: Some(tacho::event::Body::SnapshotEnded(tacho::SnapshotEnded {
                rings: vec![tacho::RingStats {
                    name: Some("core".to_string()),
                    bytes: Some(4096),
                    chunks_overwritten: Some(0),
                    chunks_skipped: Some(0),
                }],
                duration_us: Some(120),
            })),
        },
    ));
    seq += 1;

    // 27. Daemon (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Daemon,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::Daemon(tacho::Daemon {
                action: Some(tacho::DaemonAction::DaemonStarted as i32),
                version: Some("0.1.0".to_string()),
                pid: Some(4242),
                host: Some("zezeozue.c.googlers.com".to_string()),
            })),
        },
    ));
    seq += 1;

    // 28. ClockStep (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::ClockStep,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::ClockStep(tacho::ClockStep {
                delta_ns: Some(500000),
                reason: Some("monotonic clamp".to_string()),
            })),
        },
    ));
    seq += 1;

    // 29. TrackCreated (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::TrackCreated,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::TrackCreated(tacho::TrackCreated {
                key: Some("custom:test".to_string()),
                uuid: Some(99999),
                parent_uuid: Some(1),
                name: Some("custom:test".to_string()),
                collision: Some(false),
            })),
        },
    ));
    seq += 1;

    // 30. TraceError (instant)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::TraceError,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::TraceError(tacho::TraceError {
                message: Some("unmatched slice end detected".to_string()),
                track_key: Some("agent:1/run:1".to_string()),
            })),
        },
    ));
    seq += 1;

    // 31. Custom (begin)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::Custom,
        "custom:test",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::Custom(tacho::Custom {
                name: Some("my_custom_span".to_string()),
                id: Some("custom-span-1".to_string()),
                args: vec![tacho::CustomArg {
                    key: Some("environment".to_string()),
                    value: Some("staging".to_string()),
                }],
            })),
        },
    ));
    seq += 1;

    // 32. CustomEnded (end)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::CustomEnded,
        "custom:test",
        Some(130),
        None,
        tacho::Event {
            body: Some(tacho::event::Body::CustomEnded(tacho::CustomEnded {
                id: Some("custom-span-1".to_string()),
                args: vec![tacho::CustomArg {
                    key: Some("result".to_string()),
                    value: Some("ok".to_string()),
                }],
                abandoned: Some(false),
            })),
        },
    ));
    seq += 1;

    // 33. RunEnded (end of run)
    events.push(Event::new(
        seq,
        next_ts!(),
        Kind::RunEnded,
        "agent:1/run:1",
        Some(100),
        Some("agent:1".to_string()),
        tacho::Event {
            body: Some(tacho::event::Body::RunEnded(tacho::RunEnded {
                reason: Some("success".to_string()),
                exit_code: Some(0),
                turns: Some(1),
                tool_calls: Some(3),
                files_edited: Some(1),
            })),
        },
    ));

    events
}
