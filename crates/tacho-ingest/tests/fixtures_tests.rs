use std::fs;
use std::path::PathBuf;
use tacho_core::{Input, SessionKey, ToolInput, WaitOn};
use tacho_ingest::hooks::{blake3_digest_8, HookEnv};
use tacho_ingest::streams::{detect, StreamKind, StreamParser};

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // root
    p.push("tests");
    p.push("fixtures");
    p
}

fn test_env() -> HookEnv {
    HookEnv {
        agent: Some("agent-1".to_string()),
        task: Some("task-1".to_string()),
        invoked_at_ns: 1000,
        ppid: Some(1234),
    }
}

// ---------------------------------------------------------------------------
// Claude Hook Fixture Tests
// ---------------------------------------------------------------------------

#[test]
fn test_claude_session_start() {
    let path = fixtures_dir().join("hooks/claude/SessionStart.json");
    let content = fs::read_to_string(&path).expect("read SessionStart.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunStart {
            source_ts_ns: Some(1000),
            provider: "claude".to_string(),
            session_id: "7ab5f92a-5551-4941-a820-53a43c8ebeb9".to_string(),
            agent: Some("agent-1".to_string()),
            task: Some("task-1".to_string()),
            pid: Some(1234),
            cwd: Some("/Users/developer/project".to_string()),
            model: None,
            parent_session: None,
        }]
    );
}

#[test]
fn test_claude_user_prompt_submit() {
    let path = fixtures_dir().join("hooks/claude/UserPromptSubmit.json");
    let content = fs::read_to_string(&path).expect("read UserPromptSubmit.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let prompt = "Run cargo test and fix any failing unit tests";
    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![
            Input::Prompt {
                source_ts_ns: Some(1000),
                session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
                text_len: prompt.len() as u64,
                digest: Some(blake3_digest_8(prompt)),
            },
            Input::TurnStart {
                source_ts_ns: Some(1000),
                session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
                model: None,
            },
        ]
    );
}

#[test]
fn test_claude_pre_tool_use() {
    let path = fixtures_dir().join("hooks/claude/PreToolUse.json");
    let content = fs::read_to_string(&path).expect("read PreToolUse.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolBegin {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            tool_use_id: "toolu_01A2B3C4D5E6F7G8H9I0".to_string(),
            tool_name: "Bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo test --workspace".to_string(),
                cwd: None,
            },
        }]
    );
}

#[test]
fn test_claude_post_tool_use() {
    let path = fixtures_dir().join("hooks/claude/PostToolUse.json");
    let content = fs::read_to_string(&path).expect("read PostToolUse.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let stdout_len = "test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        .len() as u64;

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            tool_use_id: "toolu_01A2B3C4D5E6F7G8H9I0".to_string(),
            error: false,
            result_bytes: Some(stdout_len),
            exit_code: Some(0),
        }]
    );
}

#[test]
fn test_claude_stop() {
    let path = fixtures_dir().join("hooks/claude/Stop.json");
    let content = fs::read_to_string(&path).expect("read Stop.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::TurnEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            text_bytes: None,
            cost_micros: None,
        }]
    );
}

#[test]
fn test_claude_subagent_stop() {
    let path = fixtures_dir().join("hooks/claude/SubagentStop.json");
    let content = fs::read_to_string(&path).expect("read SubagentStop.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "subagent_98fbc12"),
            exit_code: Some(0),
            reason: Some("task_complete".to_string()),
        }]
    );
}

#[test]
fn test_claude_pre_compact() {
    let path = fixtures_dir().join("hooks/claude/PreCompact.json");
    let content = fs::read_to_string(&path).expect("read PreCompact.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::Compacted {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            tokens_before: None,
            tokens_after: None,
            bytes_compacted: None,
        }]
    );
}

#[test]
fn test_claude_session_end() {
    let path = fixtures_dir().join("hooks/claude/SessionEnd.json");
    let content = fs::read_to_string(&path).expect("read SessionEnd.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            exit_code: Some(0),
            reason: Some("user_exit".to_string()),
        }]
    );
}

