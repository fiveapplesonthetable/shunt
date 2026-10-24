use std::io::Write;
use std::process::Stdio;
use tacho_e2e::TestEnv;

#[test]
fn test_daemon_restart_lifecycle() {
    let env = TestEnv::new().unwrap();

    // 1. Ensure daemon is running
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    let initial_pid = env.daemon_pid().expect("daemon PID must be discoverable");
    assert!(TestEnv::is_pid_alive(initial_pid));

    // 2. Start agent session with hook events
    let payload_1 = serde_json::json!({
        "session_id": "session-restart-test",
        "cwd": "/workspace",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_before_kill",
        "tool_name": "View",
        "tool_input": { "file_path": "src/lib.rs" }
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
        stdin.write_all(payload_1.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    assert!(child.wait().unwrap().success());

    // Flush batch to disk
    assert!(env.flush());

    // 3. SIGKILL daemon mid-session
    assert!(env.kill_daemon_9(), "failed to send SIGKILL to daemon");
    let dead = TestEnv::wait_pid_exit(initial_pid);
    assert!(dead, "daemon process should be dead after SIGKILL");

    // 4. tacho trace while daemon is dead (reads DB read-only directly)
    let fallback_trace = env
        .home_dir
        .join("trace/fallback_after_kill.perfetto-trace");
    let fallback_out = env
        .tacho_cmd()
        .args(["trace", "-o", fallback_trace.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(
        fallback_out.status.success(),
        "tacho trace should succeed even when daemon is dead: {}",
        String::from_utf8_lossy(&fallback_out.stderr)
    );
    assert!(fallback_trace.exists());

    let fallback_oracle = env.run_oracle(&fallback_trace).unwrap();
    assert!(
        fallback_oracle.error_stats.is_empty(),
        "Fallback trace oracle errors: {:?}",
        fallback_oracle.error_stats
    );
    assert!(fallback_oracle.slice_count > 0);

    // 5. Next hook auto-spawns daemon
    let payload_2 = serde_json::json!({
        "session_id": "session-restart-test",
        "cwd": "/workspace",
        "hook_event_name": "PostToolUse",
        "tool_use_id": "toolu_before_kill",
        "tool_name": "View",
        "tool_input": { "file_path": "src/lib.rs" },
        "tool_response": { "exit_code": 0, "content": "pub fn test() {}\n" }
    });

    let mut spawn_child = env
        .tacho_cmd()
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(mut stdin) = spawn_child.stdin.take() {
        stdin.write_all(payload_2.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    assert!(spawn_child.wait().unwrap().success());

    // Wait for auto-spawned daemon to be responding on api.sock
    assert!(
        env.wait_for_api_sock(),
        "auto-spawned daemon failed to start"
    );

    let new_pid = env.daemon_pid().expect("new daemon PID should be found");
    assert_ne!(initial_pid, new_pid, "new daemon must have a different PID");
    assert!(TestEnv::is_pid_alive(new_pid));

    // 6. Session continues on same tracks
    let payload_3 = serde_json::json!({
        "session_id": "session-restart-test",
        "cwd": "/workspace",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_after_restart",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" }
    });
    let mut cont_child = env
        .tacho_cmd()
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(mut stdin) = cont_child.stdin.take() {
        stdin.write_all(payload_3.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    assert!(cont_child.wait().unwrap().success());

    assert!(env.flush());

    let cont_trace = env.home_dir.join("trace/continued.perfetto-trace");
    let cont_out = env
        .tacho_cmd()
        .args(["trace", "-o", cont_trace.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(cont_out.success());

    let cont_oracle = env.run_oracle(&cont_trace).unwrap();
    assert!(cont_oracle.error_stats.is_empty());
    assert!(cont_oracle.slice_count >= 2);

    // 7. SIGTERM writes last.perfetto-trace
    assert!(env.kill_daemon_term(), "failed to send SIGTERM to daemon");
    let term_dead = TestEnv::wait_pid_exit(new_pid);
    assert!(term_dead, "daemon should exit cleanly after SIGTERM");

    let last_trace = env.home_dir.join("trace/last.perfetto-trace");
    // The daemon writes last.perfetto-trace during SIGTERM shutdown, before it
    // exits, so once the pidfd reported exit the file must be complete.
    let wait_last = std::fs::metadata(&last_trace)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    assert!(
        wait_last,
        "last.perfetto-trace was not written upon SIGTERM"
    );

    let last_oracle = env.run_oracle(&last_trace).unwrap();
    assert!(
        last_oracle.error_stats.is_empty(),
        "last.perfetto-trace oracle reported errors: {:?}",
        last_oracle.error_stats
    );
    assert!(last_oracle.slice_count > 0);
}
