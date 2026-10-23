//! High-level lifecycle orchestrator for the tachograph flight recorder daemon.
//!
//! Ties together paths, configuration, WAL event log, appender thread,
//! sampler thread, socket servers, retention pruning, signal handling, and clean shutdown.

use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::appender::{spawn_appender_with_hook_timeout, AppenderHandle};
use crate::config::Config;
use crate::log::EventLog;
use crate::paths::DaemonPaths;
use crate::recorder::Recorder;
use crate::sampler::spawn_sampler;
use crate::server::{DaemonLock, DaemonServer};
use crate::snapshot::{write_snapshot, SnapshotStats};

/// Running daemon instance.
pub struct Daemon {
    pub paths: DaemonPaths,
    pub config: Config,
    pub appender: AppenderHandle,
    pub server: DaemonServer,
    pub shutdown_flag: Arc<AtomicBool>,
    retention_wake_tx: Option<crossbeam_channel::Sender<()>>,
    signal_handle: Option<signal_hook::iterator::Handle>,
    otlp_thread: Option<JoinHandle<()>>,
    otlp_shutdown_fds: Option<(std::os::unix::io::RawFd, std::os::unix::io::RawFd)>,
    sampler_thread: Option<JoinHandle<()>>,
    retention_thread: Option<JoinHandle<()>>,
    appender_thread: Option<JoinHandle<()>>,
    signal_thread: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Initializes and starts all daemon subsystems per design section 1 and 8.
    pub fn start(paths: DaemonPaths, config: Config) -> io::Result<Self> {
        paths.ensure_dirs()?;

        // 0. Acquire single-instance advisory locks on both $TACHO_RUNTIME/daemon.lock and
        // $TACHO_HOME/daemon.lock BEFORE opening SQLite WAL or spawning background threads.
        let lock = DaemonLock::acquire_paths(&paths)?;
        let _ = fs::write(
            paths.runtime.join("daemon.pid"),
            std::process::id().to_string(),
        );

        // 1. Open SQLite WAL event log
        let log =
            EventLog::open(&paths.events_db()).map_err(|e| io::Error::other(e.to_string()))?;

        // 2. Restart recovery: load all logged events and restore recorder state (design test 7)
        let logged_events = log
            .load_all()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut recorder = Recorder::restore(logged_events.iter());
        recorder.set_scrub(config.scrub);
        recorder.set_detector_config(tacho_core::DetectorConfig {
            silent_timeout_ns: config.detectors.silent_s * 1_000_000_000,
            loop_repeats: config.detectors.loop_count,
            spin_turns: config.detectors.spin_turns,
            burn_tokens: config.detectors.burn_tokens,
            burn_window_ns: config.detectors.burn_window_s * 1_000_000_000,
            failing_repeats: config.detectors.failing_count,
        });

        let on_stall = config.detectors.on_stall.clone();
        let on_stall_timeout = Duration::from_secs(config.detectors.on_stall_timeout_secs);

        // 3. Spawn appender thread (no in-memory rings; SQLite WAL is the sole source)
        let (appender, appender_thread) =
            spawn_appender_with_hook_timeout(recorder, log, 65_536, on_stall, on_stall_timeout);

        let shutdown_flag = Arc::new(AtomicBool::new(false));

        // 4. Spawn sampler thread
        let sampler_thread = spawn_sampler(
            config.sampler.clone(),
            appender.clone(),
            shutdown_flag.clone(),
        );

        // 5. Start UNIX domain socket servers (api.sock, trace.sock) using the acquired lock
        let server = DaemonServer::start_with_lock(
            paths.clone(),
            lock,
            appender.clone(),
            shutdown_flag.clone(),
        )?;

        // 6. Spawn retention maintenance thread (hourly small-batch prune)
        let (retention_wake_tx, retention_wake_rx) = crossbeam_channel::bounded::<()>(1);
        let retention_thread = spawn_retention_thread(
            config.retention_days,
            paths.clone(),
            shutdown_flag.clone(),
            retention_wake_rx,
        );

        // 7. Register SIGINT / SIGTERM signal handler
        let (signal_handle, signal_thread) =
            spawn_signal_handler(shutdown_flag.clone(), paths.clone(), appender.clone());

        // 8. Optional OTLP HTTP receiver (design section 1 and 8)
        let (otlp_thread, otlp_shutdown_fds) = if let Some(ref addr_str) = config.otlp_addr {
            let listener = std::net::TcpListener::bind(addr_str)?;
            let shutdown_clone = shutdown_flag.clone();
            let appender_clone = appender.clone();
            let mut fds = [0i32; 2];
            unsafe {
                libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK);
            }
            let otlp_rx = fds[0];
            let otlp_tx = fds[1];
            let otlp_cfg = tacho_ingest::otlp_server::OtlpServerConfig {
                shutdown_fd: Some(otlp_rx),
                ..Default::default()
            };
            let handle = std::thread::Builder::new()
                .name("tacho-otlp".to_string())
                .spawn(move || {
                    let on_inputs = move |inputs: Vec<tacho_core::Input>| {
                        let now_ns = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        let batch: Vec<(tacho_core::Input, u64)> =
                            inputs.into_iter().map(|inp| (inp, now_ns)).collect();
                        let _ = appender_clone.send_input_batch(batch);
                    };
                    let _ = tacho_ingest::otlp_server::serve_otlp_with_config(
                        listener,
                        shutdown_clone,
                        otlp_cfg,
                        on_inputs,
                    );
                    if otlp_rx >= 0 {
                        unsafe { libc::close(otlp_rx) };
                    }
                })
                .map_err(|e| io::Error::other(e.to_string()))?;
            (Some(handle), Some((otlp_rx, otlp_tx)))
        } else {
            (None, None)
        };