#[test]
fn test_claude_notification() {
    let path = fixtures_dir().join("hooks/claude/Notification.json");
    let content = fs::read_to_string(&path).expect("read Notification.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::claude::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::Wait {
            source_ts_ns: Some(1000),
            session: SessionKey::new("claude", "7ab5f92a-5551-4941-a820-53a43c8ebeb9"),
            target: Some("permission_prompt".to_string()),
            on: Some(WaitOn::Input),
        }]
    );
}

// ---------------------------------------------------------------------------
// Gemini Hook Fixture Tests
// ---------------------------------------------------------------------------

#[test]
fn test_gemini_session_start() {
    let path = fixtures_dir().join("hooks/gemini/SessionStart.json");
    let content = fs::read_to_string(&path).expect("read Gemini SessionStart.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunStart {
            source_ts_ns: Some(1000),
            provider: "gemini".to_string(),
            session_id: "gemini-session-987123".to_string(),
            agent: Some("agent-1".to_string()),
            task: Some("task-1".to_string()),
            pid: Some(1234),
            cwd: Some("/workspace".to_string()),
            model: Some("gemini-1.5-pro".to_string()),
            parent_session: None,
        }]
    );
}

#[test]
fn test_gemini_before_agent() {
    let path = fixtures_dir().join("hooks/gemini/BeforeAgent.json");
    let content = fs::read_to_string(&path).expect("read Gemini BeforeAgent.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let prompt = "Investigate regression in benchmark";
    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![
            Input::Prompt {
                source_ts_ns: Some(1000),
                session: SessionKey::new("gemini", "gemini-session-987123"),
                text_len: prompt.len() as u64,
                digest: Some(blake3_digest_8(prompt)),
            },
            Input::TurnStart {
                source_ts_ns: Some(1000),
                session: SessionKey::new("gemini", "gemini-session-987123"),
                model: None,
            },
        ]
    );
}

#[test]
fn test_gemini_before_tool() {
    let path = fixtures_dir().join("hooks/gemini/BeforeTool.json");
    let content = fs::read_to_string(&path).expect("read Gemini BeforeTool.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolBegin {
            source_ts_ns: Some(1000),
            session: SessionKey::new("gemini", "gemini-session-987123"),
            tool_use_id: "run_shell_command".to_string(),
            tool_name: "run_shell_command".to_string(),
            input: ToolInput::Shell {
                argv: "pytest tests/".to_string(),
                cwd: None,
            },
        }]
    );
}

#[test]
fn test_gemini_after_tool() {
    let path = fixtures_dir().join("hooks/gemini/AfterTool.json");
    let content = fs::read_to_string(&path).expect("read Gemini AfterTool.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let res_len = "=================== 25 passed in 1.42s ===================".len() as u64;
    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("gemini", "gemini-session-987123"),
            tool_use_id: "run_shell_command".to_string(),
            error: false,
            result_bytes: Some(res_len),
            exit_code: Some(0),
        }]
    );
}

#[test]
fn test_gemini_after_agent() {
    let path = fixtures_dir().join("hooks/gemini/AfterAgent.json");
    let content = fs::read_to_string(&path).expect("read Gemini AfterAgent.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::TurnEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("gemini", "gemini-session-987123"),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            text_bytes: None,
            cost_micros: None,
        }]
    );
}

#[test]
fn test_gemini_session_end() {
    let path = fixtures_dir().join("hooks/gemini/SessionEnd.json");
    let content = fs::read_to_string(&path).expect("read Gemini SessionEnd.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::gemini::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("gemini", "gemini-session-987123"),
            exit_code: Some(0),
            reason: Some("exit".to_string()),
        }]
    );
}

// ---------------------------------------------------------------------------
// Jetski Hook Fixture Tests
// ---------------------------------------------------------------------------

