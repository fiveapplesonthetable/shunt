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

use crate::appender::{spawn_appender_with_hook, AppenderHandle};
use crate::config::Config;
use crate::log::EventLog;
use crate::paths::DaemonPaths;
use crate::recorder::Recorder;
use crate::sampler::spawn_sampler;
use crate::server::DaemonServer;
use crate::snapshot::{write_snapshot, SnapshotStats};

/// Running daemon instance.
pub struct Daemon {
    pub paths: DaemonPaths,
    pub config: Config,
    pub appender: AppenderHandle,
    pub server: DaemonServer,
    pub shutdown_flag: Arc<AtomicBool>,
    otlp_thread: Option<JoinHandle<()>>,
    sampler_thread: Option<JoinHandle<()>>,
    retention_thread: Option<JoinHandle<()>>,
    appender_thread: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    signal_thread: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Initializes and starts all daemon subsystems per design section 1 and 8.
    pub fn start(paths: DaemonPaths, config: Config) -> io::Result<Self> {
        paths.ensure_dirs()?;

        // 1. Open SQLite WAL event log
        let log =
            EventLog::open(&paths.events_db()).map_err(|e| io::Error::other(e.to_string()))?;

        // 2. Restart recovery: load all logged events and restore recorder state (design test 7)
        let logged_events = log
            .load_all()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut recorder = Recorder::restore(logged_events.iter());
        recorder.set_detector_config(tacho_core::DetectorConfig {
            silent_timeout_ns: config.detectors.silent_s * 1_000_000_000,
            loop_repeats: config.detectors.loop_count,
            spin_turns: config.detectors.spin_turns,
            burn_tokens: config.detectors.burn_tokens,
            burn_window_ns: config.detectors.burn_window_s * 1_000_000_000,
            failing_repeats: config.detectors.failing_count,
        });

        let on_stall = config.detectors.on_stall.clone().or_else(|| {
            std::env::var("TACHO_ON_STALL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });

        // 3. Spawn appender thread (no in-memory rings; SQLite WAL is the sole source)
        let (appender, appender_thread) = spawn_appender_with_hook(recorder, log, 65_536, on_stall);

        let shutdown_flag = Arc::new(AtomicBool::new(false));

        // 4. Spawn sampler thread
        let sampler_thread = spawn_sampler(
            config.sampler.clone(),
            appender.clone(),
            shutdown_flag.clone(),
        );

        // 5. Start UNIX domain socket servers (api.sock, trace.sock)
        let server = DaemonServer::start(paths.clone(), appender.clone(), shutdown_flag.clone())?;

        // 6. Spawn retention maintenance thread (hourly small-batch prune)
        let retention_thread =
            spawn_retention_thread(config.retention_days, paths.clone(), shutdown_flag.clone());

        // 7. Register SIGINT / SIGTERM signal handler
        let signal_thread =
            spawn_signal_handler(shutdown_flag.clone(), paths.clone(), appender.clone());

        // 8. Optional OTLP HTTP receiver (design section 1 and 8)
        let otlp_thread = if let Some(ref addr_str) = config.otlp_addr {
            let listener = std::net::TcpListener::bind(addr_str)?;
            let shutdown_clone = shutdown_flag.clone();
            let appender_clone = appender.clone();
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
                    let _ =
                        tacho_ingest::otlp_server::serve_otlp(listener, shutdown_clone, on_inputs);
                })
                .map_err(|e| io::Error::other(e.to_string()))?;
            Some(handle)
        } else {
            None
        };

        Ok(Self {
            paths,
            config,
            appender,
            server,
            shutdown_flag,
            otlp_thread,
            sampler_thread: Some(sampler_thread),
            retention_thread: Some(retention_thread),
            appender_thread: Some(appender_thread),
            signal_thread: Some(signal_thread),
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

        // Flush and shutdown appender
        self.appender.flush();
        self.appender.shutdown();

        // Stop server and clean sockets
        self.server.stop();

        // Write final trace to last.perfetto-trace
        if let Ok(mut f) = fs::File::create(self.paths.last_trace()) {
            let _ = write_snapshot(&self.paths.events_db(), None, None, &mut f);
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
/// with a 50 ms yield between batches to guarantee zero blocking of the appender.
fn spawn_retention_thread(
    retention_days: u32,
    paths: DaemonPaths,
    stop_flag: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("tacho-retention".to_string())
        .spawn(move || {
            if retention_days == 0 {
                return;
            }

            let retention_ns = (retention_days as u64) * 86_400 * 1_000_000_000;

            while !stop_flag.load(Ordering::Acquire) {
                // Sleep in short intervals up to 1 hour (3600 seconds)
                for _ in 0..3600 {
                    if stop_flag.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }

                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;

                let cutoff_ns = now_ns.saturating_sub(retention_ns);

                if let Ok(mut log) = EventLog::open(&paths.events_db()) {
                    // Small-batch pruning: 1000 rows per batch, yielding between transactions
                    const BATCH_SIZE: usize = 1000;
                    loop {
                        if stop_flag.load(Ordering::Acquire) {
                            break;
                        }
                        match log.prune_older_than(cutoff_ns, BATCH_SIZE) {
                            Ok(0) => break, // Done
                            Ok(_) => {
                                std::thread::sleep(Duration::from_millis(50));
                            }
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
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("tacho-signals".to_string())
        .spawn(move || {
            let mut signals = match signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ]) {
                Ok(s) => s,
                Err(_) => return,
            };

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
        .expect("failed to spawn signal thread")
}
