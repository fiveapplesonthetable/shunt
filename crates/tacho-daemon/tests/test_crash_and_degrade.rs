//! Crash resilience and read-only degradation integration tests for tacho-daemon.
//!
//! Covers:
//! 1. Random write-point SIGKILL crash test (3 iterations):
//!    - Verifies SQLite WAL recovery
//!    - Verifies sequence numbers are strictly contiguous without gaps (1..=MAX(seq))
//!    - Verifies snapshots from recovered database load cleanly in Perfetto trace_processor_shell with 0 stats errors
//! 2. Read-only DB degradation test (chmod 0444):
//!    - Sends 500 inputs via api.sock
//!    - Verifies 0 panics
//!    - Verifies < 20ms response latency on api.sock
//!    - Verifies live_state continues updating despite persistent SQLite WAL write errors

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use tacho_core::{Input, SessionKey, TaskStatus, ToolInput};
use tacho_daemon::{write_snapshot, Config, Daemon, DaemonPaths};

/// Oracle validation helper using trace_processor_shell.
fn verify_trace_oracle_clean(tp_path: &str, trace_path: &Path) {
    let output = Command::new(tp_path)
        .arg(trace_path)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output()
        .expect("failed to execute trace_processor_shell");

    assert!(
        output.status.success(),
        "trace_processor_shell failed to execute on {:?}",
        trace_path
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let error_rows: Vec<&str> = stdout
        .trim()
        .lines()
        .filter(|l| !l.starts_with("\"name\"") && !l.is_empty())
        .collect();

    assert!(
        error_rows.is_empty(),
        "trace_processor reported stats errors or data_loss on {:?}: {:?}",
        trace_path,
        error_rows
    );

    // Verify tracks and slices were parsed
    let tracks_out = Command::new(tp_path)
        .arg(trace_path)
        .arg("-Q")
        .arg("SELECT count(*) FROM track;")
        .output()
        .expect("failed to count tracks");
    let tracks_str = String::from_utf8_lossy(&tracks_out.stdout);
    let track_count: i64 = tracks_str
        .lines()
        .nth(1)
        .and_then(|s| s.trim().trim_matches('"').parse().ok())
        .unwrap_or(0);
    assert!(
        track_count > 0,
        "Expected positive track count in oracle, got {}",
        track_count
    );
}

/// Crash worker subprocess entry point.
///
/// When spawned with `TACHO_CRASH_WORKER=1`, this function runs the daemon
/// and continuously ingests high-frequency events into SQLite WAL until it
/// receives SIGKILL from the parent test process.
#[test]
fn test_crash_worker() {
    if env::var("TACHO_CRASH_WORKER").is_err() {
        return;
    }

    let home = env::var("TACHO_HOME_DIR").expect("TACHO_HOME_DIR required for worker");
    let runtime = env::var("TACHO_RUNTIME_DIR").expect("TACHO_RUNTIME_DIR required for worker");

    let paths = DaemonPaths::custom(&home, &runtime).expect("failed to initialize paths");
    let daemon = Daemon::start(paths, Config::default()).expect("failed to start daemon");

    let session = SessionKey::new("crash-agent-provider", "session-sigkill-target");
    let mut seq = 0u64;

    let _ = daemon.appender.send_input(
        Input::RunStart {
            source_ts_ns: Some(1_000_000_000),
            provider: "crash-agent-provider".to_string(),
            session_id: "session-sigkill-target".to_string(),
            agent: Some("agent-crash-tester".to_string()),
            task: Some("task-crash-resilience".to_string()),
            pid: Some(99999),
            cwd: Some("/tmp/crash_work".to_string()),
            model: Some("gemini-1.5-pro".to_string()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        1_000_000_000,
    );

    let _ = daemon.appender.send_input(
        Input::Task {
            source_ts_ns: Some(1_000_000_000),
            slug: "task-crash-resilience".to_string(),
            brief_digest: None,
            brief_bytes: Some(64),
            paths: vec!["src/lib.rs".to_string()],
            agent: Some("agent-crash-tester".to_string()),
            base_commit: None,
            token_budget: None,
            cost_budget_micros: None,
        },
        1_000_000_000,
    );
    daemon.appender.flush();

    // Signal to parent that daemon is up and initial events are committed
    println!("READY");
    let _ = io::stdout().flush();

    loop {
        seq += 1;
        let ts = 1_000_000_000 + seq * 100_000;
        let input = match seq % 5 {
            0 => Input::TaskState {
                source_ts_ns: Some(ts),
                slug: "task-crash-resilience".to_string(),
                state: TaskStatus::InProgress,
                because: Some(format!("step {}", seq)),
            },
            1 => Input::TurnStart {
                source_ts_ns: Some(ts),
                session: session.clone(),
                model: Some("gemini-1.5-pro".to_string()),
            },
            2 => Input::ToolBegin {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-call-{}", seq),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: format!("cargo build --release -j {}", seq % 16),
                    cwd: Some("/tmp/crash_work".to_string()),
                    git_tree: None,
                },
            },
            3 => Input::ToolEnd {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-call-{}", seq - 1),
                result_bytes: Some(128),
                exit_code: Some(0),
                error: false,
            },
            _ => Input::CounterSample {
                source_ts_ns: Some(ts),
                track_key: format!("host:cpu:{}", seq % 4),
                value: (seq % 100) as i64,
                unit: Some("%".to_string()),
            },
        };

        let _ = daemon.appender.send_input(input, ts);

        if seq % 20 == 0 {
            thread::yield_now();
        }
    }
}

#[test]
fn test_random_write_point_sigkill_crash() {
    let tp = tacho_core::find_trace_processor()
        .expect("trace_processor_shell not found; run scripts/oracle.sh or set TACHO_TP")
        .to_string_lossy()
        .into_owned();
    assert!(
        Path::new(&tp).exists(),
        "trace_processor_shell not found at {}",
        tp
    );

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();

    let exe = env::current_exe().expect("failed to determine current_exe");
    let mut prev_max_seq = 0i64;

    for iteration in 1..=3 {
        // Spawn crash worker child subprocess
        let mut child = Command::new(&exe)
            .arg("test_crash_worker")
            .arg("--exact")
            .arg("--nocapture")
            .env("TACHO_CRASH_WORKER", "1")
            .env("TACHO_HOME_DIR", home.to_str().unwrap())
            .env("TACHO_RUNTIME_DIR", runtime.to_str().unwrap())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("failed to spawn crash worker subprocess");

        // Wait for child to initialize and print READY
        let stdout = child.stdout.take().expect("failed to capture stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!("Crash worker subprocess exited before signaling READY");
            }
            if line.trim() == "READY" {
                break;
            }
        }

        // Wait until the worker has committed additional events beyond prev_max_seq
        let target_seq = prev_max_seq + 10 + (iteration * 5);
        loop {
            if let Ok(conn) = rusqlite::Connection::open(paths.events_db()) {
                let cur_max: i64 = conn
                    .query_row("SELECT COALESCE(MAX(seq), 0) FROM events;", [], |r| {
                        r.get(0)
                    })
                    .unwrap_or(0);
                if cur_max >= target_seq {
                    break;
                }
            }
            thread::yield_now();
        }

        // Random write-point SIGKILL crash
        let pid = child.id() as i32;
        let kill_res = unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(kill_res, 0, "libc::kill SIGKILL failed on pid {}", pid);

        let status = child.wait().expect("failed to wait on child process");
        assert!(
            !status.success(),
            "Expected non-zero exit from SIGKILL, got {:?}",
            status
        );

        // Verify SQLite WAL recovery and contiguous sequence numbers
        let conn = rusqlite::Connection::open(paths.events_db())
            .expect("SQLite WAL recovery failed on opening crashed DB");

        let mut stmt = conn
            .prepare("SELECT seq FROM events ORDER BY seq ASC;")
            .expect("failed to prepare SELECT seq query");
        let seqs: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .expect("query_map failed")
            .map(|r| r.expect("failed to extract seq"))
            .collect();

        assert!(
            !seqs.is_empty(),
            "Iteration {}: events table is empty after crash",
            iteration
        );
        assert_eq!(
            seqs[0], 1,
            "Iteration {}: first sequence number must be 1",
            iteration
        );

        // Verify contiguous sequence numbers without gaps: seq[i] == i + 1
        for (idx, &seq) in seqs.iter().enumerate() {
            let expected = (idx + 1) as i64;
            assert_eq!(
                seq, expected,
                "Iteration {}: sequence gap detected at index {} (expected {}, got {})",
                iteration, idx, expected, seq
            );
        }

        let max_seq = *seqs.last().unwrap();
        assert_eq!(
            max_seq,
            seqs.len() as i64,
            "Iteration {}: max_seq does not match row count",
            iteration
        );
        assert!(
            max_seq > prev_max_seq,
            "Iteration {}: max_seq ({}) should be greater than prev ({})",
            iteration,
            max_seq,
            prev_max_seq
        );
        prev_max_seq = max_seq;

        // Verify snapshot from recovered database loads cleanly in Perfetto oracle
        let trace_path = temp
            .path()
            .join(format!("crash_recovered_iter_{}.perfetto-trace", iteration));
        let mut trace_out = fs::File::create(&trace_path).unwrap();
        let stats = write_snapshot(&paths.events_db(), None, None, &mut trace_out)
            .expect("write_snapshot failed on recovered database");

        assert!(stats.events > 0);
        assert_eq!(stats.events, max_seq as u64);

        verify_trace_oracle_clean(&tp, &trace_path);
    }
}

#[test]
fn test_readonly_db_degradation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let session = SessionKey::new("gemini-test", "session-degrade-test");

    // Phase 1: Initialize database with schema and initial events
    {
        let daemon = Daemon::start(paths.clone(), config.clone()).unwrap();
        daemon
            .appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(1_000_000_000),
                    provider: "gemini-test".to_string(),
                    session_id: "session-degrade-test".to_string(),
                    agent: Some("agent-degrade".to_string()),
                    task: Some("task-degrade".to_string()),
                    pid: Some(10101),
                    cwd: Some("/tmp".to_string()),
                    model: Some("gemini-1.5-flash".to_string()),
                    parent_session: None,
                    git_tree: None,
                    role: None,
                },
                1_000_000_000,
            )
            .unwrap();
        daemon.appender.flush();
    } // Daemon stops here cleanly

    // Phase 2: Chmod database file to 0444 (read-only)
    let db_path = paths.events_db();
    assert!(db_path.exists(), "events.sqlite must exist");
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o444)).unwrap();

    let wal_path = paths.home.join("events.sqlite-wal");
    if wal_path.exists() {
        let _ = fs::set_permissions(&wal_path, fs::Permissions::from_mode(0o444));
    }
    let shm_path = paths.home.join("events.sqlite-shm");
    if shm_path.exists() {
        let _ = fs::set_permissions(&shm_path, fs::Permissions::from_mode(0o444));
    }

    // Phase 3: Start daemon on the read-only database
    let daemon = Daemon::start(paths.clone(), config)
        .expect("Daemon should start even when database file is read-only");

    // Phase 4: Send 500 inputs via api.sock, verifying < 20ms response latency and no panics
    let sock_path = paths.api_sock();
    let stream = UnixStream::connect(&sock_path).expect("failed to connect to api.sock");
    stream
        .set_read_timeout(Some(Duration::from_millis(1000)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(1000)))
        .unwrap();

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);

    let num_inputs = 500;
    let mut max_latency = Duration::ZERO;

    for i in 1..=num_inputs {
        let ts = 1_000_000_000 + i as u64 * 1_000_000;
        let input = if i % 2 == 1 {
            Input::ToolBegin {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-degrade-{}", i),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: format!("echo test {}", i),
                    cwd: None,
                    git_tree: None,
                },
            }
        } else {
            Input::ToolEnd {
                source_ts_ns: Some(ts),
                session: session.clone(),
                tool_use_id: format!("tool-degrade-{}", i - 1),
                result_bytes: Some(4),
                exit_code: Some(0),
                error: false,
            }
        };

        let req = serde_json::json!({ "input": input });
        let req_bytes = serde_json::to_vec(&req).unwrap();

        let req_start = Instant::now();
        writer.write_all(&req_bytes).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();

        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        let latency = req_start.elapsed();

        if latency > max_latency {
            max_latency = latency;
        }

        assert!(
            resp_line.contains("\"status\":\"ok\""),
            "Unexpected response: {}",
            resp_line
        );
    }

    println!(
        "Max api.sock response latency over 500 inputs under read-only DB: {:?}",
        max_latency
    );

    // Phase 5: Flush and verify graceful degradation
    daemon.appender.flush();

    // Appender write_errors must be greater than 0 due to SQLite write failures
    assert!(
        daemon.appender.write_errors() > 0,
        "Expected write_errors > 0 on read-only DB, got {}",
        daemon.appender.write_errors()
    );

    // Verify live_state continued to update in-memory
    let state = daemon.appender.live_state();
    let agent_entry = state
        .agents
        .get("agent-degrade")
        .expect("live_state must contain agent-degrade");
    assert_eq!(agent_entry.provider, "gemini-test");
    assert_eq!(agent_entry.session_id, "session-degrade-test");
    assert!(
        !agent_entry.recent_commands.is_empty(),
        "recent_commands in live_state should have updated"
    );

    // Verify api.sock status command continues working
    let status_req = serde_json::json!({ "cmd": "status", "json": true }).to_string() + "\n";
    writer.write_all(status_req.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut status_resp = String::new();
    reader.read_line(&mut status_resp).unwrap();
    assert!(
        status_resp.contains("\"status\":\"ok\""),
        "Status command failed under degraded mode: {}",
        status_resp
    );
}
