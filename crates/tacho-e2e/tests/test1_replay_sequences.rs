use std::fs;
use std::io::Write;
use std::process::Stdio;
use std::thread;
use tacho_e2e::{fixtures_dir, TestEnv};

fn replay_jsonl_sequential(env: &TestEnv, provider: &str, fixture_path: &std::path::Path) {
    let content = fs::read_to_string(fixture_path).expect("failed to read fixture");
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut child = env
            .tacho_cmd()
            .args(["hook", provider])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn tacho hook");

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(trimmed.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }

        let output = child
            .wait_with_output()
            .expect("failed to wait on tacho hook");
        assert!(
            output.status.success(),
            "tacho hook must exit 0, got {:?}",
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "tacho hook must print nothing to stdout"
        );
    }
}

fn replay_jsonl_parallel(env: &TestEnv, provider: &str, fixture_path: &std::path::Path) {
    let content = fs::read_to_string(fixture_path).expect("failed to read fixture");
    let lines: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    thread::scope(|s| {
        for chunk in lines.chunks(3) {
            let chunk_vec = chunk.to_vec();
            let provider_str = provider.to_string();

            s.spawn(move || {
                for line in chunk_vec {
                    let mut child = env
                        .tacho_cmd()
                        .args(["hook", &provider_str])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .expect("failed to spawn tacho hook in thread");

                    if let Some(mut stdin) = child.stdin.take() {
                        stdin.write_all(line.as_bytes()).unwrap();
                        stdin.write_all(b"\n").unwrap();
                    }

                    let output = child.wait_with_output().expect("wait failed");
                    assert!(output.status.success());
                    assert!(output.stdout.is_empty());
                }
            });
        }
    });
}

#[test]
fn test_replay_claude_sequential() {
    let env = TestEnv::new().unwrap();
    let claude_fixture = fixtures_dir().join("sequences/claude-parallel-tools.jsonl");

    // Pre-start daemon or let hook autospawn it
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    replay_jsonl_sequential(&env, "claude", &claude_fixture);
    assert!(env.flush());

    let trace_path = env.home_dir.join("trace/claude_seq.perfetto-trace");
    let status = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .status()
        .expect("tacho trace failed");
    assert!(status.success());
    assert!(trace_path.exists());

    let oracle = env.run_oracle(&trace_path).expect("oracle query failed");
    assert!(
        oracle.error_stats.is_empty(),
        "Oracle reported errors: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0, "Expected slices, got 0");
    assert!(oracle.track_count > 0, "Expected tracks, got 0");

    // Verify expected slices
    let slices = env
        .query_trace(&trace_path, "SELECT name FROM slice ORDER BY ts;")
        .unwrap();
    let slice_names: Vec<String> = slices.into_iter().flatten().collect();
    assert!(
        slice_names
            .iter()
            .any(|s| s.contains("View") || s.contains("Read")),
        "Expected View/Read slice in {:?}",
        slice_names
    );
}

#[test]
fn test_replay_jetski_parallel() {
    let env = TestEnv::new().unwrap();
    let jetski_fixture = fixtures_dir().join("sequences/jetski-session.jsonl");

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    replay_jsonl_parallel(&env, "jetski", &jetski_fixture);
    assert!(env.flush());

    let trace_path = env.home_dir.join("trace/jetski_par.perfetto-trace");
    let status = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .status()
        .expect("tacho trace failed");
    assert!(status.success());
    assert!(trace_path.exists());

    let oracle = env.run_oracle(&trace_path).expect("oracle query failed");
    assert!(
        oracle.error_stats.is_empty(),
        "Oracle reported errors: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0, "Expected slices, got 0");
    assert!(oracle.track_count > 0, "Expected tracks, got 0");

    // Verify state track exists
    let tracks = env
        .query_trace(&trace_path, "SELECT name FROM track WHERE name = 'state';")
        .unwrap();
    assert!(!tracks.is_empty(), "Expected state track to exist");
}
