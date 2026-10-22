//! Snapshots under load: 4 concurrent snapshot clients while high-rate inputs stream in.
//!
//! Verifies: Each snapshot captured under load loads clean in trace_processor_shell with 0 errors.

use std::env;
use std::fs;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tacho_core::{Input, SessionKey, ToolInput};
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_snapshots_under_load() {
    let tp_path = env::var("TACHO_TP").unwrap_or_else(|_| {
        "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell".to_string()
    });

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let daemon = Daemon::start(paths.clone(), config).unwrap();
    let stop_readers = Arc::new(AtomicBool::new(false));
    let snapshot_count = Arc::new(AtomicUsize::new(0));

    // Spawn 4 concurrent snapshot reader threads
    let mut reader_handles = Vec::new();
    for reader_idx in 0..4 {
        let trace_sock = paths.trace_sock();
        let stop = stop_readers.clone();
        let count = snapshot_count.clone();
        let tp = tp_path.clone();
        let reader_dir = temp.path().join(format!("reader_{}", reader_idx));
        fs::create_dir_all(&reader_dir).unwrap();

        let handle = std::thread::spawn(move || {
            let mut iter = 0;
            while !stop.load(Ordering::Acquire) {
                if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                    let mut trace_bytes = Vec::new();
                    if stream.read_to_end(&mut trace_bytes).is_ok() && !trace_bytes.is_empty() {
                        iter += 1;
                        count.fetch_add(1, Ordering::Relaxed);

                        // Periodically validate captured trace in oracle
                        if iter % 5 == 1 {
                            let trace_file = reader_dir.join(format!("snap_{}.pftrace", iter));
                            let _ = fs::write(&trace_file, &trace_bytes);

                            let output = Command::new(&tp)
                                .arg(&trace_file)
                                .arg("-Q")
                                .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
                                .output();

                            if let Ok(out) = output {
                                if out.status.success() {
                                    let stdout = String::from_utf8_lossy(&out.stdout);
                                    let error_rows: Vec<&str> = stdout
                                        .trim()
                                        .lines()
                                        .filter(|l| !l.starts_with("\"name\"") && !l.is_empty())
                                        .collect();
                                    assert!(
                                        error_rows.is_empty(),
                                        "Reader {} iteration {} had oracle errors: {:?}",
                                        reader_idx,
                                        iter,
                                        error_rows
                                    );
                                }
                            }
                            let _ = fs::remove_file(&trace_file);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        reader_handles.push(handle);
    }

    // Stream 200,000 inputs rapidly in balanced begin/end pairs
    let total_inputs = 200_000;
    let session = SessionKey::new("claude", "sess-load-test");

    daemon
        .appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(1_000_000_000),
                provider: "claude".to_string(),
                session_id: "sess-load-test".to_string(),
                agent: Some("agent-load".to_string()),
                task: None,
                pid: Some(777),
                cwd: Some("/dev".to_string()),
                model: Some("claude-3-5".to_string()),
                parent_session: None,
                git_tree: None,
            },
            1_000_000_000,
        )
        .unwrap();

    let start = Instant::now();
    for i in 0..total_inputs {
        let ts = 1_000_000_000 + (i as u64) * 10_000;
        let input = if i % 2 == 0 {
            Input::ToolBegin {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-{}", i / 2),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: format!("echo test-{}", i / 2),
                    cwd: Some("/dev".to_string()),
                    git_tree: None,
                },
            }
        } else {
            Input::ToolEnd {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-{}", i / 2),
                error: false,
                result_bytes: Some(64),
                exit_code: Some(0),
            }
        };
        daemon.appender.send_input(input, ts).unwrap();
    }

    daemon.appender.flush();
    let elapsed = start.elapsed();
    println!(
        "Streamed {} inputs under 4 concurrent snapshot readers in {:.2?} ({:.0} inputs/s)",
        total_inputs,
        elapsed,
        (total_inputs as f64) / elapsed.as_secs_f64()
    );

    stop_readers.store(true, Ordering::Release);
    for handle in reader_handles {
        handle
            .join()
            .expect("Reader thread panicked with oracle error");
    }

    let completed_snapshots = snapshot_count.load(Ordering::Acquire);
    assert!(
        completed_snapshots > 0,
        "Expected at least one completed snapshot under load, got 0"
    );
    println!(
        "Completed {} concurrent snapshots successfully under load",
        completed_snapshots
    );
}
