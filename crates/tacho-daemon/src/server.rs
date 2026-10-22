//! UNIX domain socket servers for `api.sock` and `trace.sock`.
//!
//! Enforces single-instance locking via flock, handles stale socket cleanup,
//! serves `api.sock` using a bounded worker pool (8 threads), and serves
//! `trace.sock` via a dedicated single thread accessing only rings and registry.

use crossbeam_channel::{Receiver, Sender};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tacho_core::Input;

use crate::appender::AppenderHandle;
use crate::paths::DaemonPaths;
use crate::recorder::{render_report, render_status};
use crate::snapshot::write_snapshot;

/// Single-instance advisory lock on `$TACHO_RUNTIME/daemon.lock`.
pub struct DaemonLock {
    _file: fs::File,
}

impl DaemonLock {
    /// Attempts to acquire an exclusive, non-blocking advisory flock on `lock_path`.
    pub fn acquire(lock_path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if res != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another daemon instance holds lock {:?}", lock_path),
            ));
        }

        Ok(Self { _file: file })
    }
}

/// Running server handle managing background socket listener threads.
pub struct DaemonServer {
    paths: DaemonPaths,
    _lock: DaemonLock,
    shutdown_flag: Arc<AtomicBool>,
    api_thread: Option<std::thread::JoinHandle<()>>,
    trace_thread: Option<std::thread::JoinHandle<()>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
}

impl DaemonServer {
    /// Starts the servers on `api.sock` and `trace.sock`.
    pub fn start(
        paths: DaemonPaths,
        appender: AppenderHandle,
        shutdown_flag: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        // 1. Acquire single-instance flock
        let lock = DaemonLock::acquire(&paths.lock_file())?;

        // 2. Clean up any stale socket files from prior unclean shutdown
        if paths.api_sock().exists() {
            let _ = fs::remove_file(paths.api_sock());
        }
        if paths.trace_sock().exists() {
            let _ = fs::remove_file(paths.trace_sock());
        }

        // 3. Bind UNIX listeners
        let api_listener = UnixListener::bind(paths.api_sock())?;
        let trace_listener = UnixListener::bind(paths.trace_sock())?;

        // Set mode 0700 on sockets
        let _ = fs::set_permissions(paths.api_sock(), fs::Permissions::from_mode(0o700));
        let _ = fs::set_permissions(paths.trace_sock(), fs::Permissions::from_mode(0o700));

        // 4. Bounded thread pool for api.sock (8 worker threads)
        const API_WORKERS: usize = 8;
        let (conn_tx, conn_rx) = crossbeam_channel::bounded::<UnixStream>(128);
        let mut worker_threads = Vec::with_capacity(API_WORKERS);

        for i in 0..API_WORKERS {
            let rx = conn_rx.clone();
            let app = appender.clone();
            let stop = shutdown_flag.clone();
            let p = paths.clone();

            let worker = std::thread::Builder::new()
                .name(format!("tacho-api-worker-{}", i + 1))
                .spawn(move || {
                    run_api_worker(rx, app, stop, p);
                })
                .expect("failed to spawn api worker");
            worker_threads.push(worker);
        }

        // 5. Acceptor thread for api.sock
        let stop_api = shutdown_flag.clone();
        api_listener.set_nonblocking(true)?;
        let api_thread = std::thread::Builder::new()
            .name("tacho-api-acceptor".to_string())
            .spawn(move || {
                run_api_acceptor(api_listener, conn_tx, stop_api);
            })
            .expect("failed to spawn api acceptor");

        // 6. Dedicated thread for trace.sock (streaming from SQLite read-only WAL connection)
        let stop_trace = shutdown_flag.clone();
        trace_listener.set_nonblocking(true)?;
        let db_path_trace = paths.events_db();
        let trace_thread = std::thread::Builder::new()
            .name("tacho-trace-server".to_string())
            .spawn(move || {
                run_trace_server(trace_listener, db_path_trace, stop_trace);
            })
            .expect("failed to spawn trace server");

        Ok(Self {
            paths,
            _lock: lock,
            shutdown_flag,
            api_thread: Some(api_thread),
            trace_thread: Some(trace_thread),
            worker_threads,
        })
    }

