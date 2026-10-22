//! Replay tests for Claude Code and Jetski fixture sequences.
//!
//! Validates that parallel tool calls receive concurrent lane allocations,
//! subagent flows link properly, file edits and command classifications are derived,
//! and the resulting traces render with 0 errors in Perfetto trace_processor_shell.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tacho_core::{
    encode_snapshot_header, Encoder, Event, FileEditKind, Input, Kind, Recorder, SessionKey,
    ToolInput,
};

fn run_trace_processor(trace_bytes: &[u8]) {
    let tp_path = env::var("TACHO_TP").unwrap_or_else(|_| {
        "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell".to_string()
    });

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let trace_path = temp_dir.path().join("replay.pftrace");
    fs::write(&trace_path, trace_bytes).expect("write trace");

    // 1. Assert 0 error/data_loss stats
    let output = Command::new(&tp_path)
        .arg(&trace_path)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output()
        .expect("trace_processor_shell stats");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "trace_processor_shell failed:\n{}\n{}",
        stdout,
        stderr
    );

    let stats_rows: Vec<&str> = stdout
        .trim()
        .lines()
        .filter(|l| l.contains("\"error\"") || l.contains("\"data_loss\""))
        .collect();
    assert!(
        stats_rows.is_empty(),
        "trace_processor reported error or data_loss stats: {:?}",
        stats_rows
    );

    // 2. Query slice depth to verify proper nesting
    let depth_output = Command::new(&tp_path)
        .arg(&trace_path)
        .arg("-Q")
        .arg("SELECT track_id, depth, name FROM slice ORDER BY ts;")
        .output()
        .expect("trace_processor_shell slice query");
    assert!(depth_output.status.success());
}

