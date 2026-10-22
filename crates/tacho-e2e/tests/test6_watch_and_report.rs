use std::io::Write;
use std::process::Stdio;
use std::time::Duration;
use tacho_e2e::TestEnv;

#[test]
fn test_watch_until_stalled_and_report() {
    let env = TestEnv::new().unwrap();

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    // 1. Launch `tacho watch --until stalled` in a separate process
    let mut watch_child = env
        .tacho_cmd()
        .args(["watch", "--until", "stalled"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tacho watch");

    // Give watch process a moment to connect
    std::thread::sleep(Duration::from_millis(100));

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
        std::thread::sleep(Duration::from_millis(50));
    }

    // 3. `tacho watch --until stalled` must terminate within a short deadline (e.g. 5s)
    let watch_exited = env.wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        watch_child.try_wait().unwrap().is_some()
    });
    assert!(
        watch_exited,
        "`tacho watch --until stalled` did not terminate after inducing LOOP"
    );

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