    /// Stops servers, joins threads, and removes socket files.
    pub fn stop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);

        if let Some(t) = self.api_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.trace_thread.take() {
            let _ = t.join();
        }
        for w in self.worker_threads.drain(..) {
            let _ = w.join();
        }

        let _ = fs::remove_file(self.paths.api_sock());
        let _ = fs::remove_file(self.paths.trace_sock());
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Acceptor loop for api.sock.
fn run_api_acceptor(listener: UnixListener, conn_tx: Sender<UnixStream>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = conn_tx.send(stream);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

/// Dedicated single-thread server for trace.sock.
///
/// CRITICAL: This thread streams snapshots using its own independent read-only WAL connection.
/// It never touches the appender channel or locks, ensuring snapshots succeed even if the appender is wedged.
fn run_trace_server(listener: UnixListener, db_path: std::path::PathBuf, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = write_snapshot(&db_path, None, None, &mut stream);
                let _ = stream.flush();
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

/// Worker thread servicing api.sock requests.
fn run_api_worker(
    rx: Receiver<UnixStream>,
    appender: AppenderHandle,
    stop: Arc<AtomicBool>,
    paths: DaemonPaths,
) {
    while !stop.load(Ordering::Acquire) {
        let stream = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(s) => s,
            Err(_) => continue,
        };

        handle_api_connection(stream, &appender, &stop, &paths);
    }
}

/// Processes NDJSON requests on a client stream until disconnection.
fn handle_api_connection(
    stream: UnixStream,
    appender: &AppenderHandle,
    stop: &Arc<AtomicBool>,
    paths: &DaemonPaths,
) {
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::with_capacity(65536, reader_stream);
    let mut writer = io::BufWriter::with_capacity(65536, stream);
    let mut line_buf = String::with_capacity(1024);
    let mut input_batch: Vec<(Input, u64)> = Vec::with_capacity(128);

    while !stop.load(Ordering::Acquire) {
        line_buf.clear();
        match reader.read_line(&mut line_buf) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Fast path for batched streaming ingest inputs {"inputs": [...]}
        if trimmed.starts_with("{\"inputs\"") {
            if let Ok(batch) = serde_json::from_str::<IngestBatchFastPath>(trimmed) {
                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                for inp in batch.inputs {
                    input_batch.push((inp, now_ns));
                }
                if !input_batch.is_empty() {
                    if let Err(e) = appender.send_input_batch(std::mem::take(&mut input_batch)) {
                        eprintln!("[tacho-daemon] queue full: {}", e);
                    }
                }
                if writer.write_all(b"{\"status\":\"ok\"}\n").is_err() {
                    break;
                }
                if reader.buffer().is_empty() && writer.flush().is_err() {
                    break;
                }
                continue;
            }
        }

        // Fast path for streaming ingest inputs
        if trimmed.starts_with("{\"input\"") || trimmed.contains("\"input\"") {
            if let Ok(ingest) = serde_json::from_str::<IngestFastPath>(trimmed) {
                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                input_batch.push((ingest.input, now_ns));

                // If batch limit reached or socket buffer drained, dispatch to appender BEFORE sending ack
                if input_batch.len() >= 128 || reader.buffer().is_empty() {
                    if let Err(e) = appender.send_input_batch(std::mem::take(&mut input_batch)) {
                        eprintln!("[tacho-daemon] queue full: {}", e);
                    }
                }
                if writer.write_all(b"{\"status\":\"ok\"}\n").is_err() {
                    break;
                }
                if reader.buffer().is_empty() && writer.flush().is_err() {
                    break;
                }
                continue;
            }
        }

        // Drain any pending input batch before servicing control commands
        if !input_batch.is_empty() {
            let _ = appender.send_input_batch(std::mem::take(&mut input_batch));
        }

        let resp = process_ndjson_line(trimmed, appender, stop, writer.get_mut(), paths);
        if let Some(resp_bytes) = resp {
            if writer.write_all(&resp_bytes).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }
        }

        // Flush writer when incoming buffer has been drained
        if reader.buffer().is_empty() && writer.flush().is_err() {
            break;
        }
    }

    if !input_batch.is_empty() {
        let _ = appender.send_input_batch(input_batch);
    }
    let _ = writer.flush();
}

/// Helper struct for zero-overhead direct deserialization of ingest inputs.
#[derive(serde::Deserialize)]
struct IngestFastPath {
    input: Input,
}

/// Helper struct for batched ingest inputs over a single request line.
#[derive(serde::Deserialize)]
struct IngestBatchFastPath {
    inputs: Vec<Input>,
}