#[test]
fn test_jetski_session_start() {
    let path = fixtures_dir().join("hooks/jetski/SessionStart.json");
    let content = fs::read_to_string(&path).expect("read Jetski SessionStart.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunStart {
            source_ts_ns: Some(1000),
            provider: "jetski".to_string(),
            session_id: "ec33ebf9-0cba-4100-8142-c61503f6c587".to_string(),
            agent: Some("agent-1".to_string()),
            task: Some("task-1".to_string()),
            pid: Some(1234),
            cwd: Some("/google/src/cloud/zezeozue/workspace/google3".to_string()),
            model: Some("auto".to_string()),
            parent_session: None,
        }]
    );
}

#[test]
fn test_jetski_pre_invocation() {
    let path = fixtures_dir().join("hooks/jetski/PreInvocation.json");
    let content = fs::read_to_string(&path).expect("read Jetski PreInvocation.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::TurnStart {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            model: Some("auto".to_string()),
        }]
    );
}

#[test]
fn test_jetski_pre_tool_use() {
    let path = fixtures_dir().join("hooks/jetski/PreToolUse.json");
    let content = fs::read_to_string(&path).expect("read Jetski PreToolUse.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolBegin {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            tool_use_id: "step-19".to_string(),
            tool_name: "run_command".to_string(),
            input: ToolInput::Shell {
                argv: "blaze test //my/project:test".to_string(),
                cwd: None,
            },
        }]
    );
}

#[test]
fn test_jetski_post_tool_use() {
    let path = fixtures_dir().join("hooks/jetski/PostToolUse.json");
    let content = fs::read_to_string(&path).expect("read Jetski PostToolUse.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let res_len = "//my/project:test PASSED in 4.2s\n1 test passed".len() as u64;

    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::ToolEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            tool_use_id: "step-19".to_string(),
            error: false,
            result_bytes: Some(res_len),
            exit_code: None,
        }]
    );
}

#[test]
fn test_jetski_post_invocation() {
    let path = fixtures_dir().join("hooks/jetski/PostInvocation.json");
    let content = fs::read_to_string(&path).expect("read Jetski PostInvocation.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    // The raw fixture PostInvocation.json without hook_event_name is byte-for-byte identical to PreInvocation.json
    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::TurnStart {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            model: Some("auto".to_string()),
        }]
    );

    // When tagged with hook_event_name: "PostInvocation", it produces TurnEnd:
    let mut val_named = val.clone();
    val_named["hook_event_name"] = serde_json::json!("PostInvocation");
    let inputs_named = tacho_ingest::hooks::jetski::map(&val_named, &test_env());
    assert_eq!(
        inputs_named,
        vec![Input::TurnEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            text_bytes: None,
            cost_micros: None,
        }]
    );
}

