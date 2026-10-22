//! Deterministic simulation test driving seeded event streams with manually advanced timestamps.
//!
//! Validates the 5 stall detector and state lifecycle guarantees:
//! 1) No SILENT fires during a 25-minute simulated `cargo clippy` while `agent:w1/run:1/cpu` counter increases every 2s.
//! 2) SILENT fires once CPU stops increasing for `> silent_threshold_ns`.
//! 3) Every `Stalled` state clears on the next real progress event.
//! 4) No double firing on the same evidence (SILENT, LOOP, and FAILING).
//! 5) CLAIM fires when a run edits a file and exits without a subsequent passing TEST,
//!    and does NOT fire if a passing TEST succeeds after the edit.

use tacho_core::{FileEditKind, Input, Kind, Recorder, SessionKey, ToolInput};
use tacho_proto::tacho;

#[test]
fn test_simulation_silent_and_cpu_awareness() {
    let mut rec = Recorder::new();
    let mut out = Vec::new();
    let session = SessionKey::new("claude", "sess-w1");
    let mut ts = 1_000_000_000u64; // 1s

    // Start run for agent w1
    rec.apply(
        Input::RunStart {
            provider: "claude".into(),
            session_id: "sess-w1".into(),
            agent: Some("w1".into()),
            task: Some("Compile clippy".into()),
            pid: Some(1001),
            cwd: Some("/workspace".into()),
            model: Some("claude-3-5".into()),
            parent_session: None,
            git_tree: None,
            source_ts_ns: Some(ts),
            role: None,
        },
        ts,
        &mut out,
    );

    // Start turn
    ts += 1_000_000;
    rec.apply(
        Input::TurnStart {
            session: session.clone(),
            model: Some("claude-3-5".into()),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    // Begin tool: cargo clippy
    ts += 1_000_000;
    rec.apply(
        Input::ToolBegin {
            session: session.clone(),
            tool_use_id: "clippy-1".into(),
            tool_name: "bash".into(),
            input: ToolInput::Shell {
                argv: "cargo clippy".into(),
                cwd: Some("/workspace".into()),
                git_tree: None,
            },
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    // Guarantee 1: 25-minute simulated cargo clippy (1500s = 750 iterations of 2s).
    // CPU counter increases every 2s.
    for i in 0..750 {
        ts += 2_000_000_000; // +2s
        rec.apply(
            Input::CounterSample {
                track_key: "agent:w1/run:1/cpu".into(),
                value: (i + 1) * 100,
                unit: Some("ms".into()),
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
        rec.tick(ts, &mut out);
    }

    // Verify 0 SILENT stall events occurred during active CPU
    let silent_events: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Silent as i32);
                }
            }
            false
        })
        .collect();
    assert!(
        silent_events.is_empty(),
        "No SILENT stall events should fire during active CPU, got {}",
        silent_events.len()
    );
    assert_ne!(
        rec.live_state().agents.get("w1").map(|a| a.state.as_str()),
        Some("Stalled")
    );

    // Guarantee 2: CPU stops increasing. Advance time past silent_threshold_ns (300s).
    // Advance 350s in 10s steps with periodic ticks and no CPU counter increases.
    for _ in 0..35 {
        ts += 10_000_000_000; // +10s
        rec.tick(ts, &mut out);
    }

    let silent_events: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Silent as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        silent_events.len(),
        1,
        "Expected exactly 1 SILENT stall event once CPU stopped, got {}",
        silent_events.len()
    );
    assert_eq!(
        rec.live_state().agents.get("w1").map(|a| a.state.as_str()),
        Some("Stalled")
    );
    assert!(rec.live_state().agents["w1"]
        .flags
        .contains(&"SILENT".to_string()));

    // Guarantee 3: Stalled state clears on next real progress event
    ts += 1_000_000;
    rec.apply(
        Input::ToolEnd {
            session: session.clone(),
            tool_use_id: "clippy-1".into(),
            error: false,
            result_bytes: Some(512),
            exit_code: Some(0),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    assert_ne!(
        rec.live_state().agents.get("w1").map(|a| a.state.as_str()),
        Some("Stalled"),
        "Agent state should clear away from Stalled upon tool completion"
    );
    assert!(
        rec.live_state().agents["w1"].flags.is_empty(),
        "Agent stall flags should be cleared after progress event"
    );
}

#[test]
fn test_simulation_no_double_firing_on_same_evidence() {
    let mut rec = Recorder::new();
    let mut out = Vec::new();
    let session = SessionKey::new("claude", "sess-w-dedup");
    let mut ts = 1_000_000_000u64;

    rec.apply(
        Input::RunStart {
            provider: "claude".into(),
            session_id: "sess-w-dedup".into(),
            agent: Some("w-dedup".into()),
            task: Some("Dedup tests".into()),
            pid: Some(2001),
            cwd: Some("/workspace".into()),
            model: Some("claude-3-5".into()),
            parent_session: None,
            git_tree: None,
            source_ts_ns: Some(ts),
            role: None,
        },
        ts,
        &mut out,
    );

    // 1) Silence dedup: advance time past 300s to fire SILENT
    ts += 350_000_000_000; // 350s
    rec.tick(ts, &mut out);

    let count_silent_1 = out
        .iter()
        .filter(|e| {
            e.kind == Kind::Stall
                && e.custom_name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("Stall"))
        })
        .count();
    assert_eq!(
        count_silent_1, 1,
        "First silence timeout should fire SILENT once"
    );

    // Repeated ticks without any intervening events must NOT fire SILENT again
    for _ in 0..10 {
        ts += 10_000_000_000;
        rec.tick(ts, &mut out);
    }
    let count_silent_2 = out
        .iter()
        .filter(|e| {
            e.kind == Kind::Stall
                && e.custom_name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("Stall"))
        })
        .count();
    assert_eq!(
        count_silent_2, 1,
        "Repeated ticks on same silence must not produce duplicate firings"
    );

    // 2) Loop dedup: 4 identical tool calls fire LOOP on the 4th
    ts += 1_000_000;
    rec.apply(
        Input::TurnStart {
            session: session.clone(),
            model: None,
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    for i in 1..=4 {
        ts += 1_000_000;
        rec.apply(
            Input::ToolBegin {
                session: session.clone(),
                tool_use_id: format!("loop-tool-{}", i),
                tool_name: "bash".into(),
                input: ToolInput::Shell {
                    argv: "ls -la".into(),
                    cwd: None,
                    git_tree: None,
                },
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
        ts += 1_000_000;
        rec.apply(
            Input::ToolEnd {
                session: session.clone(),
                tool_use_id: format!("loop-tool-{}", i),
                error: false,
                result_bytes: Some(10),
                exit_code: Some(0),
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
    }

    let loop_stalls: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Loop as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        loop_stalls.len(),
        1,
        "LOOP should fire exactly once on 4th repeat"
    );

    // 5th and 6th identical call should NOT double-fire on the same repeat streak
    for i in 5..=6 {
        ts += 1_000_000;
        rec.apply(
            Input::ToolBegin {
                session: session.clone(),
                tool_use_id: format!("loop-tool-{}", i),
                tool_name: "bash".into(),
                input: ToolInput::Shell {
                    argv: "ls -la".into(),
                    cwd: None,
                    git_tree: None,
                },
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
    }

    let loop_stalls_after: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Loop as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        loop_stalls_after.len(),
        1,
        "LOOP must not double fire on 5th or 6th identical tool"
    );

    // 3) FAILING dedup: failing command 3 times in a row fires FAILING on 3rd failure
    for i in 1..=3 {
        ts += 1_000_000;
        rec.apply(
            Input::ToolBegin {
                session: session.clone(),
                tool_use_id: format!("fail-tool-{}", i),
                tool_name: "bash".into(),
                input: ToolInput::Shell {
                    argv: "cargo test -p core".into(),
                    cwd: None,
                    git_tree: None,
                },
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
        ts += 1_000_000;
        rec.apply(
            Input::ToolEnd {
                session: session.clone(),
                tool_use_id: format!("fail-tool-{}", i),
                error: true,
                result_bytes: Some(10),
                exit_code: Some(1),
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
    }

    let failing_stalls: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Failing as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        failing_stalls.len(),
        1,
        "FAILING should fire on 3rd failure"
    );

    // 4th failure of the same command must NOT double fire
    ts += 1_000_000;
    rec.apply(
        Input::ToolBegin {
            session: session.clone(),
            tool_use_id: "fail-tool-4".into(),
            tool_name: "bash".into(),
            input: ToolInput::Shell {
                argv: "cargo test -p core".into(),
                cwd: None,
                git_tree: None,
            },
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );
    ts += 1_000_000;
    rec.apply(
        Input::ToolEnd {
            session: session.clone(),
            tool_use_id: "fail-tool-4".into(),
            error: true,
            result_bytes: Some(10),
            exit_code: Some(1),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    let failing_stalls_after: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::Failing as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        failing_stalls_after.len(),
        1,
        "FAILING must not double fire on 4th consecutive failure"
    );
}

#[test]
fn test_simulation_claim_detector() {
    let mut rec = Recorder::new();
    let mut out = Vec::new();
    let session_claim = SessionKey::new("claude", "sess-claim");
    let mut ts = 1_000_000_000u64;

    // Start run
    rec.apply(
        Input::RunStart {
            provider: "claude".into(),
            session_id: "sess-claim".into(),
            agent: Some("w-claim".into()),
            task: Some("Claim test".into()),
            pid: Some(3001),
            cwd: Some("/workspace".into()),
            model: Some("claude-3-5".into()),
            parent_session: None,
            git_tree: None,
            source_ts_ns: Some(ts),
            role: None,
        },
        ts,
        &mut out,
    );

    // Edit a file
    ts += 1_000_000;
    rec.apply(
        Input::ToolBegin {
            session: session_claim.clone(),
            tool_use_id: "edit-1".into(),
            tool_name: "edit".into(),
            input: ToolInput::FileEdit {
                path: "src/lib.rs".into(),
                kind: Some(FileEditKind::Edit),
                lines_added: Some(10),
                lines_removed: Some(2),
            },
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );
    ts += 1_000_000;
    rec.apply(
        Input::ToolEnd {
            session: session_claim.clone(),
            tool_use_id: "edit-1".into(),
            error: false,
            result_bytes: Some(100),
            exit_code: Some(0),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    // Run a test that FAILS
    ts += 1_000_000;
    rec.apply(
        Input::ToolBegin {
            session: session_claim.clone(),
            tool_use_id: "test-failing".into(),
            tool_name: "bash".into(),
            input: ToolInput::Shell {
                argv: "cargo test".into(),
                cwd: None,
                git_tree: None,
            },
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );
    ts += 1_000_000;
    rec.apply(
        Input::ToolEnd {
            session: session_claim.clone(),
            tool_use_id: "test-failing".into(),
            error: true,
            result_bytes: Some(100),
            exit_code: Some(1),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    // End run with clean exit (exit_code == 0)
    ts += 1_000_000;
    rec.apply(
        Input::RunEnd {
            session: session_claim.clone(),
            exit_code: Some(0),
            reason: Some("work completed".into()),
            source_ts_ns: Some(ts),
        },
        ts,
        &mut out,
    );

    // Guarantee 5: CLAIM fires because run edited files and exited without subsequent passing TEST
    let claim_stalls: Vec<_> = out
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::StallClaim as i32);
                }
            }
            false
        })
        .collect();
    assert_eq!(
        claim_stalls.len(),
        1,
        "CLAIM stall must fire on clean run exit after edits without passing TEST"
    );
    assert!(rec.live_state().agents["w-claim"]
        .flags
        .contains(&"CLAIM".to_string()));

    // Positive case: Run with edit FOLLOWED BY passing test does NOT fire CLAIM
    let mut rec_good = Recorder::new();
    let mut out_good = Vec::new();
    let session_good = SessionKey::new("claude", "sess-good");
    let mut ts_good = 1_000_000_000u64;

    rec_good.apply(
        Input::RunStart {
            provider: "claude".into(),
            session_id: "sess-good".into(),
            agent: Some("w-good".into()),
            task: Some("Verified edits".into()),
            pid: Some(4001),
            cwd: Some("/workspace".into()),
            model: Some("claude-3-5".into()),
            parent_session: None,
            git_tree: None,
            source_ts_ns: Some(ts_good),
            role: None,
        },
        ts_good,
        &mut out_good,
    );

    // Edit file
    ts_good += 1_000_000;
    rec_good.apply(
        Input::ToolBegin {
            session: session_good.clone(),
            tool_use_id: "edit-good".into(),
            tool_name: "edit".into(),
            input: ToolInput::FileEdit {
                path: "src/lib.rs".into(),
                kind: Some(FileEditKind::Edit),
                lines_added: Some(5),
                lines_removed: Some(0),
            },
            source_ts_ns: Some(ts_good),
        },
        ts_good,
        &mut out_good,
    );
    ts_good += 1_000_000;
    rec_good.apply(
        Input::ToolEnd {
            session: session_good.clone(),
            tool_use_id: "edit-good".into(),
            error: false,
            result_bytes: Some(10),
            exit_code: Some(0),
            source_ts_ns: Some(ts_good),
        },
        ts_good,
        &mut out_good,
    );

    // Subsequent PASSING test
    ts_good += 1_000_000;
    rec_good.apply(
        Input::ToolBegin {
            session: session_good.clone(),
            tool_use_id: "test-passing".into(),
            tool_name: "bash".into(),
            input: ToolInput::Shell {
                argv: "cargo test --workspace".into(),
                cwd: None,
                git_tree: None,
            },
            source_ts_ns: Some(ts_good),
        },
        ts_good,
        &mut out_good,
    );
    ts_good += 1_000_000;
    rec_good.apply(
        Input::ToolEnd {
            session: session_good.clone(),
            tool_use_id: "test-passing".into(),
            error: false,
            result_bytes: Some(100),
            exit_code: Some(0),
            source_ts_ns: Some(ts_good),
        },
        ts_good,
        &mut out_good,
    );

    // End run
    ts_good += 1_000_000;
    rec_good.apply(
        Input::RunEnd {
            session: session_good.clone(),
            exit_code: Some(0),
            reason: Some("all tests passed".into()),
            source_ts_ns: Some(ts_good),
        },
        ts_good,
        &mut out_good,
    );

    let claim_stalls_good: Vec<_> = out_good
        .iter()
        .filter(|e| {
            if e.kind == Kind::Stall {
                if let Some(tacho::event::Body::Stall(ref s)) = e.body.body {
                    return s.kind == Some(tacho::StallKind::StallClaim as i32);
                }
            }
            false
        })
        .collect();
    assert!(
        claim_stalls_good.is_empty(),
        "CLAIM stall must NOT fire when passing test occurs after file edits"
    );
    assert!(!rec_good.live_state().agents["w-good"]
        .flags
        .contains(&"CLAIM".to_string()));
}