/// Handles a single line of NDJSON input.
fn process_ndjson_line(
    line: &str,
    appender: &AppenderHandle,
    stop: &Arc<AtomicBool>,
    stream: &mut UnixStream,
    paths: &DaemonPaths,
) -> Option<Vec<u8>> {
    // Fast path: Ingest input {"input": ...} bypasses Value AST creation
    if line.contains("\"input\"") {
        if let Ok(ingest) = serde_json::from_str::<IngestFastPath>(line) {
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            if let Err(e) = appender.send_input(ingest.input, now_ns) {
                return Some(format!(r#"{{"error":"queue full: {}"}}"#, e).into_bytes());
            }
            return Some(b"{\"status\":\"ok\"}".to_vec());
        }
    }

    let val: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(format!(r#"{{"error":"invalid JSON: {}"}}"#, e).into_bytes());
        }
    };

    // Case 1: Ingest input {"input": { ... }}
    if let Some(input_val) = val.get("input") {
        match serde_json::from_value::<Input>(input_val.clone()) {
            Ok(input) => {
                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                if let Err(e) = appender.send_input(input, now_ns) {
                    return Some(format!(r#"{{"error":"queue full: {}"}}"#, e).into_bytes());
                }
                return Some(b"{\"status\":\"ok\"}".to_vec());
            }
            Err(e) => {
                return Some(format!(r#"{{"error":"invalid Input schema: {}"}}"#, e).into_bytes());
            }
        }
    }

    // Case 2: Control command {"cmd": "..."}
    if let Some(cmd) = val.get("cmd").and_then(|v| v.as_str()) {
        match cmd {
            "ping" => Some(b"{\"pong\":true}".to_vec()),

            "status" => {
                let json = val.get("json").and_then(|v| v.as_bool()).unwrap_or(false);
                let state = appender.live_state();
                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let mut text = render_status(&state, json, now_ns);
                let db_size = fs::metadata(paths.events_db())
                    .map(|m| m.len())
                    .unwrap_or(0);
                if !json {
                    text.push_str(&format!("\nevents.sqlite: {} bytes\n", db_size));
                }
                let resp = serde_json::json!({
                    "status": "ok",
                    "output": text,
                    "events_sqlite_bytes": db_size,
                    "events_sqlite_size_bytes": db_size,
                });
                Some(serde_json::to_vec(&resp).unwrap_or_default())
            }

            "report" => {
                let agent = val.get("agent").and_then(|v| v.as_str()).unwrap_or("");
                let since = val.get("since").and_then(|v| v.as_u64());
                let state = appender.live_state();
                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let text = render_report(&state, agent, since, now_ns);
                let agent_state = state.agents.get(agent);
                let resp = serde_json::json!({
                    "status": "ok",
                    "output": text,
                    "agent_state": agent_state,
                });
                Some(serde_json::to_vec(&resp).unwrap_or_default())
            }

            "watch" => {
                // Streaming watch updates
                let until = val.get("until").and_then(|v| v.as_str());
                let _ = stream.write_all(b"{\"status\":\"watching\"}\n");
                let _ = stream.flush();

                let mut last_rendered = String::new();
                for _ in 0..100 {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let state = appender.live_state();
                    let now_ns = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    let current = render_status(&state, false, now_ns);

                    if current != last_rendered {
                        last_rendered = current.clone();
                        let event_line = serde_json::json!({
                            "event": "state_change",
                            "state": current,
                        });
                        let _ =
                            stream.write_all(&serde_json::to_vec(&event_line).unwrap_or_default());
                        let _ = stream.write_all(b"\n");
                        let _ = stream.flush();
                    }

                    if let Some(target) = until {
                        let is_match = state.agents.values().any(|a| {
                            a.state.eq_ignore_ascii_case(target)
                                || (target.eq_ignore_ascii_case("stalled")
                                    && (!a.flags.is_empty()
                                        || a.state.eq_ignore_ascii_case("stalled")))
                        });
                        if is_match {
                            let match_line = serde_json::json!({"matched": target});
                            let _ = stream
                                .write_all(&serde_json::to_vec(&match_line).unwrap_or_default());
                            let _ = stream.write_all(b"\n");
                            let _ = stream.flush();
                            break;
                        }
                    }

                    std::thread::sleep(Duration::from_millis(100));
                }
                None
            }

            "trace" => {
                let since = val.get("since").and_then(|v| v.as_u64());
                let until = val.get("until").and_then(|v| v.as_u64());
                let from_log = val
                    .get("from_log")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if from_log {
                    let _ =
                        crate::snapshot::rebuild_from_log(&paths.events_db(), since, until, stream);
                } else {
                    let _ = write_snapshot(&paths.events_db(), since, until, stream);
                }
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                None
            }

            "shutdown" => {
                stop.store(true, Ordering::Release);
                appender.shutdown();
                Some(b"{\"status\":\"ok\",\"message\":\"shutting down\"}".to_vec())
            }

            _ => Some(format!(r#"{{"error":"unknown command '{}'"}}"#, cmd).into_bytes()),
        }
    } else {
        Some(b"{\"error\":\"missing 'input' or 'cmd' in request\"}".to_vec())
    }
}
