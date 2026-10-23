use std::fs;
use std::io::Write;
use std::process::Stdio;
use tacho_e2e::TestEnv;

const CANARY_SECRETS: &[&str] = &[
    concat!("AKIA", "IOSFODNN7EXAMPLE"),
    concat!("AIza", "SyDCanary1234567890abcdef123456"),
    concat!("ghp_", "canary1234567890abcdefghijklmnopqr"),
    concat!("sk-ant-", "api03-canary_anthropic_secret_key_12345678"),
    concat!("sk-proj-", "canary_openai_secret_token_1234567890abcdef"),
    concat!("xoxb-", "1234567890-canary_slack_token_secret"),
    concat!("sk_", "live_canary_stripe_secret_key_123456789"),
    "canary_super_secret_bearer_token_string",
    "canary_super_secret_password_phrase_999",
    "a1b2c3d4e5f678901234567890abcdef1234567890abcdef1234567890abcdef",
];

fn assert_no_leaks(target_name: &str, data: &[u8]) {
    for &secret in CANARY_SECRETS {
        let needle = secret.as_bytes();
        let found = data.windows(needle.len()).any(|w| w == needle);
        assert!(
            !found,
            "CANARY LEAK DETECTED in {target_name}: secret '{secret}' found in data (length {})",
            data.len()
        );
    }
}

#[test]
fn test_canary_secrets_zero_leak() {
    let env = TestEnv::new().unwrap();
    fs::write(env.home_dir.join("config.toml"), "scrub = true\n").unwrap();

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(std::time::Duration::from_secs(3)));

    // 1. Seed secrets via tacho hook claude (SessionStart + PreToolUse + PostToolUse)
    let send_hook = |payload: serde_json::Value| {
        let mut hook_child = env
            .tacho_cmd()
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(mut stdin) = hook_child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        let hook_out = hook_child.wait_with_output().unwrap();
        assert!(hook_out.status.success());
    };

    send_hook(serde_json::json!({
        "session_id": "session-canary",
        "agent": "canary",
        "cwd": format!("/workspace/secret={}", CANARY_SECRETS[0]),
        "hook_event_name": "SessionStart",
    }));

    send_hook(serde_json::json!({
        "session_id": "session-canary",
        "cwd": "/workspace",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_canary_01",
        "tool_name": "Bash",
        "prompt": format!("Please use secret {}", CANARY_SECRETS[0]),
        "tool_input": {
            "command": format!(
                "curl -H 'Authorization: Bearer {}' https://api.com --password {} --key secret={}",
                CANARY_SECRETS[7], CANARY_SECRETS[8], CANARY_SECRETS[1]
            ),
            "secret_key": CANARY_SECRETS[1],
        },
    }));

    send_hook(serde_json::json!({
        "session_id": "session-canary",
        "cwd": "/workspace",
        "hook_event_name": "PostToolUse",
        "tool_use_id": "toolu_canary_01",
        "tool_name": "Bash",
        "tool_response": {
            "exit_code": 0,
            "content": format!("Response containing {} and {}", CANARY_SECRETS[2], CANARY_SECRETS[3]),
        }
    }));

    // 2. Seed secrets via stream lines (tacho run)
    let stream_script = env.temp_dir.path().join("canary_stream.sh");
    let stream_content = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' '{{"type":"assistant","session_id":"canary-run","message":{{"content":[{{"type":"text","text":"Token: {}"}}]}}}}'
"#,
        CANARY_SECRETS[4]
    );
    fs::write(&stream_script, stream_content).unwrap();

    let run_out = env
        .tacho_cmd()
        .args(["run", "--", "bash", stream_script.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(run_out.status.success());

    // 3. Seed secrets via tacho emit
    let emit_out = env
        .tacho_cmd()
        .args([
            "emit",
            "span",
            "begin",
            "agent:canary/custom",
            "canary-span",
            "--arg",
            &format!("key={}", CANARY_SECRETS[5]),
            "--arg",
            &format!("stripe={}", CANARY_SECRETS[6]),
            "--arg",
            &format!("hex={}", CANARY_SECRETS[9]),
        ])
        .output()
        .unwrap();
    assert!(emit_out.status.success());

    // Wait for appender to flush
    assert!(env.flush());

    // 4. Check snapshot file
    let trace_path = env.home_dir.join("trace/canary.perfetto-trace");
    let trace_status = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(trace_status.success());
    let trace_bytes = fs::read(&trace_path).unwrap();
    assert_no_leaks("trace snapshot file", &trace_bytes);

    // Verify Command argv was recorded in the trace and scrubbed to <redacted>
    let argv_rows = env
        .query_trace(
            &trace_path,
            "SELECT string_value FROM args WHERE key LIKE '%argv%';",
        )
        .unwrap();
    let argv_vals: Vec<String> = argv_rows.into_iter().flatten().collect();
    assert!(
        argv_vals.iter().any(|s| s.contains("<redacted>")),
        "Expected Command argv with <redacted> in trace args, got {:?}",
        argv_vals
    );

    // 5. Check --from-log rebuild
    let rebuild_path = env.home_dir.join("trace/canary_rebuild.perfetto-trace");
    let rebuild_status = env
        .tacho_cmd()
        .args([
            "trace",
            "--from-log",
            "10m",
            "-o",
            rebuild_path.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(rebuild_status.success());
    let rebuild_bytes = fs::read(&rebuild_path).unwrap();
    assert_no_leaks("from-log rebuild trace", &rebuild_bytes);

    // 6. Check SQLite events.db / events.sqlite raw bytes (including -wal)
    let db_path = env.events_db();
    if db_path.exists() {
        let db_bytes = fs::read(&db_path).unwrap();
        assert_no_leaks("SQLite events.db", &db_bytes);
    }
    let wal_path = db_path.with_extension("db-wal");
    if wal_path.exists() {
        let wal_bytes = fs::read(&wal_path).unwrap();
        assert_no_leaks("SQLite events.db-wal", &wal_bytes);
    }
    let sqlite_wal = env.home_dir.join("events.sqlite-wal");
    if sqlite_wal.exists() {
        let wal_bytes = fs::read(&sqlite_wal).unwrap();
        assert_no_leaks("SQLite events.sqlite-wal", &wal_bytes);
    }

    // 7. Check tacho status and status --json
    let status_plain = env.tacho_cmd().arg("status").output().unwrap();
    assert_no_leaks("tacho status text", &status_plain.stdout);
    assert_no_leaks("tacho status stderr", &status_plain.stderr);

    let status_json = env.tacho_cmd().args(["status", "--json"]).output().unwrap();
    assert_no_leaks("tacho status --json", &status_json.stdout);

    // 8. Check tacho report
    let report_out = env.tacho_cmd().args(["report", "canary"]).output().unwrap();
    assert_no_leaks("tacho report", &report_out.stdout);

    // 9. Check daemon.log
    let log_path = env.daemon_log();
    if log_path.exists() {
        let log_bytes = fs::read(&log_path).unwrap();
        assert_no_leaks("daemon.log", &log_bytes);
    }
}