#[test]
fn test_jetski_stop() {
    let path = fixtures_dir().join("hooks/jetski/Stop.json");
    let content = fs::read_to_string(&path).expect("read Jetski Stop.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let inputs = tacho_ingest::hooks::jetski::map(&val, &test_env());
    assert_eq!(
        inputs,
        vec![Input::RunEnd {
            source_ts_ns: Some(1000),
            session: SessionKey::new("jetski", "ec33ebf9-0cba-4100-8142-c61503f6c587"),
            exit_code: Some(0),
            reason: Some("NO_TOOL_CALL".to_string()),
        }]
    );
}

// ---------------------------------------------------------------------------
// Stream Fixture Tests
// ---------------------------------------------------------------------------

#[test]
fn test_claude_stream_fixture() {
    let path = fixtures_dir().join("streams/claude-stream.jsonl");
    let content = fs::read_to_string(&path).expect("read claude-stream.jsonl");
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(detect(&lines), Some(StreamKind::Claude));

    let mut parser = StreamParser::for_kind(StreamKind::Claude);
    let mut all_inputs = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let ts = (i as u64 + 1) * 1000;
        let mut inputs = parser.feed_line(line, ts);
        all_inputs.append(&mut inputs);
    }

    assert_eq!(all_inputs.len(), 6);
    // Line 0: init -> RunStart
    assert!(
        matches!(&all_inputs[0], Input::RunStart { provider, session_id, .. } if provider == "claude" && session_id == "7ab5f92a-5551-4941-a820-53a43c8ebeb9")
    );
    // Line 1: assistant text -> TurnStart
    assert!(matches!(&all_inputs[1], Input::TurnStart { .. }));
    // Line 2: assistant tool_use -> ToolBegin
    assert!(
        matches!(&all_inputs[2], Input::ToolBegin { tool_use_id, tool_name, .. } if tool_use_id == "toolu_01" && tool_name == "Bash")
    );
    // Line 3: user tool_result -> ToolEnd
    assert!(
        matches!(&all_inputs[3], Input::ToolEnd { tool_use_id, error, .. } if tool_use_id == "toolu_01" && !error)
    );
    // Line 5: result -> TurnEnd and RunEnd
    assert!(matches!(
        &all_inputs[4],
        Input::TurnEnd {
            input_tokens: Some(1250),
            output_tokens: Some(340),
            cached_tokens: Some(9650),
            cost_micros: Some(35056),
            ..
        }
    ));
    assert!(
        matches!(&all_inputs[5], Input::RunEnd { session, exit_code: Some(0), .. } if session.provider == "claude" && session.session_id == "7ab5f92a-5551-4941-a820-53a43c8ebeb9")
    );
}

#[test]
fn test_gemini_stream_fixture() {
    let path = fixtures_dir().join("streams/gemini-stream.jsonl");
    let content = fs::read_to_string(&path).expect("read gemini-stream.jsonl");
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(detect(&lines), Some(StreamKind::Gemini));

    let mut parser = StreamParser::for_kind(StreamKind::Gemini);
    let mut all_inputs = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let ts = (i as u64 + 1) * 1000;
        let mut inputs = parser.feed_line(line, ts);
        all_inputs.append(&mut inputs);
    }

    // Line 0: init -> RunStart
    assert!(
        matches!(&all_inputs[0], Input::RunStart { provider, session_id, .. } if provider == "gemini" && session_id == "gemini-session-987123")
    );
    // Line 1: user message -> Prompt, TurnStart
    assert!(matches!(&all_inputs[1], Input::Prompt { .. }));
    assert!(matches!(&all_inputs[2], Input::TurnStart { .. }));
    // Line 2: tool_use -> ToolBegin
    assert!(
        matches!(&all_inputs[3], Input::ToolBegin { tool_name, .. } if tool_name == "run_shell_command")
    );
    // Line 3: tool_result -> ToolEnd
    assert!(matches!(&all_inputs[4], Input::ToolEnd { .. }));
    // Line 5: result -> TurnEnd, RunEnd
    assert!(matches!(
        &all_inputs[5],
        Input::TurnEnd {
            input_tokens: Some(1820),
            output_tokens: Some(520),
            ..
        }
    ));
    assert!(matches!(&all_inputs[6], Input::RunEnd { .. }));
}

