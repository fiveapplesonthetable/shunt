//! Test 14: Wedged appender does not block trace.sock snapshots.
//!
//! Verifies: block appender via test hook; trace.sock still returns a valid Perfetto
//! trace that loads in trace_processor_shell with zero error or data_loss stats.

use std::env;
use std::fs;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::Ordering;
use tacho_core::{Input, SessionKey, ToolInput};
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_wedged_appender_trace_oracle() {
    let tp_path = env::var("TACHO_TP").unwrap_or_else(|_| {
        "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell".to_string()
    });

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let daemon = Daemon::start(paths.clone(), config).unwrap();
    let session = SessionKey::new("claude", "sess-wedge-test");

    // Populate some events into the rings
    daemon
        .appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(1_000_000_000),
                provider: "claude".to_string(),
                session_id: "sess-wedge-test".to_string(),
                agent: Some("agent-wedge".to_string()),
                task: None,
                pid: Some(9999),
                cwd: Some("/dev".to_string()),
                model: Some("claude-3-opus".to_string()),
                parent_session: None,
                git_tree: None,
            },
            1_000_000_000,
        )
        .unwrap();

    daemon
        .appender
        .send_input(
            Input::ToolBegin {
                source_ts_ns: Some(1_000_100_000),
                session: session.clone(),
                tool_use_id: "tool-1".to_string(),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: "ls -la".to_string(),
                    cwd: Some("/dev".to_string()),
                    git_tree: None,
                },
            },
            1_000_100_000,
        )
        .unwrap();

    daemon.appender.flush();

    // 1. Block the appender thread via test hook
    daemon
        .appender
        .hooks()
        .wedged
        .store(true, Ordering::Release);

    // 2. Connect to trace.sock while appender is completely blocked
    let mut stream = UnixStream::connect(paths.trace_sock())
        .expect("trace.sock connection failed while appender was wedged");

    let mut trace_bytes = Vec::new();
    stream
        .read_to_end(&mut trace_bytes)
        .expect("Failed to read snapshot from trace.sock");

    assert!(
        !trace_bytes.is_empty(),
        "trace.sock returned 0 bytes while appender was wedged"
    );

    // 3. Un-wedge appender thread
    daemon
        .appender
        .hooks()
        .wedged
        .store(false, Ordering::Release);

    // 4. Validate captured trace in the oracle (trace_processor_shell)
    let trace_file = temp.path().join("wedged.pftrace");
    fs::write(&trace_file, &trace_bytes).unwrap();

    let output = Command::new(&tp_path)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output();

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "trace_processor_shell failed: {}\n{}",
            stdout,
            stderr
        );
        let trimmed = stdout.trim();
        // Skip CSV header line if present
        let error_rows: Vec<&str> = trimmed
            .lines()
            .filter(|l| !l.starts_with("\"name\"") && !l.is_empty())
            .collect();
        assert!(
            error_rows.is_empty(),
            "Oracle reported errors in trace: {:?}",
            error_rows
        );
    } else {
        eprintln!(
            "Notice: trace_processor_shell not found at {}, skipping oracle execution",
            tp_path
        );
    }
}
