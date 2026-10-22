use std::fs;
use tacho_e2e::{fixtures_dir, TestEnv};

fn run_stream_fixture(env: &TestEnv, fixture_name: &str) {
    let fixture_path = fixtures_dir().join(format!("streams/{fixture_name}"));
    let fixture_bytes = fs::read(&fixture_path).expect("failed to read stream fixture");

    // Create a helper shell script that prints the fixture lines with small sleeps
    let script_path = env
        .temp_dir
        .path()
        .join(format!("stream_{fixture_name}.sh"));
    let script_content = format!(
        r#"#!/usr/bin/env bash
while IFS= read -r line || [ -n "$line" ]; do
    printf "%s\n" "$line"
    sleep 0.005
done < "{}"
"#,
        fixture_path.display()
    );
    fs::write(&script_path, script_content).unwrap();

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(std::time::Duration::from_secs(3)));

    // Execute tacho run -- bash <script>
    let output = env
        .tacho_cmd()
        .args(["run", "--", "bash", script_path.to_str().unwrap()])
        .output()
        .expect("tacho run failed to execute");

    assert!(
        output.status.success(),
        "tacho run failed: {:?}",
        output.status
    );

    // Hard constraint: stdout must be byte-identical to the raw fixture
    assert_eq!(
        output.stdout, fixture_bytes,
        "stdout was not byte-identical to fixture {fixture_name}"
    );

    // Wait briefly for appender to flush batch
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Snapshot trace
    let trace_path = env
        .home_dir
        .join(format!("trace/{fixture_name}.perfetto-trace"));
    let trace_status = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .status()
        .expect("tacho trace failed");
    assert!(trace_status.success());
    assert!(trace_path.exists());

    // Oracle verification
    let oracle = env.run_oracle(&trace_path).expect("oracle query failed");
    assert!(
        oracle.error_stats.is_empty(),
        "Oracle reported errors: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0, "Expected slices from stream");

    // Verify Run.pid is recorded
    let pid_rows = env
        .query_trace(
            &trace_path,
            "SELECT count(*) FROM args WHERE key LIKE '%pid%' AND int_value IS NOT NULL AND int_value > 0;",
        )
        .unwrap();
    let pid_count = pid_rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        pid_count > 0,
        "Expected Run.pid arg to be recorded in trace"
    );
}

#[test]
fn test_stream_claude() {
    let env = TestEnv::new().unwrap();
    run_stream_fixture(&env, "claude-stream.jsonl");
}

#[test]
fn test_stream_gemini() {
    let env = TestEnv::new().unwrap();
    run_stream_fixture(&env, "gemini-stream.jsonl");
}

#[test]
fn test_stream_codex() {
    let env = TestEnv::new().unwrap();
    run_stream_fixture(&env, "codex-exec.jsonl");
}
