//! Throughput, latency, stage profiling, and snapshot scaling benchmarks.
//!
//! Measures:
//! 1. Ingest pipeline stage breakdown (JSON parse, recorder apply, SQLite commit, live-state publish).
//! 2. Streaming NDJSON throughput on persistent api.sock (target: >= 100k inputs/s).
//! 3. Streaming throughput with concurrent snapshot readers (drop <= 2x).
//! 4. Snapshot scaling and peak RSS at 100k, 1M, 10M rows.
//! 5. Appender ingest throughput while a 10M-row snapshot runs.

use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tacho_core::{Event, Input, Kind, Recorder};
use tacho_daemon::{write_snapshot, Config, Daemon, DaemonPaths, EventLog};
use tacho_proto::tacho;

fn get_peak_rss_mb() -> f64 {
    let mut usage = libc::rusage {
        ru_utime: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }
    (usage.ru_maxrss as f64) / 1024.0
}

/// Profiling benchmark measuring exact nanoseconds per input across ingest pipeline stages.
fn bench_ingest_pipeline_stage_profiling() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("profile_events.sqlite");
    let mut log = EventLog::open(&db_path).unwrap();

    let mut recorder = Recorder::new();
    let mut batch_events = Vec::new();

    // Prepare sample inputs
    let n = 20_000;
    let mut json_lines = Vec::with_capacity(n);
    for i in 0..n {
        let input_obj = serde_json::json!({
            "input": {
                "type": if i % 2 == 0 { "tool_begin" } else { "tool_end" },
                "session": { "provider": "claude", "session_id": "sess-profile" },
                "tool_use_id": format!("tool-{}", i / 2),
                "tool_name": "Bash",
                "input": { "tool": "shell", "argv": "cargo check" },
                "error": false
            }
        });
        json_lines.push(serde_json::to_string(&input_obj).unwrap());
    }

    // Stage 1: Parse JSON
    #[derive(serde::Deserialize)]
    struct FastPath {
        input: Input,
    }

    let start_parse = Instant::now();
    let mut parsed_inputs = Vec::with_capacity(n);
    for line in &json_lines {
        let parsed: FastPath = serde_json::from_str(line).unwrap();
        parsed_inputs.push(parsed.input);
    }
    let parse_dur = start_parse.elapsed();

    // Stage 2: Recorder.apply
    let start_recorder = Instant::now();
    let mut all_events = Vec::with_capacity(n);
    for (i, input) in parsed_inputs.into_iter().enumerate() {
        let ts = 1_000_000_000 + (i as u64) * 1_000;
        recorder.apply(input, ts, &mut batch_events);
        all_events.append(&mut batch_events);
    }
    let recorder_dur = start_recorder.elapsed();

    // Stage 3: SQLite WAL batch insertion (256-event batches)
    let start_sqlite = Instant::now();
    for chunk in all_events.chunks(256) {
        log.insert_batch(chunk).unwrap();
    }
    let sqlite_dur = start_sqlite.elapsed();

    // Stage 4: LiveState deep clone (amortized over 50ms intervals = ~5,000 events)
    let start_live = Instant::now();
    let live_clones = (n / 256).max(1);
    for _ in 0..live_clones {
        let _ = recorder.live_state().clone();
    }
    let live_dur = start_live.elapsed();

    let total_time = parse_dur + recorder_dur + sqlite_dur + live_dur;
    let parse_ns = (parse_dur.as_nanos() as f64) / (n as f64);
    let recorder_ns = (recorder_dur.as_nanos() as f64) / (n as f64);
    let sqlite_ns = (sqlite_dur.as_nanos() as f64) / (n as f64);
    let live_ns = (live_dur.as_nanos() as f64) / (n as f64);
    let total_ns = (total_time.as_nanos() as f64) / (n as f64);

    println!("\n================================================================================");
    println!("Ingest Pipeline Stage Latency Breakdown ({} inputs):", n);
    println!("--------------------------------------------------------------------------------");
    println!("Stage             | Latency / Input | % of Total Time | Theoretical Max Throughput");
    println!("--------------------------------------------------------------------------------");
    println!(
        "1. JSON Parse     | {:>8.1} ns     | {:>13.1}% | {:>10.0} inputs/s",
        parse_ns,
        (parse_ns / total_ns) * 100.0,
        1e9 / parse_ns
    );
    println!(
        "2. Recorder Apply | {:>8.1} ns     | {:>13.1}% | {:>10.0} inputs/s",
        recorder_ns,
        (recorder_ns / total_ns) * 100.0,
        1e9 / recorder_ns
    );
    println!(
        "3. SQLite Commit  | {:>8.1} ns     | {:>13.1}% | {:>10.0} inputs/s",
        sqlite_ns,
        (sqlite_ns / total_ns) * 100.0,
        1e9 / sqlite_ns
    );
    println!(
        "4. Live Publish   | {:>8.1} ns     | {:>13.1}% | {:>10.0} inputs/s",
        live_ns,
        (live_ns / total_ns) * 100.0,
        1e9 / live_ns
    );
    println!("--------------------------------------------------------------------------------");
    println!(
        "Total Ingest Time | {:>8.1} ns     | {:>13.1}% | {:>10.0} inputs/s",
        total_ns,
        100.0,
        1e9 / total_ns
    );
    println!("================================================================================\n");
}