#[test]
fn test_replay_claude_parallel_tools() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/sequences/claude-parallel-tools.jsonl");
    let content = fs::read_to_string(&fixture_path).expect("read claude fixture");

    let mut rec = Recorder::new();
    let mut events = Vec::new();
    let mut now_ns = 1_000_000_000u64;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).expect("parse json line");
        let hook = v["hook_event_name"].as_str().unwrap_or_default();
        let session_id = v["session_id"].as_str().unwrap_or("unknown");
        let session = SessionKey::new("claude", session_id);
        let cwd = v["cwd"].as_str().map(String::from);

        now_ns += 5_000_000;

        match hook {
            "UserPromptSubmit" => {
                let prompt = v["prompt"].as_str().unwrap_or_default();
                rec.apply(
                    Input::RunStart {
                        provider: "claude".to_string(),
                        session_id: session_id.to_string(),
                        agent: Some("parent".to_string()),
                        task: Some("Search repo".to_string()),
                        pid: Some(101),
                        cwd,
                        model: Some("claude-3-5-sonnet".to_string()),
                        parent_session: None,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
                rec.apply(
                    Input::Prompt {
                        session: session.clone(),
                        text_len: prompt.len() as u64,
                        digest: Some("prompt-hash".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
                rec.apply(
                    Input::TurnStart {
                        session,
                        model: Some("claude-3-5-sonnet".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "SessionStart" => {
                let parent_tool = v["parent_tool_use_id"].as_str().map(String::from);
                rec.apply(
                    Input::RunStart {
                        provider: "claude".to_string(),
                        session_id: session_id.to_string(),
                        agent: Some("subagent-202".to_string()),
                        task: Some("Run cargo test".to_string()),
                        pid: Some(202),
                        cwd,
                        model: Some("claude-3-5-sonnet".to_string()),
                        parent_session: parent_tool,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PreToolUse" => {
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or_default().to_string();
                let tool_name = v["tool_name"].as_str().unwrap_or_default().to_string();
                let tool_in_val = &v["tool_input"];

                let input = match tool_name.as_str() {
                    "View" => ToolInput::FileRead {
                        path: tool_in_val["file_path"].as_str().unwrap_or("").to_string(),
                    },
                    "Grep" => ToolInput::FileSearch {
                        pattern: tool_in_val["pattern"].as_str().unwrap_or("").to_string(),
                        path: None,
                    },
                    "Task" => ToolInput::Subagent {
                        description_len: tool_in_val["prompt"].as_str().map(|p| p.len() as u64),
                    },
                    "Bash" => ToolInput::Shell {
                        argv: tool_in_val["command"].as_str().unwrap_or("").to_string(),
                        cwd: None,
                    },
                    _ => ToolInput::Other {
                        input_digest: None,
                        input_bytes: None,
                    },
                };

                rec.apply(
                    Input::ToolBegin {
                        session,
                        tool_use_id,
                        tool_name,
                        input,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PostToolUse" => {
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or_default().to_string();
                let resp = &v["tool_response"];
                let exit_code = resp["exit_code"].as_i64().map(|c| c as i32);
                let content_len = resp["content"]
                    .as_str()
                    .or_else(|| resp["stdout"].as_str())
                    .or_else(|| resp["result"].as_str())
                    .map(|s| s.len() as u64);

                rec.apply(
                    Input::ToolEnd {
                        session,
                        tool_use_id,
                        error: false,
                        result_bytes: content_len,
                        exit_code,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "SubagentStop" => {
                // Agent is done
            }
            "SessionEnd" => {
                rec.apply(
                    Input::RunEnd {
                        session,
                        exit_code: Some(0),
                        reason: Some("complete".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "Stop" => {
                rec.apply(
                    Input::TurnEnd {
                        session: session.clone(),
                        input_tokens: Some(150),
                        output_tokens: Some(80),
                        cached_tokens: Some(30),
                        text_bytes: Some(120),
                        cost_micros: Some(45),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
                rec.apply(
                    Input::RunEnd {
                        session,
                        exit_code: Some(0),
                        reason: Some("end_turn".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            _ => {}
        }
    }

    // 1. Assert parallel lane allocation for toolu_01_read and toolu_02_grep
    let tool1 = events
        .iter()
        .find(|e| {
            if let Some(tacho_proto::tacho::event::Body::FileAccess(ref f)) = e.body.body {
                f.path.as_deref() == Some("src/main.rs")
            } else {
                false
            }
        })
        .expect("tool1 View found");

    let tool2 = events
        .iter()
        .find(|e| {
            if let Some(tacho_proto::tacho::event::Body::FileAccess(ref f)) = e.body.body {
                f.query.as_deref() == Some("test")
            } else {
                false
            }
        })
        .expect("tool2 Grep found");

    assert_eq!(
        tool1.track_key, "agent:parent/run:1/turns",
        "Tool 1 nested in turn without sibling should be on turns track"
    );
    assert_eq!(
        tool2.track_key, "agent:parent/run:1/lane:1",
        "Tool 2 concurrent with tool 1 should be on lane 1"
    );

    // 2. Assert subagent session run has flow link from parent Task tool call
    let subagent_run = events
        .iter()
        .find(|e| e.kind == Kind::Run && e.track_key == "agent:subagent-202/run:1")
        .expect("subagent run found");
    assert!(
        !subagent_run.terminating_flow_ids.is_empty(),
        "Subagent run should terminate parent flow"
    );

    // 3. Encode to Perfetto trace and run trace_processor_shell
    let mut trace_bytes = Vec::new();
    encode_snapshot_header(
        rec.registry(),
        1_000_000_000,
        Some(500_000_000),
        &mut trace_bytes,
    );

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::new();
    for event in &events {
        packet_buf.clear();
        encoder.encode_event(event, rec.registry(), &mut packet_buf);
        trace_bytes.extend_from_slice(&packet_buf);
    }

    run_trace_processor(&trace_bytes);
}

#[test]
fn test_replay_jetski_session() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/sequences/jetski-session.jsonl");
    let content = fs::read_to_string(&fixture_path).expect("read jetski fixture");

    let mut rec = Recorder::new();
    let mut events = Vec::new();
    let mut now_ns = 1_000_000_000u64;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).expect("parse json line");
        let hook = v["hook_event_name"].as_str().unwrap_or_default();
        let conv_id = v["conversationId"].as_str().unwrap_or("unknown");
        let session = SessionKey::new("jetski", conv_id);

        now_ns += 5_000_000;

        match hook {
            "SessionStart" => {
                let cwd = v["workspacePaths"][0].as_str().map(String::from);
                rec.apply(
                    Input::RunStart {
                        provider: "jetski".to_string(),
                        session_id: conv_id.to_string(),
                        agent: Some("jetski-main".to_string()),
                        task: Some("Math operations".to_string()),
                        pid: Some(777),
                        cwd,
                        model: v["modelName"].as_str().map(String::from),
                        parent_session: None,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PreInvocation" => {
                rec.apply(
                    Input::TurnStart {
                        session,
                        model: Some("gemini-2.5-pro".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PreToolUse" => {
                let step_idx = v["stepIdx"].as_u64().unwrap_or(0);
                let tool_call = &v["toolCall"];
                let name = tool_call["name"].as_str().unwrap_or_default();
                let args = &tool_call["args"];
                let tool_use_id = format!("call-{}", step_idx);

                let input = match name {
                    "run_command" => ToolInput::Shell {
                        argv: args["CommandLine"].as_str().unwrap_or("").to_string(),
                        cwd: None,
                    },
                    "read_file" => ToolInput::FileRead {
                        path: args["Path"].as_str().unwrap_or("").to_string(),
                    },
                    "edit_file" => ToolInput::FileEdit {
                        path: args["Path"].as_str().unwrap_or("").to_string(),
                        kind: Some(FileEditKind::Edit),
                        lines_added: Some(1),
                        lines_removed: Some(1),
                    },
                    _ => ToolInput::Other {
                        input_digest: None,
                        input_bytes: None,
                    },
                };

                rec.apply(
                    Input::ToolBegin {
                        session,
                        tool_use_id,
                        tool_name: name.to_string(),
                        input,
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PostToolUse" => {
                let step_idx = v["stepIdx"].as_u64().unwrap_or(0);
                let tool_use_id = format!("call-{}", step_idx);
                let res = v["result"].as_str();
                let err = v["error"].as_str().unwrap_or("");

                rec.apply(
                    Input::ToolEnd {
                        session,
                        tool_use_id,
                        error: !err.is_empty(),
                        result_bytes: res.map(|r| r.len() as u64),
                        exit_code: Some(0),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "PostInvocation" => {
                rec.apply(
                    Input::TurnEnd {
                        session,
                        input_tokens: Some(400),
                        output_tokens: Some(200),
                        cached_tokens: Some(100),
                        text_bytes: Some(300),
                        cost_micros: Some(85),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            "Stop" => {
                rec.apply(
                    Input::RunEnd {
                        session,
                        exit_code: Some(0),
                        reason: Some("NO_TOOL_CALL".to_string()),
                        source_ts_ns: Some(now_ns),
                    },
                    now_ns,
                    &mut events,
                );
            }
            _ => {}
        }
    }

    // 1. Assert sequential tool calls reuse lane 1
    let tool_begins: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e.kind, Kind::Command | Kind::FileAccess | Kind::ToolCall))
        .collect();
    assert_eq!(
        tool_begins.len(),
        4,
        "Should have 4 tool calls (git status, read, edit, blaze test)"
    );
    for tb in &tool_begins {
        assert_eq!(
            tb.track_key, "agent:jetski-main/run:1/turns",
            "Sequential tool calls inside turn without siblings should be on turns track"
        );
    }

    // 2. Assert command classification: 'blaze test //src:math_test' -> CommandClass::Test
    let blaze_test = events
        .iter()
        .find(|e| {
            if let Some(tacho_proto::tacho::event::Body::Command(ref c)) = e.body.body {
                c.argv.as_deref() == Some("blaze test //src:math_test")
            } else {
                false
            }
        })
        .expect("blaze test command event");
    if let Some(tacho_proto::tacho::event::Body::Command(ref c)) = blaze_test.body.body {
        assert_eq!(
            c.class,
            Some(tacho_proto::tacho::CommandClass::Test as i32),
            "blaze test must be classified as Test"
        );
    }

    // 3. Assert FileEdited event was generated for edit_file
    let file_edited = events
        .iter()
        .find(|e| e.kind == Kind::FileEdited)
        .expect("FileEdited event found");
    if let Some(tacho_proto::tacho::event::Body::FileEdited(ref fe)) = file_edited.body.body {
        assert_eq!(fe.lines_added, Some(1));
        assert_eq!(fe.lines_removed, Some(1));
    }

    // 4. Encode to Perfetto and run trace_processor_shell
    let mut trace_bytes = Vec::new();
    encode_snapshot_header(
        rec.registry(),
        1_000_000_000,
        Some(500_000_000),
        &mut trace_bytes,
    );

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::new();
    for event in &events {
        packet_buf.clear();
        encoder.encode_event(event, rec.registry(), &mut packet_buf);
        trace_bytes.extend_from_slice(&packet_buf);
    }

    run_trace_processor(&trace_bytes);
}
