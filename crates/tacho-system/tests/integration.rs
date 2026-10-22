//! Integration tests for tacho-system: verifies unprivileged traced lifecycle,
//! background session management, live cloning, and trace validation via Trace Processor.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tacho_system::{Perfetto, Profile, SystemError};

/// Guard that ensures child daemon processes are terminated and reaped on test exit.
struct DaemonGuard {
    traced: Option<Child>,
    probes: Option<Child>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut p) = self.probes.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        if let Some(mut t) = self.traced.take() {
            let _ = t.kill();
            let _ = t.wait();
        }
    }
}

/// Resolves a path relative to Perfetto release binaries directory.
fn find_perfetto_binary(name: &str) -> PathBuf {
    if let Ok(dir) = env::var("PERFETTO_DIR") {
        let candidate = Path::new(&dir).join("out/release").join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Ok(home) = env::var("HOME") {
        let candidate = PathBuf::from(home)
            .join("dev/perfetto/out/release")
            .join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    let direct =
        PathBuf::from("/usr/local/google/home/zezeozue/dev/perfetto/out/release").join(name);
    if direct.is_file() {
        return direct;
    }
    panic!(
        "Could not locate Perfetto binary '{name}'. Ensure ~/dev/perfetto/out/release is built."
    );
}

/// Parses a single integer result from Trace Processor query output.
fn parse_query_count(output: &str) -> Option<u64> {
    let mut header_seen = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "\"count(*)\"" || trimmed == "count(*)" {
            header_seen = true;
            continue;
        }
        if header_seen {
            if let Ok(val) = trimmed.parse::<u64>() {
                return Some(val);
            }
        }
    }
    None
}

#[test]
fn test_system_tracing_lifecycle() {
    if env::var("TACHO_TEST_TRACED").as_deref() != Ok("1") {
        eprintln!("Skipping test_system_tracing_lifecycle: set TACHO_TEST_TRACED=1 to run");
        return;
    }

    let traced_bin = find_perfetto_binary("traced");
    let probes_bin = find_perfetto_binary("traced_probes");
    let perfetto_bin = find_perfetto_binary("perfetto");
    let tp_bin = tacho_core::find_trace_processor()
        .unwrap_or_else(|| find_perfetto_binary("trace_processor_shell"));

    let temp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let producer_sock = temp_dir.path().join("producer.sock");
    let consumer_sock = temp_dir.path().join("consumer.sock");
    let runtime_dir = temp_dir.path().join("runtime");
    fs::create_dir_all(&runtime_dir).expect("failed to create runtime dir");

    // Start unprivileged traced
    let mut traced_cmd = Command::new(&traced_bin);
    traced_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let traced_child = traced_cmd.spawn().expect("failed to start traced");

    // Wait for traced socket to appear
    for _ in 0..20 {
        if consumer_sock.exists() && producer_sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Start unprivileged traced_probes
    let mut probes_cmd = Command::new(&probes_bin);
    probes_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let probes_child = probes_cmd.spawn().expect("failed to start traced_probes");

    let _daemon_guard = DaemonGuard {
        traced: Some(traced_child),
        probes: Some(probes_child),
    };

    // Set TACHO_PERFETTO so Perfetto::locate() finds the binary
    env::set_var("TACHO_PERFETTO", &perfetto_bin);

    // Configure Perfetto instance via locate()
    let perfetto = Perfetto::locate()
        .expect("Perfetto::locate() should succeed with TACHO_PERFETTO")
        .with_consumer_socket(&consumer_sock)
        .with_runtime_dir(&runtime_dir);

    // Verify traced daemon is reachable
    assert!(
        perfetto.traced_running(),
        "traced daemon should be running and reachable on {:?}",
        consumer_sock
    );

    // Initial status should be None (no session started)
    assert!(
        perfetto.status().expect("status query failed").is_none(),
        "session should not be running yet"
    );

    // Start background session with Default profile
    perfetto
        .start(Profile::Default)
        .expect("start(Profile::Default) failed");

    // Verify status returns active session
    let session = perfetto
        .status()
        .expect("status query failed")
        .expect("session should be running");
    assert_eq!(session.name, "tacho-system");
    assert_eq!(session.state, "STARTED");

    // Idempotent start: calling start again should succeed
    perfetto
        .start(Profile::Default)
        .expect("idempotent start should succeed");

    // Sleep briefly to let process_stats and sys_stats record system telemetry
    thread::sleep(Duration::from_millis(1500));

    // Clone session into Vec<u8>
    let mut trace_bytes = Vec::new();
    let bytes_cloned = perfetto
        .clone_into(&mut trace_bytes)
        .expect("clone_into should succeed");
    assert!(bytes_cloned > 0, "cloned trace should not be empty");
    assert_eq!(
        bytes_cloned as usize,
        trace_bytes.len(),
        "returned byte count must match buffer length"
    );

    // Validate cloned trace with trace_processor_shell
    let trace_file = runtime_dir.join("cloned_test.pftrace");
    fs::write(&trace_file, &trace_bytes).expect("failed to write cloned trace file");

    // Check stats table: 0 errors and 0 data loss
    let stats_output = Command::new(&tp_bin)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT count(*) FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output()
        .expect("failed to execute trace_processor_shell for stats query");
    assert!(
        stats_output.status.success(),
        "trace_processor_shell failed: {}",
        String::from_utf8_lossy(&stats_output.stderr)
    );
    let stats_str = String::from_utf8_lossy(&stats_output.stdout);
    let error_count = parse_query_count(&stats_str)
        .unwrap_or_else(|| panic!("failed to parse count from stats query: {stats_str}"));
    assert_eq!(
        error_count, 0,
        "cloned trace must have 0 error/data_loss stats"
    );

    // Check process table: non-empty
    let proc_output = Command::new(&tp_bin)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT count(*) FROM process;")
        .output()
        .expect("failed to execute trace_processor_shell for process query");
    assert!(
        proc_output.status.success(),
        "trace_processor_shell failed: {}",
        String::from_utf8_lossy(&proc_output.stderr)
    );
    let proc_str = String::from_utf8_lossy(&proc_output.stdout);
    let proc_count = parse_query_count(&proc_str)
        .unwrap_or_else(|| panic!("failed to parse count from process query: {proc_str}"));
    assert!(
        proc_count > 0,
        "process table must be non-empty, found {proc_count}"
    );

    // Stop session
    perfetto.stop().expect("stop() failed");

    // Verify session is no longer active
    assert!(
        perfetto.status().expect("status query failed").is_none(),
        "session should be stopped"
    );

    // Attempting to clone when no session is active must return clear error
    let mut empty_vec = Vec::new();
    let err = perfetto
        .clone_into(&mut empty_vec)
        .expect_err("clone_into must fail when no session is active");
    match err {
        SystemError::NoActiveSession(_) => {}
        other => panic!("expected NoActiveSession, got: {other:?}"),
    }
}