/// End-to-end streaming throughput test on one persistent api.sock connection.
/// Target: >= 100k inputs/s.
fn bench_api_sock_streaming_throughput() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let daemon = Daemon::start(paths.clone(), config).unwrap();

    let stream = UnixStream::connect(paths.api_sock()).expect("Failed to connect to api.sock");
    let reader_stream = stream.try_clone().unwrap();

    let mut writer = BufWriter::with_capacity(128 * 1024, stream);
    let mut reader = BufReader::with_capacity(128 * 1024, reader_stream);

    // Initial run start
    let run_start = serde_json::json!({
        "input": {
            "type": "run_start",
            "provider": "claude",
            "session_id": "sess-bench-stream",
            "agent": "worker-1"
        }
    });
    writer
        .write_all(serde_json::to_string(&run_start).unwrap().as_bytes())
        .unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"status\":\"ok\""));

    let num_inputs = if cfg!(debug_assertions) {
        10_000
    } else {
        100_000
    };

    let reader_handle = std::thread::spawn(move || {
        let mut resp_line = String::new();
        let mut count = 0;
        while count < num_inputs {
            resp_line.clear();
            if reader.read_line(&mut resp_line).unwrap() == 0 {
                break;
            }
            if resp_line.contains("\"status\":\"ok\"") {
                count += 1;
            }
        }
        count
    });

    let start = Instant::now();

    let begin_line: &[u8] = br#"{"input":{"type":"tool_begin","session":{"provider":"claude","session_id":"sess-bench-stream"},"tool_use_id":"t-1","tool_name":"Bash","input":{"tool":"shell","argv":"echo 1"},"error":false}}"#;
    let end_line: &[u8] = br#"{"input":{"type":"tool_end","session":{"provider":"claude","session_id":"sess-bench-stream"},"tool_use_id":"t-1","tool_name":"Bash","input":{"tool":"shell","argv":"echo 1"},"error":false}}"#;

    for i in 0..num_inputs {
        let line_bytes = if i % 2 == 0 { begin_line } else { end_line };
        writer.write_all(line_bytes).unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();

    let read_count = reader_handle.join().unwrap();
    let elapsed = start.elapsed();
    let throughput = (num_inputs as f64) / elapsed.as_secs_f64();

    println!("============================================================");
    println!(
        "api.sock Streaming Ingest Benchmark ({} inputs):",
        num_inputs
    );
    println!("Responses received: {}", read_count);
    println!("Elapsed time: {:.3?}", elapsed);
    println!("Throughput: {:.0} inputs/sec", throughput);
    println!("============================================================");

    assert_eq!(read_count, num_inputs, "Did not receive all responses");

    daemon.appender.flush();
    assert_eq!(daemon.appender.write_errors(), 0);
    let state = daemon.appender.live_state();
    assert!(
        state.agents.contains_key("worker-1"),
        "live_state must contain worker-1"
    );
}

