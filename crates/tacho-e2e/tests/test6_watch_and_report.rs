use std::io::Write;
use std::process::{Command, Stdio};
use tacho_e2e::TestEnv;

#[test]
fn test_watch_until_stalled_and_report() {
    let env = TestEnv::new().unwrap();

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    // 1. Launch `tacho watch --until stalled` in a separate process
    let watch_child = env
        .tacho_cmd()
        .args(["watch", "--until", "stalled"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tacho watch");

    // 2. Induce a LOOP by repeating identical tool call inputs 4+ times on agent `loop-agent`
    for i in 1..=5 {
        let payload = serde_json::json!({
            "session_id": "session-loop-agent",
            "cwd": "/workspace",
            "hook_event_name": "PreToolUse",
            "tool_use_id": format!("toolu_loop_{i}"),
            "tool_name": "Grep",
            "tool_input": {
                "pattern": "IDENTICAL_LOOP_PATTERN",
                "path": "src/"
            }
        });

        let mut child = env
            .tacho_cmd()
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        assert!(child.wait().unwrap().success());
    }

    // 3. `tacho watch --until stalled` must terminate once the stalled state is reached
    let watch_output = watch_child.wait_with_output().unwrap();
    assert!(watch_output.status.success());
    let watch_stdout = String::from_utf8_lossy(&watch_output.stdout);
    assert!(
        watch_stdout.contains("Target state reached: stalled")
            || watch_stdout.contains("stalled")
            || watch_stdout.contains("LOOP"),
        "watch output did not indicate stalled state: {watch_stdout}"
    );

    // 4. Verify tacho report content for the agent
    let status_out = env
        .tacho_cmd()
        .args(["status", "--json"])
        .output()
        .expect("tacho status failed");
    let status_val: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    let agent_id = status_val
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|obj| obj.get("agent"))
        .and_then(|v| v.as_str())
        .expect("agent should exist in status");

    let report_out = env
        .tacho_cmd()
        .args(["report", agent_id])
        .output()
        .expect("tacho report failed");
    assert!(report_out.status.success());

    let report_str = String::from_utf8_lossy(&report_out.stdout);
    assert!(
        report_str.contains(agent_id),
        "Report does not mention agent: {report_str}"
    );
    assert!(
        report_str.contains("Timeline"),
        "Report does not include timeline: {report_str}"
    );
}

#[test]
fn test_report_diff_claims() {
    let env = TestEnv::new().unwrap();

    let worktree_dir = env.home_dir.join("worktree");
    std::fs::create_dir_all(&worktree_dir).unwrap();

    // Initialize git repository in test worktree so git status checks pass cleanly
    Command::new("git")
        .args(["init"])
        .current_dir(&worktree_dir)
        .output()
        .expect("git init failed");
    Command::new("git")
        .args(["config", "user.name", "Tacho Test"])
        .current_dir(&worktree_dir)
        .output()
        .expect("git config user.name failed");
    Command::new("git")
        .args(["config", "user.email", "tacho@test.local"])
        .current_dir(&worktree_dir)
        .output()
        .expect("git config user.email failed");
    let init_file = worktree_dir.join("init.txt");
    std::fs::write(&init_file, "init\n").unwrap();
    Command::new("git")
        .args(["add", "init.txt"])
        .current_dir(&worktree_dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&worktree_dir)
        .output()
        .unwrap();

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    let send_hook = |payload: serde_json::Value| {
        let mut child = env
            .tacho_cmd()
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        assert!(child.wait().unwrap().success());
    };

    // 1. Session start for agent `verified-worker`
    send_hook(serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-verified",
        "agent": "verified-worker",
        "cwd": worktree_dir.to_str().unwrap(),
    }));

    // 2. File edit
    send_hook(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-edit-1",
        "tool_name": "Edit",
        "tool_input": { "path": "src/lib.rs" },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-edit-1",
        "tool_response": { "exit_code": 0 },
    }));

    // 3. Passing test after edit
    send_hook(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-test-1",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-test-1",
        "tool_response": { "exit_code": 0 },
    }));

    // 4. Successful commit and push
    send_hook(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-commit-1",
        "tool_name": "Bash",
        "tool_input": { "command": "git commit -m 'feat'" },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-commit-1",
        "tool_response": { "exit_code": 0 },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-push-1",
        "tool_name": "Bash",
        "tool_input": { "command": "git push" },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-verified",
        "tool_use_id": "tool-push-1",
        "tool_response": { "exit_code": 0 },
    }));

    // 5. Test `tacho report verified-worker --diff-claims`
    let report_out = env
        .tacho_cmd()
        .args(["report", "verified-worker", "--diff-claims"])
        .output()
        .expect("tacho report --diff-claims failed");
    assert!(report_out.status.success());

    let report_str = String::from_utf8_lossy(&report_out.stdout);
    println!("Report diff-claims output:\n{}", report_str);

    assert!(report_str.contains("=== Claim Verification: verified-worker ==="));
    assert!(report_str.contains("Test After Last Edit: VERIFIED"));
    assert!(report_str.contains("Commits Succeeded:    1"));
    assert!(report_str.contains("Pushes Succeeded:     1"));
    assert!(report_str.contains("Claim Stall Flags:    NONE"));
    assert!(report_str.contains("Git Worktree Status:  CLEAN"));

    // Verify brevity (< 100 tokens, approx 400 chars)
    assert!(
        report_str.len() < 500,
        "Report summary is too long (expected <100 tokens): {} bytes",
        report_str.len()
    );

    // 6. Test unverified worker: edits without subsequent passing test
    send_hook(serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-unverified",
        "agent": "unverified-worker",
        "cwd": env.home_dir.to_str().unwrap(),
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-unverified",
        "tool_use_id": "tool-edit-bad",
        "tool_name": "Edit",
        "tool_input": { "path": "src/main.rs" },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-unverified",
        "tool_use_id": "tool-edit-bad",
        "tool_response": { "exit_code": 0 },
    }));
    send_hook(serde_json::json!({
        "hook_event_name": "SessionEnd",
        "session_id": "sess-unverified",
        "reason": "done",
    }));

    assert!(env.flush());

    let unverified_out = env
        .tacho_cmd()
        .args(["report", "unverified-worker", "--diff-claims"])
        .output()
        .expect("tacho report --diff-claims failed");
    assert!(unverified_out.status.success());
    let unverified_str = String::from_utf8_lossy(&unverified_out.stdout);
    println!("Unverified output:\n{}", unverified_str);

    assert!(unverified_str.contains("=== Claim Verification: unverified-worker ==="));
    assert!(unverified_str.contains("Test After Last Edit: UNVERIFIED"));
}