#[test]
fn test_codex_stream_fixture() {
    let path = fixtures_dir().join("streams/codex-exec.jsonl");
    let content = fs::read_to_string(&path).expect("read codex-exec.jsonl");
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(detect(&lines), Some(StreamKind::Codex));

    let mut parser = StreamParser::for_kind(StreamKind::Codex);
    let mut all_inputs = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let ts = (i as u64 + 1) * 1000;
        let mut inputs = parser.feed_line(line, ts);
        all_inputs.append(&mut inputs);
    }

    assert_eq!(all_inputs.len(), 5);
    // Line 0: thread.started -> RunStart
    assert!(
        matches!(&all_inputs[0], Input::RunStart { provider, session_id, .. } if provider == "codex" && session_id == "01931a2c-7b44-7000-8000-000000000001")
    );
    // Line 1: turn.started -> TurnStart
    assert!(matches!(&all_inputs[1], Input::TurnStart { .. }));
    // Line 3: command_execution -> ToolBegin, ToolEnd
    assert!(
        matches!(&all_inputs[2], Input::ToolBegin { tool_use_id, .. } if tool_use_id == "item_02")
    );
    assert!(
        matches!(&all_inputs[3], Input::ToolEnd { tool_use_id, exit_code: Some(0), .. } if tool_use_id == "item_02")
    );
    // Line 4: turn.completed -> TurnEnd
    assert!(matches!(
        &all_inputs[4],
        Input::TurnEnd {
            input_tokens: Some(1420),
            output_tokens: Some(85),
            cached_tokens: Some(1024),
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Sequence Fixture Tests (Well-formed begin/end pairs, parallel tools)
// ---------------------------------------------------------------------------

#[test]
fn test_claude_parallel_tools_sequence() {
    let path = fixtures_dir().join("sequences/claude-parallel-tools.jsonl");
    let content = fs::read_to_string(&path).expect("read claude-parallel-tools.jsonl");

    let mut open_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut open_turns = 0i32;

    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let env = HookEnv {
            agent: None,
            task: None,
            invoked_at_ns: (i as u64 + 1) * 1000,
            ppid: None,
        };
        let inputs = tacho_ingest::hooks::claude::map(&val, &env);

        for inp in inputs {
            match inp {
                Input::TurnStart { .. } => {
                    open_turns += 1;
                }
                Input::TurnEnd { .. } => {
                    open_turns -= 1;
                }
                Input::ToolBegin { tool_use_id, .. } => {
                    assert!(
                        open_tools.insert(tool_use_id.clone()),
                        "ToolBegin for already open tool: {}",
                        tool_use_id
                    );
                }
                Input::ToolEnd { tool_use_id, .. } => {
                    assert!(
                        open_tools.remove(&tool_use_id),
                        "ToolEnd for unopened tool: {}",
                        tool_use_id
                    );
                }
                _ => {}
            }
        }
    }

    // Parallel tool calls (View and Grep) and subagent tools must all be closed!
    assert!(
        open_tools.is_empty(),
        "All tool begin/end calls must pair cleanly. Unclosed: {:?}",
        open_tools
    );
    assert_eq!(open_turns, 0, "TurnStart must pair with TurnEnd");
}

#[test]
fn test_jetski_session_sequence() {
    let path = fixtures_dir().join("sequences/jetski-session.jsonl");
    let content = fs::read_to_string(&path).expect("read jetski-session.jsonl");

    let mut open_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut open_turns = 0i32;
    let mut open_runs = 0i32;

    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let env = HookEnv {
            agent: None,
            task: None,
            invoked_at_ns: (i as u64 + 1) * 1000,
            ppid: None,
        };
        let inputs = tacho_ingest::hooks::jetski::map(&val, &env);

        for inp in inputs {
            match inp {
                Input::RunStart { .. } => {
                    open_runs += 1;
                }
                Input::RunEnd { .. } => {
                    open_runs -= 1;
                }
                Input::TurnStart { .. } => {
                    open_turns += 1;
                }
                Input::TurnEnd { .. } => {
                    open_turns -= 1;
                }
                Input::ToolBegin { tool_use_id, .. } => {
                    assert!(
                        open_tools.insert(tool_use_id.clone()),
                        "ToolBegin for already open tool: {}",
                        tool_use_id
                    );
                }
                Input::ToolEnd { tool_use_id, .. } => {
                    assert!(
                        open_tools.remove(&tool_use_id),
                        "ToolEnd for unopened tool: {}",
                        tool_use_id
                    );
                }
                _ => {}
            }
        }
    }

    assert_eq!(open_runs, 0, "RunStart must pair with RunEnd");
    assert_eq!(open_turns, 0, "TurnStart must pair with TurnEnd");
    assert!(
        open_tools.is_empty(),
        "All tool begin/end calls must pair cleanly"
    );
}

#[test]
fn test_classify_multiedit_line_counts() {
    let input = serde_json::json!({
        "file_path": "a.rs",
        "edits": [{"old_string": "foo\n", "new_string": "bar\nbaz\n"}]
    });
    let classified = tacho_ingest::hooks::classify_tool("MultiEdit", &input);
    assert_eq!(
        classified,
        tacho_core::ToolInput::FileEdit {
            path: "a.rs".to_string(),
            kind: Some(tacho_core::FileEditKind::Edit),
            lines_added: Some(2),
            lines_removed: Some(1),
        }
    );
}

#[test]
fn test_classify_ls_and_list_dir() {
    let input = serde_json::json!({"path": "src"});
    let classified = tacho_ingest::hooks::classify_tool("LS", &input);
    assert_eq!(
        classified,
        tacho_core::ToolInput::FileSearch {
            pattern: "".to_string(),
            path: Some("src".to_string()),
        }
    );

    let input_jetski = serde_json::json!({"dir_path": "crates/"});
    let classified_jetski = tacho_ingest::hooks::classify_tool("list_dir", &input_jetski);
    assert_eq!(
        classified_jetski,
        tacho_core::ToolInput::FileSearch {
            pattern: "".to_string(),
            path: Some("crates/".to_string()),
        }
    );
}

#[test]
fn test_classify_notebook_edit() {
    let input = serde_json::json!({"notebook_path": "nb.ipynb"});
    let classified = tacho_ingest::hooks::classify_tool("NotebookEdit", &input);
    assert_eq!(
        classified,
        tacho_core::ToolInput::FileEdit {
            path: "nb.ipynb".to_string(),
            kind: Some(tacho_core::FileEditKind::Edit),
            lines_added: None,
            lines_removed: None,
        }
    );
}

#[test]
fn test_classify_jetski_tools() {
    let repl = serde_json::json!({
        "path": "src/lib.rs",
        "target": "old_code\n",
        "replacement": "new_code_1\nnew_code_2\n"
    });
    assert_eq!(
        tacho_ingest::hooks::classify_tool("replace_file_content", &repl),
        tacho_core::ToolInput::FileEdit {
            path: "src/lib.rs".to_string(),
            kind: Some(tacho_core::FileEditKind::Edit),
            lines_added: Some(2),
            lines_removed: Some(1),
        }
    );

    let wr = serde_json::json!({
        "path": "src/new.rs",
        "content": "line 1\nline 2\nline 3\n"
    });
    assert_eq!(
        tacho_ingest::hooks::classify_tool("write_to_file", &wr),
        tacho_core::ToolInput::FileEdit {
            path: "src/new.rs".to_string(),
            kind: Some(tacho_core::FileEditKind::Create),
            lines_added: Some(3),
            lines_removed: Some(0),
        }
    );

    let grp = serde_json::json!({
        "query": "struct StreamParser",
        "dir_path": "crates/tacho-ingest"
    });
    assert_eq!(
        tacho_ingest::hooks::classify_tool("grep_search", &grp),
        tacho_core::ToolInput::FileSearch {
            pattern: "struct StreamParser".to_string(),
            path: Some("crates/tacho-ingest".to_string()),
        }
    );
}

#[test]
fn test_jetski_post_tool_use_exit_status_error() {
    let payload = serde_json::json!({
        "conversationId": "jetski-test-1",
        "hook_event_name": "PostToolUse",
        "stepIdx": 5,
        "toolCall": {
            "name": "run_command",
            "args": {"CommandLine": "cargo test"}
        },
        "error": "exit status 101",
        "result": "test failed"
    });
    let env = test_env();
    let inputs = tacho_ingest::hooks::jetski::map(&payload, &env);
    assert_eq!(inputs.len(), 1);
    match &inputs[0] {
        Input::ToolEnd {
            error, exit_code, ..
        } => {
            assert!(*error);
            assert_eq!(*exit_code, Some(101));
        }
        other => panic!("expected ToolEnd, got {:?}", other),
    }
}

#[test]
fn test_stream_partial_lines_buffering() {
    let mut parser = tacho_ingest::streams::claude::StreamParser::new();
    let r1 = parser.feed_line(
        "{\"type\": \"assistant\", \"session_id\": \"sess-1\", \"message\": {\"role\": \"assistant\", ",
        100,
    );
    assert!(r1.is_empty());
    let r2 = parser.feed_line(
        "\"content\": [{\"type\": \"text\", \"text\": \"hello\"}]}}\n",
        200,
    );
    assert_eq!(r2.len(), 1);
    assert!(matches!(&r2[0], Input::TurnStart { .. }));
}

#[test]
fn test_claude_multi_turn_stream() {
    let mut parser = tacho_ingest::streams::claude::StreamParser::new();
    parser.feed_line(
        "{\"type\": \"system\", \"subtype\": \"init\", \"session_id\": \"sess-mt\", \"cwd\": \"/\", \"model\": \"claude-3\"}\n",
        100,
    );
    let p1 = parser.feed_line(
        "{\"type\": \"user\", \"session_id\": \"sess-mt\", \"message\": {\"role\": \"user\", \"content\": [{\"type\": \"text\", \"text\": \"First prompt\"}]}}\n",
        200,
    );
    assert_eq!(p1.len(), 2);
    assert!(matches!(&p1[0], Input::Prompt { text_len: 12, .. }));
    assert!(matches!(&p1[1], Input::TurnStart { .. }));

    parser.feed_line(
        "{\"type\": \"assistant\", \"session_id\": \"sess-mt\", \"message\": {\"role\": \"assistant\", \"content\": [{\"type\": \"text\", \"text\": \"Response 1\"}], \"usage\": {\"input_tokens\": 10, \"output_tokens\": 5}}}\n",
        300,
    );

    let p2 = parser.feed_line(
        "{\"type\": \"user\", \"session_id\": \"sess-mt\", \"message\": {\"role\": \"user\", \"content\": [{\"type\": \"text\", \"text\": \"Second prompt\"}]}}\n",
        400,
    );
    assert_eq!(p2.len(), 3);
    assert!(matches!(
        &p2[0],
        Input::TurnEnd {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..
        }
    ));
    assert!(matches!(&p2[1], Input::Prompt { text_len: 13, .. }));
    assert!(matches!(&p2[2], Input::TurnStart { .. }));
}

#[test]
fn test_gemini_stream_usage_metadata() {
    let mut parser = tacho_ingest::streams::gemini::StreamParser::new();
    parser.feed_line(
        "{\"type\": \"init\", \"session_id\": \"gem-1\", \"model\": \"gemini-1.5\"}\n",
        100,
    );
    parser.feed_line(
        "{\"type\": \"message\", \"session_id\": \"gem-1\", \"role\": \"user\", \"content\": \"Hi\"}\n",
        200,
    );
    parser.feed_line(
        "{\"type\": \"message\", \"session_id\": \"gem-1\", \"role\": \"assistant\", \"content\": \"Hello\", \"usageMetadata\": {\"promptTokenCount\": 45, \"candidatesTokenCount\": 12, \"cachedContentTokenCount\": 30}}\n",
        300,
    );
    let res = parser.feed_line(
        "{\"type\": \"result\", \"session_id\": \"gem-1\", \"status\": \"success\"}\n",
        400,
    );
    assert_eq!(res.len(), 2);
    match &res[0] {
        Input::TurnEnd {
            input_tokens,
            output_tokens,
            cached_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, Some(45));
            assert_eq!(*output_tokens, Some(12));
            assert_eq!(*cached_tokens, Some(30));
        }
        other => panic!("expected TurnEnd, got {:?}", other),
    }
}