/// End-to-end streaming ingest under concurrent snapshot readers.
fn bench_api_sock_streaming_under_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let daemon = Daemon::start(paths.clone(), config).unwrap();

    let stop_snapshots = Arc::new(AtomicBool::new(false));
    let trace_sock = paths.trace_sock();
    let stop_clone = stop_snapshots.clone();

    // Background thread continuously taking snapshots from trace.sock
    let snapshot_worker = std::thread::spawn(move || {
        let mut snap_count = 0;
        let mut buf = Vec::with_capacity(512 * 1024);
        while !stop_clone.load(Ordering::Acquire) {
            if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                buf.clear();
                let _ = stream.read_to_end(&mut buf);
                if !buf.is_empty() {
                    snap_count += 1;
                }
            }
            std::thread::yield_now();
        }
        snap_count
    });

    let stream = UnixStream::connect(paths.api_sock()).expect("Failed to connect to api.sock");
    let reader_stream = stream.try_clone().unwrap();

    let mut writer = BufWriter::with_capacity(128 * 1024, stream);
    let mut reader = BufReader::with_capacity(128 * 1024, reader_stream);

    let run_start = serde_json::json!({
        "input": {
            "type": "run_start",
            "provider": "claude",
            "session_id": "sess-bench-snap",
            "agent": "worker-snap"
        }
    });
    writer
        .write_all(serde_json::to_string(&run_start).unwrap().as_bytes())
        .unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"status\":\"ok\""));

    let num_inputs = if cfg!(debug_assertions) {
        10_000
    } else {
        100_000
    };

    let reader_handle = std::thread::spawn(move || {
        let mut resp_line = String::new();
        let mut count = 0;
        while count < num_inputs {
            resp_line.clear();
            if reader.read_line(&mut resp_line).unwrap() == 0 {
                break;
            }
            if resp_line.contains("\"status\":\"ok\"") {
                count += 1;
            }
        }
        count
    });

    let start = Instant::now();

    let begin_line: &[u8] = br#"{"input":{"type":"tool_begin","session":{"provider":"claude","session_id":"sess-bench-snap"},"tool_use_id":"tsnap-1","tool_name":"Bash","input":{"tool":"shell","argv":"echo 1"},"error":false}}"#;
    let end_line: &[u8] = br#"{"input":{"type":"tool_end","session":{"provider":"claude","session_id":"sess-bench-snap"},"tool_use_id":"tsnap-1","tool_name":"Bash","input":{"tool":"shell","argv":"echo 1"},"error":false}}"#;

    for i in 0..num_inputs {
        let line_bytes = if i % 2 == 0 { begin_line } else { end_line };
        writer.write_all(line_bytes).unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();

    let read_count = reader_handle.join().unwrap();
    let elapsed = start.elapsed();
    let throughput = (num_inputs as f64) / elapsed.as_secs_f64();

    stop_snapshots.store(true, Ordering::Release);
    let snaps_taken = snapshot_worker.join().unwrap();

    println!("============================================================");
    println!(
        "api.sock Streaming Under Concurrent Snapshots ({} inputs):",
        num_inputs
    );
    println!("Snapshots taken concurrently: {}", snaps_taken);
    println!("Responses received: {}", read_count);
    println!("Elapsed time: {:.3?}", elapsed);
    println!("Throughput: {:.0} inputs/sec", throughput);
    println!("============================================================");

    assert_eq!(read_count, num_inputs);

    daemon.appender.flush();
    assert_eq!(daemon.appender.write_errors(), 0);
    let state = daemon.appender.live_state();
    assert!(
        state.agents.contains_key("worker-snap"),
        "live_state must contain worker-snap"
    );
}