        Ok(Self {
            paths,
            config,
            appender,
            server,
            shutdown_flag,
            retention_wake_tx: Some(retention_wake_tx),
            signal_handle,
            otlp_thread,
            otlp_shutdown_fds,
            sampler_thread: Some(sampler_thread),
            retention_thread: Some(retention_thread),
            appender_thread: Some(appender_thread),
            signal_thread,
        })
    }

    /// Captures an on-demand snapshot directly to `w`.
    pub fn snapshot(&self, since: Option<u64>, w: &mut impl Write) -> io::Result<SnapshotStats> {
        write_snapshot(&self.paths.events_db(), since, None, w)
    }

    /// Rebuilds a trace from the SQLite log.
    pub fn rebuild_from_log(
        &self,
        since: Option<u64>,
        until: Option<u64>,
        w: &mut impl Write,
    ) -> io::Result<SnapshotStats> {
        write_snapshot(&self.paths.events_db(), since, until, w)
    }

    /// Cleanly stops all daemon threads and writes final snapshot.
    pub fn stop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        drop(self.retention_wake_tx.take());
        if let Some(h) = self.signal_handle.take() {
            h.close();
        }

        // Flush and shutdown appender
        self.appender.flush();
        self.appender.shutdown();

        // Stop server and clean sockets
        self.server.stop();

        // Write final trace to last.perfetto-trace
        if let Ok(mut f) = fs::File::create(self.paths.last_trace()) {
            let _ = write_snapshot(&self.paths.events_db(), None, None, &mut f);
        }

        if let Some((_rx, tx)) = self.otlp_shutdown_fds.take() {
            if tx >= 0 {
                unsafe { libc::close(tx) };
            }
        }
        if let Some(t) = self.otlp_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.retention_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.sampler_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.appender_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.signal_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns the retention pruning background thread.
///
/// Runs hourly, deleting rows older than `retention_days` in small batches of 1,000
/// while listening on `wake_rx` for immediate shutdown notification.
fn spawn_retention_thread(
    retention_days: u32,
    paths: DaemonPaths,
    stop_flag: Arc<AtomicBool>,
    wake_rx: crossbeam_channel::Receiver<()>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("tacho-retention".to_string())
        .spawn(move || {
            if retention_days == 0 {
                return;
            }

            let retention_ns = (retention_days as u64) * 86_400 * 1_000_000_000;

            while !stop_flag.load(Ordering::Acquire) {
                match wake_rx.recv_timeout(Duration::from_secs(3600)) {
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
                if stop_flag.load(Ordering::Acquire) {
                    return;
                }

                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;

                let cutoff_ns = now_ns.saturating_sub(retention_ns);
                let cutoff_secs = (cutoff_ns / 1_000_000_000) as i64;
                if let Ok(mut store) = crate::fleet::FleetStore::open(&paths.events_db()) {
                    let _ = store.prune_decision_ticks_older_than(cutoff_secs);
                }

                if let Ok(mut log) = EventLog::open(&paths.events_db()) {
                    // Small-batch pruning: 1000 rows per batch, yielding between transactions
                    const BATCH_SIZE: usize = 1000;
                    loop {
                        if stop_flag.load(Ordering::Acquire) {
                            break;
                        }
                        match log.prune_older_than(cutoff_ns, BATCH_SIZE) {
                            Ok(0) => break, // Done
                            Ok(_) => match wake_rx.recv_timeout(Duration::from_millis(50)) {
                                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                    return;
                                }
                                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                            },
                            Err(e) => {
                                eprintln!("[tacho-daemon] retention prune error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        })
        .expect("failed to spawn retention thread")
}

/// Spawns background signal handler thread for clean SIGINT / SIGTERM handling.
fn spawn_signal_handler(
    shutdown_flag: Arc<AtomicBool>,
    paths: DaemonPaths,
    appender: AppenderHandle,
) -> (
    Option<signal_hook::iterator::Handle>,
    Option<JoinHandle<()>>,
) {
    let mut signals = match signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ]) {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    let handle = signals.handle();
    let thread = std::thread::Builder::new()
        .name("tacho-signals".to_string())
        .spawn(move || {
            if let Some(_sig) = signals.forever().next() {
                shutdown_flag.store(true, Ordering::Release);
                appender.flush();

                // Save final trace
                if let Ok(mut f) = fs::File::create(paths.last_trace()) {
                    let _ = write_snapshot(&paths.events_db(), None, None, &mut f);
                }

                let _ = fs::remove_file(paths.api_sock());
                let _ = fs::remove_file(paths.trace_sock());
            }
        })
        .expect("failed to spawn signal thread");
    (Some(handle), Some(thread))
}
