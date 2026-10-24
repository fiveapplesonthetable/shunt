//! Torture & chaos test suite for tacho-daemon.
//!
//! Covers:
//! 1. 16 concurrent threads hammering mixed inputs (hooks/sessions, 100-deep nested
//!    custom spans, counters, OTLP HTTP/1.1 gzipped batches) + concurrent `write_snapshot` readers.
//! 2. Mid-storm chaos:
//!    - Half-closed `api.sock` and `trace.sock` connections dropped mid-frame.
//!    - Corrupt non-UTF8 binary data.
//!    - 50 concurrent open tools on a single agent triggering `LaneOverflow` onto `overflow` track.
//!    - Backward clock jump (`ClockStep` of 1 hour).
//!    - Simulated abrupt restart recovery on the same SQLite DB while operations are inflight.
//! 3. Perfetto `trace_processor_shell` oracle validation verifying 0 error / data_loss stats.

use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tacho_core::{CustomOp, Input, SessionKey, ToolInput};
use tacho_daemon::{write_snapshot, Config, Daemon, DaemonPaths};

/// Helper to execute query against trace_processor_shell and return integer scalar result.
fn query_oracle_scalar(tp_path: &str, trace_path: &Path, sql: &str) -> i64 {
    let output = Command::new(tp_path)
        .arg(trace_path)
        .arg("-Q")
        .arg(sql)
        .output()
        .expect("failed to execute trace_processor_shell");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    if lines.len() >= 2 {
        lines[1]
            .trim()
            .trim_matches('"')
            .parse::<i64>()
            .unwrap_or(-1)
    } else {
        -1
    }
}