/// Helper to generate N test events in SQLite database.
fn populate_test_database(log: &mut EventLog, start_seq: u64, count: usize) {
    let mut batch = Vec::with_capacity(1000);
    for i in 0..count {
        let seq = start_seq + i as u64;
        let kind = if seq % 2 == 0 {
            Kind::ToolCall
        } else {
            Kind::ToolCallEnded
        };
        let ev = Event::new(
            seq,
            1_000_000_000 + seq * 1_000,
            kind,
            "agent:bench/run:1",
            if seq > 1 && seq % 2 == 1 {
                Some(seq - 1)
            } else {
                None
            },
            Some("agent:bench".to_string()),
            tacho::Event {
                body: Some(tacho::event::Body::Command(tacho::Command {
                    argv: Some("bench".to_string()),
                    ..Default::default()
                })),
            },
        );
        batch.push(ev);
        if batch.len() >= 1000 {
            log.insert_batch(batch.iter()).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        log.insert_batch(batch.iter()).unwrap();
    }
}

/// Benchmark snapshot scaling and peak RSS at 100k, 1M, and 10M rows,
/// plus appender throughput while a 10M-row snapshot runs.
fn bench_snapshot_scaling_and_peak_rss() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("scaling_events.sqlite");
    let mut log = EventLog::open(&db_path).unwrap();

    // Track creation event
    let track_ev = Event::new(
        1,
        1_000_000_000,
        Kind::TrackCreated,
        "daemon",
        None,
        None,
        tacho::Event {
            body: Some(tacho::event::Body::TrackCreated(tacho::TrackCreated {
                key: Some("agent:bench/run:1".to_string()),
                uuid: Some(12345),
                parent_uuid: None,
                name: Some("run:1".to_string()),
                collision: Some(false),
            })),
        },
    );
    log.insert_batch(&[track_ev]).unwrap();

    let scales: &[usize] = if cfg!(debug_assertions) {
        &[10_000, 100_000, 1_000_000]
    } else {
        &[100_000, 1_000_000, 10_000_000]
    };
    let mut current_rows: usize = 1;

    println!("\n================================================================================");
    println!("Snapshot Scaling and Peak RSS Benchmark:");
    println!("--------------------------------------------------------------------------------");
    println!("Rows       | Snapshot Time | Rate (rows/s) | Snapshot Size | Peak RSS (MB)");
    println!("--------------------------------------------------------------------------------");

    for &target_rows in scales {
        let needed = target_rows - current_rows;
        populate_test_database(&mut log, (current_rows + 1) as u64, needed);
        current_rows = target_rows;

        let _rss_before = get_peak_rss_mb();
        let start = Instant::now();

        let mut sink = io::sink();
        let stats = write_snapshot(&db_path, None, None, &mut sink).unwrap();
        let dur = start.elapsed();
        let rss_after = get_peak_rss_mb();

        let rate = (target_rows as f64) / dur.as_secs_f64();
        println!(
            "{:>10} | {:>11.3?} | {:>13.0} | {:>10.2} MB | {:>10.2} MB",
            target_rows,
            dur,
            rate,
            (stats.bytes as f64) / (1024.0 * 1024.0),
            rss_after
        );

        assert_eq!(stats.events, target_rows as u64);
    }
    println!("================================================================================\n");

    // Benchmark appender throughput while a 10M-row snapshot runs concurrently
    let appender_temp = tempfile::tempdir().unwrap();
    let home = appender_temp.path().join("home");
    let runtime = appender_temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    // Copy the 10M-row database to daemon home
    fs::copy(&db_path, paths.events_db()).unwrap();

    let daemon = Daemon::start(paths.clone(), config).unwrap();
    let stop_snap = Arc::new(AtomicBool::new(false));
    let db_path_clone = paths.events_db();
    let _stop_clone = stop_snap.clone();

    // Start background 10M-row snapshot
    let snap_handle = std::thread::spawn(move || {
        let mut sink = io::sink();
        let start = Instant::now();
        let res = write_snapshot(&db_path_clone, None, None, &mut sink);
        (start.elapsed(), res)
    });

    // Ingest inputs rapidly while snapshot runs
    let num_inputs = 50_000;
    let session = tacho_core::SessionKey::new("bench", "sess-10m");
    let start_ingest = Instant::now();

    for i in 0..num_inputs {
        let input = if i % 2 == 0 {
            Input::ToolBegin {
                source_ts_ns: Some(2_000_000_000 + i as u64),
                session: session.clone(),
                tool_use_id: format!("t-{}", i / 2),
                tool_name: "Bash".to_string(),
                input: tacho_core::ToolInput::Shell {
                    argv: "echo hi".to_string(),
                    cwd: None,
                    git_tree: None,
                },
            }
        } else {
            Input::ToolEnd {
                source_ts_ns: Some(2_000_000_000 + i as u64),
                session: session.clone(),
                tool_use_id: format!("t-{}", i / 2),
                result_bytes: Some(2),
                exit_code: Some(0),
                error: false,
            }
        };
        daemon
            .appender
            .send_input(input, 2_000_000_000 + i as u64)
            .unwrap();
    }
    daemon.appender.flush();
    let ingest_dur = start_ingest.elapsed();
    let ingest_rate = (num_inputs as f64) / ingest_dur.as_secs_f64();

    stop_snap.store(true, Ordering::Release);
    let (snap_dur, snap_res) = snap_handle.join().unwrap();
    let snap_stats = snap_res.expect("10M snapshot failed during concurrent ingest");

    println!("================================================================================");
    println!("Concurrent 10M-Row Snapshot & Appender Ingest Throughput:");
    println!("--------------------------------------------------------------------------------");
    println!("10M-Row Snapshot Duration: {:.3?}", snap_dur);
    println!("10M-Row Snapshot Events:   {}", snap_stats.events);
    println!(
        "Appender Ingest Rate:      {:.0} inputs/s while snapshot ran",
        ingest_rate
    );
    println!("================================================================================\n");

    assert_eq!(daemon.appender.write_errors(), 0);
    assert!(snap_stats.events >= current_rows as u64);
}

fn main() {
    if std::env::args().any(|a| a == "--bench") {
        bench_ingest_pipeline_stage_profiling();
        bench_api_sock_streaming_throughput();
        bench_api_sock_streaming_under_snapshots();
        bench_snapshot_scaling_and_peak_rss();
    }
}