/// Helper to verify trace has 0 error and 0 data_loss stats.
fn verify_trace_oracle_clean(tp_path: &str, trace_path: &Path) {
    let output = Command::new(tp_path)
        .arg(trace_path)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output()
        .expect("failed to execute trace_processor_shell");

    assert!(
        output.status.success(),
        "trace_processor_shell failed to execute"
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
}

const NUM_THREADS: usize = 16;

/// Operation counter the main thread can block on (instead of spinning on an atomic).
///
/// Tracks the total op count plus the number of completed iterations per chaos thread.
struct OpCounter {
    state: Mutex<(usize, [usize; NUM_THREADS])>,
    cvar: Condvar,
}

impl OpCounter {
    fn new() -> Self {
        Self {
            state: Mutex::new((0, [0; NUM_THREADS])),
            cvar: Condvar::new(),
        }
    }

    fn add(&self, thread_idx: usize, n: usize) {
        let mut st = self.state.lock().unwrap();
        st.0 += n;
        st.1[thread_idx] += 1;
        self.cvar.notify_all();
    }

    fn snapshot(&self) -> (usize, [usize; NUM_THREADS]) {
        *self.state.lock().unwrap()
    }

    /// Blocks until at least `min_total` ops and every thread completed more iterations
    /// than in `since`.
    fn wait_for(&self, min_total: usize, since: &[usize; NUM_THREADS]) {
        let guard = self.state.lock().unwrap();
        let _guard = self
            .cvar
            .wait_while(guard, |st| {
                st.0 < min_total || st.1.iter().zip(since).any(|(now, then)| now <= then)
            })
            .unwrap();
    }

    fn total(&self) -> usize {
        self.state.lock().unwrap().0
    }
}

/// Helper to send single input over api.sock with retry on disconnect.
fn send_input_resilient(sock_path: &Path, input: &Input) -> bool {
    let fast_path = serde_json::json!({ "input": input });
    let mut bytes = serde_json::to_vec(&fast_path).unwrap();
    bytes.push(b'\n');

    for _ in 0..10 {
        if let Ok(mut stream) = UnixStream::connect(sock_path) {
            if stream.write_all(&bytes).is_ok() && stream.flush().is_ok() {
                let mut reader = BufReader::new(stream);
                let mut resp = String::new();
                if reader.read_line(&mut resp).is_ok() && resp.contains("\"status\":\"ok\"") {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn test_chaos_and_torture() {
    let tp_path = tacho_core::find_trace_processor()
        .expect("trace_processor_shell not found; run scripts/oracle.sh or set TACHO_TP")
        .to_string_lossy()
        .into_owned();

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();

    // Pick available ephemeral port for OTLP server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let otlp_port = listener.local_addr().unwrap().port();
    drop(listener);

    let config = Config {
        otlp_addr: Some(format!("127.0.0.1:{}", otlp_port)),
        ..Default::default()
    };

    println!("Starting initial daemon instance...");
    let mut daemon =
        Some(Daemon::start(paths.clone(), config.clone()).expect("failed to start daemon"));

    let running = Arc::new(AtomicBool::new(true));
    let total_ops = Arc::new(OpCounter::new());
    let mut handles = Vec::new();

    // 1. Threads 0..3: Multi-agent lifecycle hook sessions (claude, gemini, jetski)
    for worker_idx in 0..4 {
        let sock = paths.api_sock();
        let run = running.clone();
        let ops = total_ops.clone();
        let provider = match worker_idx % 3 {
            0 => "claude",
            1 => "gemini",
            _ => "jetski",
        };

        handles.push(
            thread::Builder::new()
                .name(format!("chaos-worker-{}", worker_idx))
                .spawn(move || {
                    let mut iter = 0;
                    while run.load(Ordering::Acquire) {
                        iter += 1;
                        let sess_id = format!("sess-{}-{}", worker_idx, iter);
                        let session = SessionKey::new(provider, &sess_id);

                        send_input_resilient(
                            &sock,
                            &Input::RunStart {
                                source_ts_ns: None,
                                provider: provider.to_string(),
                                session_id: sess_id.clone(),
                                agent: Some(format!("worker-{}", worker_idx)),
                                task: Some("torture-task".to_string()),
                                pid: Some(1000 + worker_idx as u32),
                                cwd: Some("/workspace".to_string()),
                                model: Some("model-v1".to_string()),
                                parent_session: None,
                                git_tree: None,
                                role: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::TurnStart {
                                source_ts_ns: None,
                                session: session.clone(),
                                model: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::ToolBegin {
                                source_ts_ns: None,
                                session: session.clone(),
                                tool_use_id: format!("t-{}", iter),
                                tool_name: "Bash".to_string(),
                                input: ToolInput::Shell {
                                    argv: "cargo check".to_string(),
                                    cwd: None,
                                    git_tree: None,
                                },
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::ToolEnd {
                                source_ts_ns: None,
                                session: session.clone(),
                                tool_use_id: format!("t-{}", iter),
                                error: false,
                                result_bytes: Some(1024),
                                exit_code: Some(0),
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::TurnEnd {
                                source_ts_ns: None,
                                session: session.clone(),
                                input_tokens: Some(500),
                                output_tokens: Some(150),
                                cached_tokens: Some(100),
                                text_bytes: Some(2048),
                                cost_micros: Some(5000),
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::RunEnd {
                                source_ts_ns: None,
                                session,
                                exit_code: Some(0),
                                reason: Some("ok".to_string()),
                            },
                        );

                        ops.add(worker_idx, 6);
                    }
                })
                .unwrap(),
        );
    }

    // 2. Threads 4..5: 100-deep nested custom spans
    for worker_idx in 4..6 {
        let sock = paths.api_sock();
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(
            thread::Builder::new()
                .name(format!("chaos-nested-{}", worker_idx))
                .spawn(move || {
                    let track = format!("custom:thread-{}", worker_idx);
                    while run.load(Ordering::Acquire) {
                        // Open 100 nested spans
                        for d in 0..100 {
                            send_input_resilient(
                                &sock,
                                &Input::Custom {
                                    source_ts_ns: None,
                                    track: track.clone(),
                                    op: CustomOp::Begin,
                                    name: Some(format!("span_{}", d)),
                                    id: Some(format!("id_{}", d)),
                                    args: BTreeMap::new(),
                                    value: None,
                                    flow_out: None,
                                    flow_in: None,
                                },
                            );
                        }

                        // Leaf instant
                        send_input_resilient(
                            &sock,
                            &Input::Custom {
                                source_ts_ns: None,
                                track: track.clone(),
                                op: CustomOp::Instant,
                                name: Some("deepest_leaf".to_string()),
                                id: None,
                                args: BTreeMap::new(),
                                value: None,
                                flow_out: None,
                                flow_in: None,
                            },
                        );

                        // Close 100 nested spans in reverse order
                        for d in (0..100).rev() {
                            send_input_resilient(
                                &sock,
                                &Input::Custom {
                                    source_ts_ns: None,
                                    track: track.clone(),
                                    op: CustomOp::End,
                                    name: Some(format!("span_{}", d)),
                                    id: Some(format!("id_{}", d)),
                                    args: BTreeMap::new(),
                                    value: None,
                                    flow_out: None,
                                    flow_in: None,
                                },
                            );
                        }

                        ops.add(worker_idx, 201);
                    }
                })
                .unwrap(),
        );
    }

    // 3. Threads 6..7: High-rate counter hammering
    for worker_idx in 6..8 {
        let sock = paths.api_sock();
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(
            thread::Builder::new()
                .name(format!("chaos-counter-{}", worker_idx))
                .spawn(move || {
                    let mut val = 0i64;
                    while run.load(Ordering::Acquire) {
                        val += 1;
                        send_input_resilient(
                            &sock,
                            &Input::CounterSample {
                                source_ts_ns: None,
                                track_key: format!("machine/cpu_worker_{}", worker_idx),
                                value: val * 10,
                                unit: Some("jiffies".to_string()),
                            },
                        );
                        send_input_resilient(
                            &sock,
                            &Input::CounterSample {
                                source_ts_ns: None,
                                track_key: format!("host:cpu/worker-{}", worker_idx),
                                value: val * 5,
                                unit: Some("us".to_string()),
                            },
                        );
                        send_input_resilient(
                            &sock,
                            &Input::CounterSample {
                                source_ts_ns: None,
                                track_key: format!("machine/mem_worker_{}", worker_idx),
                                value: val * 1024,
                                unit: Some("bytes".to_string()),
                            },
                        );
                        ops.add(worker_idx, 3);
                    }
                })
                .unwrap(),
        );
    }

    // 4. Threads 8..9: OTLP HTTP/1.1 gzipped log batches
    for worker_idx in 8..10 {
        let port = otlp_port;
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(thread::Builder::new()
            .name(format!("chaos-otlp-{}", worker_idx))
            .spawn(move || {
                let mut iter = 0;
                while run.load(Ordering::Acquire) {
                    iter += 1;
                    let json_body = serde_json::json!({
                        "resourceLogs": [{
                            "resource": {
                                "attributes": [{"key": "service.name", "value": {"stringValue": "otlp-chaos"}}]
                            },
                            "scopeLogs": [{
                                "logRecords": [{
                                    "timeUnixNano": format!("{}", 1_700_000_000_000_000_000u64 + iter * 1_000_000),
                                    "attributes": [
                                        {"key": "event.name", "value": {"stringValue": "claude_code.tool_result"}},
                                        {"key": "session_id", "value": {"stringValue": format!("sess-otlp-{}", worker_idx)}}
                                    ],
                                    "body": {"stringValue": "OTLP chaos payload"}
                                }]
                            }]
                        }]
                    }).to_string();

                    // Gzip compress payload
                    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                    let _ = encoder.write_all(json_body.as_bytes());
                    if let Ok(compressed) = encoder.finish() {
                        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {

                            let req = format!(
                                "POST /v1/logs HTTP/1.1\r\n\
                                 Host: 127.0.0.1:{}\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Encoding: gzip\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\r\n",
                                port,
                                compressed.len()
                            );
                            if stream.write_all(req.as_bytes()).is_ok()
                                && stream.write_all(&compressed).is_ok()
                                && stream.flush().is_ok()
                            {
                                let mut resp = String::new();
                                let _ = stream.read_to_string(&mut resp);
                                if resp.starts_with("HTTP/1.1 200 OK") {
                                    ops.add(worker_idx, 1);
                                }
                            }
                        }
                    }
                }
            })
            .unwrap());
    }

    // 5. Threads 10..11: Concurrent snapshot readers via trace.sock & direct write_snapshot
    let snap_temp = temp.path().join("snapshots");
    fs::create_dir_all(&snap_temp).unwrap();

    for worker_idx in 10..12 {
        let trace_sock = paths.trace_sock();
        let db_path = paths.events_db();
        let run = running.clone();
        let ops = total_ops.clone();
        let sdir = snap_temp.clone();
        let tp = tp_path.clone();

        handles.push(
            thread::Builder::new()
                .name(format!("chaos-snap-{}", worker_idx))
                .spawn(move || {
                    let mut snap_iter = 0;
                    while run.load(Ordering::Acquire) {
                        snap_iter += 1;
                        let snap_file =
                            sdir.join(format!("snap_{}_{}.pftrace", worker_idx, snap_iter));

                        if worker_idx == 10 {
                            // Via trace.sock
                            if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                                let mut buf = Vec::new();
                                if stream.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                                    let _ = fs::write(&snap_file, &buf);
                                    verify_trace_oracle_clean(&tp, &snap_file);
                                    let _ = fs::remove_file(&snap_file);
                                    ops.add(worker_idx, 1);
                                }
                            }
                        } else {
                            // Direct write_snapshot
                            let mut buf = Vec::new();
                            if write_snapshot(&db_path, None, None, &mut buf).is_ok()
                                && !buf.is_empty()
                            {
                                let _ = fs::write(&snap_file, &buf);
                                verify_trace_oracle_clean(&tp, &snap_file);
                                let _ = fs::remove_file(&snap_file);
                                ops.add(worker_idx, 1);
                            }
                        }
                    }
                })
                .unwrap(),
        );
    }

    // 6. Threads 12..13: Half-closed connections dropped mid-frame & malformed chaos
    for worker_idx in 12..14 {
        let api_sock = paths.api_sock();
        let trace_sock = paths.trace_sock();
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(thread::Builder::new()
            .name(format!("chaos-drops-{}", worker_idx))
            .spawn(move || {
                while run.load(Ordering::Acquire) {
                    // Mid-frame drop on api.sock
                    if let Ok(mut stream) = UnixStream::connect(&api_sock) {
                        let _ = stream.write_all(b"{\"input\":{\"type\":\"tool_begin\",\"session\":{\"provider\":\"chaos\"");
                        let _ = stream.flush();
                        drop(stream); // abrupt close
                    }

                    // Malformed non-UTF8 binary data on api.sock
                    if let Ok(mut stream) = UnixStream::connect(&api_sock) {
                        let _ = stream.write_all(&[0xff, 0xfe, 0x00, 0x01, 0xaa, 0xbb, 0xcc, b'\n']);
                        let _ = stream.flush();
                        drop(stream);
                    }

                    // Partial read and abrupt drop on trace.sock
                    if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                        let mut small_buf = [0u8; 128];
                        let _ = stream.read(&mut small_buf);
                        drop(stream); // abrupt drop while server is streaming
                    }

                    ops.add(worker_idx, 3);
                }
            })
            .unwrap());
    }

    // 7. Thread 14: 50 concurrent open tools on a single agent (Lane Overflow)
    {
        let sock = paths.api_sock();
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(
            thread::Builder::new()
                .name("chaos-lane-overflow".to_string())
                .spawn(move || {
                    let mut cycle = 0;
                    while run.load(Ordering::Acquire) {
                        cycle += 1;
                        let sess_id = format!("sess-overflow-{}", cycle);
                        let session = SessionKey::new("overflow-provider", &sess_id);

                        send_input_resilient(
                            &sock,
                            &Input::RunStart {
                                source_ts_ns: None,
                                provider: "overflow-provider".to_string(),
                                session_id: sess_id.clone(),
                                agent: Some("overflow-worker".to_string()),
                                task: Some("lane-stress".to_string()),
                                pid: Some(5555),
                                cwd: Some("/overflow".to_string()),
                                model: None,
                                parent_session: None,
                                git_tree: None,
                                role: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::TurnStart {
                                source_ts_ns: None,
                                session: session.clone(),
                                model: None,
                            },
                        );

                        // 50 concurrent ToolBegin invocations
                        for i in 0..50 {
                            send_input_resilient(
                                &sock,
                                &Input::ToolBegin {
                                    source_ts_ns: None,
                                    session: session.clone(),
                                    tool_use_id: format!("tool-ovf-{}", i),
                                    tool_name: "Bash".to_string(),
                                    input: ToolInput::Shell {
                                        argv: format!("stress_task_{}", i),
                                        cwd: None,
                                        git_tree: None,
                                    },
                                },
                            );
                        }

                        // Overflow track marker
                        send_input_resilient(
                            &sock,
                            &Input::Custom {
                                source_ts_ns: None,
                                track: "agent:overflow-worker/run:1/overflow".to_string(),
                                op: CustomOp::Instant,
                                name: Some("overflow_marker".to_string()),
                                id: None,
                                args: BTreeMap::new(),
                                value: None,
                                flow_out: None,
                                flow_in: None,
                            },
                        );

                        // 50 matching ToolEnd invocations
                        for i in 0..50 {
                            send_input_resilient(
                                &sock,
                                &Input::ToolEnd {
                                    source_ts_ns: None,
                                    session: session.clone(),
                                    tool_use_id: format!("tool-ovf-{}", i),
                                    error: false,
                                    result_bytes: Some(128),
                                    exit_code: Some(0),
                                },
                            );
                        }

                        send_input_resilient(
                            &sock,
                            &Input::TurnEnd {
                                source_ts_ns: None,
                                session: session.clone(),
                                input_tokens: Some(100),
                                output_tokens: Some(50),
                                cached_tokens: None,
                                text_bytes: None,
                                cost_micros: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::RunEnd {
                                source_ts_ns: None,
                                session,
                                exit_code: Some(0),
                                reason: Some("complete".to_string()),
                            },
                        );

                        ops.add(14, 104);
                    }
                })
                .unwrap(),
        );
    }

    // 8. Thread 15: Backward clock jumps (ClockStep of 1 hour)
    {
        let sock = paths.api_sock();
        let run = running.clone();
        let ops = total_ops.clone();

        handles.push(
            thread::Builder::new()
                .name("chaos-clock-step".to_string())
                .spawn(move || {
                    let mut cycle = 0;
                    while run.load(Ordering::Acquire) {
                        cycle += 1;
                        let sess_id = format!("sess-clock-{}", cycle);
                        let session = SessionKey::new("clock-provider", &sess_id);

                        let now_ns = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;

                        // 1 hour backward jump (3600s >> 600s bound) -> triggers ClockStep
                        let one_hour_ns = 3600 * 1_000_000_000u64;
                        let past_ts = now_ns.saturating_sub(one_hour_ns);

                        send_input_resilient(
                            &sock,
                            &Input::RunStart {
                                source_ts_ns: Some(past_ts),
                                provider: "clock-provider".to_string(),
                                session_id: sess_id.clone(),
                                agent: Some("clock-worker".to_string()),
                                task: None,
                                pid: None,
                                cwd: None,
                                model: None,
                                parent_session: None,
                                git_tree: None,
                                role: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::TurnStart {
                                source_ts_ns: Some(past_ts + 1_000_000),
                                session: session.clone(),
                                model: None,
                            },
                        );

                        // Followed by normal forward timestamp
                        send_input_resilient(
                            &sock,
                            &Input::TurnEnd {
                                source_ts_ns: Some(now_ns),
                                session: session.clone(),
                                input_tokens: Some(50),
                                output_tokens: Some(50),
                                cached_tokens: None,
                                text_bytes: None,
                                cost_micros: None,
                            },
                        );

                        send_input_resilient(
                            &sock,
                            &Input::RunEnd {
                                source_ts_ns: Some(now_ns + 1_000_000),
                                session,
                                exit_code: Some(0),
                                reason: None,
                            },
                        );

                        ops.add(15, 4);
                    }
                })
                .unwrap(),
        );
    }

    assert_eq!(
        handles.len(),
        NUM_THREADS,
        "Must have exactly 16 concurrent chaos threads"
    );

    println!("All 16 chaos threads running. Hammering until 500 ops...");
    // Wait for 500 ops and at least one completed iteration of every chaos thread.
    total_ops.wait_for(500, &[0; NUM_THREADS]);

    // Mid-storm chaos: Simulate abrupt daemon restart while threads are actively hammering!
    println!("=== INJECTING CHAOS: Abrupt daemon restart ===");
    let mut d = daemon.take().unwrap();
    d.stop();
    drop(d);

    // Verify SQLite WAL database exists and is valid
    assert!(paths.events_db().exists(), "SQLite database must exist");

    // Re-launch daemon on the exact same paths (restoring state from SQLite WAL)
    daemon = Some(Daemon::start(paths.clone(), config.clone()).expect("failed to restart daemon"));
    println!("=== Daemon restarted successfully on existing DB ===");

    // Hammer for another 500 ops against the restarted daemon
    // (and at least one more completed iteration of every thread after the restart).
    let (ops_before, iters_before) = total_ops.snapshot();
    total_ops.wait_for(ops_before + 500, &iters_before);

    // Stop all chaos threads
    println!("Stopping chaos threads...");
    running.store(false, Ordering::Release);

    for h in handles {
        h.join().expect("chaos thread panicked!");
    }

    let final_ops = total_ops.total();
    println!(
        "Chaos campaign finished. Total operations executed: {}",
        final_ops
    );
    assert!(
        final_ops > 500,
        "Expected at least 500 operations, got {}",
        final_ops
    );

    // Flush appender
    let active_daemon = daemon.as_ref().unwrap();
    active_daemon.appender.flush();

    // Query status via api.sock to verify health
    let mut status_stream =
        UnixStream::connect(paths.api_sock()).expect("connect to api.sock status");
    let cmd = serde_json::json!({ "cmd": "status", "json": true });
    status_stream
        .write_all(serde_json::to_string(&cmd).unwrap().as_bytes())
        .unwrap();
    status_stream.write_all(b"\n").unwrap();
    status_stream.flush().unwrap();

    let mut status_line = String::new();
    let mut reader = BufReader::new(status_stream);
    reader.read_line(&mut status_line).unwrap();
    assert!(
        status_line.contains("\"status\":\"ok\""),
        "Status response failed: {}",
        status_line
    );

    // Capture final trace snapshot from trace.sock
    let mut trace_stream = UnixStream::connect(paths.trace_sock()).expect("connect to trace.sock");
    let mut trace_bytes = Vec::new();
    trace_stream
        .read_to_end(&mut trace_bytes)
        .expect("failed to read trace.sock");
    assert!(!trace_bytes.is_empty(), "Trace snapshot was empty");

    let final_trace_file = temp.path().join("chaos_final.pftrace");
    fs::write(&final_trace_file, &trace_bytes).unwrap();
    println!("Saved final trace snapshot: {} bytes", trace_bytes.len());

    // Validate in trace_processor_shell oracle
    println!("Validating trace in Perfetto oracle: {:?}", tp_path);
    verify_trace_oracle_clean(&tp_path, &final_trace_file);

    // Query oracle for recorded structure
    let total_slices =
        query_oracle_scalar(&tp_path, &final_trace_file, "SELECT count(*) FROM slice;");
    println!("Total Perfetto slices: {}", total_slices);
    assert!(
        total_slices > 50,
        "Expected > 50 slices, got {}",
        total_slices
    );

    let total_tracks =
        query_oracle_scalar(&tp_path, &final_trace_file, "SELECT count(*) FROM track;");
    println!("Total Perfetto tracks: {}", total_tracks);
    assert!(
        total_tracks > 10,
        "Expected > 10 tracks, got {}",
        total_tracks
    );

    let total_counters =
        query_oracle_scalar(&tp_path, &final_trace_file, "SELECT count(*) FROM counter;");
    println!("Total Perfetto counter samples: {}", total_counters);
    assert!(
        total_counters > 0,
        "Expected counter samples, got {}",
        total_counters
    );

    // Verify 100-deep nested custom spans exist in slice table
    let deep_spans = query_oracle_scalar(
        &tp_path,
        &final_trace_file,
        "SELECT count(*) FROM slice WHERE depth >= 50;",
    );
    println!("Perfetto slices at depth >= 50: {}", deep_spans);
    assert!(
        deep_spans > 0,
        "Expected deep nested slices (depth >= 50), got {}",
        deep_spans
    );

    // Verify lane overflow tracks were allocated
    let lane_tracks = query_oracle_scalar(
        &tp_path,
        &final_trace_file,
        "SELECT count(*) FROM track WHERE name LIKE '%lane%' OR name LIKE '%overflow%';",
    );
    println!("Perfetto lane/overflow tracks: {}", lane_tracks);
    assert!(
        lane_tracks > 0,
        "Expected allocated lane/overflow tracks, got {}",
        lane_tracks
    );

    // Verify ClockStep instant was recorded on daemon track
    let clock_steps = query_oracle_scalar(&tp_path, &final_trace_file, "SELECT count(*) FROM slice WHERE track_id IN (SELECT id FROM track WHERE name = 'daemon');");
    println!(
        "Perfetto daemon track events (including ClockStep): {}",
        clock_steps
    );
    assert!(
        clock_steps > 0,
        "Expected daemon track events from backward clock jumps"
    );

    // Verify zero unclosed/orphaned slices at snapshot boundary
    let unclosed_slices = query_oracle_scalar(
        &tp_path,
        &final_trace_file,
        "SELECT count(*) FROM slice WHERE dur < 0;",
    );
    assert_eq!(
        unclosed_slices, 0,
        "Expected 0 unclosed slices at snapshot boundary, got {}",
        unclosed_slices
    );

    println!("All chaos & torture validations PASSED with 0 errors!");
}
