//! Client connection and autostart management for the tachograph daemon.
//!
//! Connects to `$TACHO_RUNTIME/api.sock`. If absent, automatically spawns
//! `tacho daemon` detached (`setsid`, stdio to `$TACHO_HOME/daemon.log`)
//! and retries with exponential backoff up to a deadline.
//! Trace snapshotting connects to `trace.sock` with fallback to `api.sock`.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::paths::{ensure_dir_0700, resolve_home, resolve_runtime};

/// Client for communicating with the local tachograph flight recorder daemon.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    home_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl Default for DaemonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonClient {
    /// Creates a new `DaemonClient` with resolved paths.
    pub fn new() -> Self {
        Self {
            home_dir: resolve_home(),
            runtime_dir: resolve_runtime(),
        }
    }

    /// Creates a client with explicit custom home and runtime directories (useful for testing).
    pub fn custom(home: impl Into<PathBuf>, runtime: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home.into(),
            runtime_dir: runtime.into(),
        }
    }

    /// Path to `api.sock`.
    pub fn api_sock(&self) -> PathBuf {
        self.runtime_dir.join("api.sock")
    }

    /// Path to `trace.sock`.
    pub fn trace_sock(&self) -> PathBuf {
        self.runtime_dir.join("trace.sock")
    }

    /// Path to `daemon.log`.
    pub fn daemon_log(&self) -> PathBuf {
        self.home_dir.join("daemon.log")
    }

    /// Connects to `api.sock`. If the daemon is not running, launches it detached
    /// and waits with exponential backoff up to a 3-second deadline.
    pub fn connect_api(&self) -> io::Result<UnixStream> {
        let sock = self.api_sock();

        // 1. First fast attempt
        if let Ok(stream) = UnixStream::connect(&sock) {
            return Ok(stream);
        }

        // 2. Daemon not responding; spawn it detached
        self.spawn_daemon_detached()?;

        // 3. Retry connection with exponential backoff up to 2s
        let deadline = Instant::now() + Duration::from_millis(2000);
        let mut delay = Duration::from_millis(10);

        while Instant::now() < deadline {
            thread::sleep(delay);
            if let Ok(stream) = UnixStream::connect(&sock) {
                return Ok(stream);
            }
            delay = (delay * 2).min(Duration::from_millis(150));
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "tachograph daemon failed to start and respond at {:?} within 2s",
                sock
            ),
        ))
    }

    /// Tries connecting to `api.sock` immediately without spawning a daemon.
    pub fn try_connect_api_fast(&self) -> io::Result<UnixStream> {
        let sock = self.api_sock();
        UnixStream::connect(&sock)
    }

    /// Spawns `tacho daemon` in the background detached (`setsid`, stdio to `daemon.log`),
    /// returning immediately without waiting for initialization.
    /// Guarded by an O_EXCL/flock spawn lock in TACHO_RUNTIME and daemon.lock check.
    pub fn spawn_daemon_detached(&self) -> io::Result<()> {
        ensure_dir_0700(&self.home_dir)?;
        ensure_dir_0700(&self.runtime_dir)?;

        // 1. Guard spawning with flock on spawn.lock in TACHO_RUNTIME
        let spawn_lock_path = self.runtime_dir.join("spawn.lock");
        let spawn_lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&spawn_lock_path)?;

        let lock_res =
            unsafe { libc::flock(spawn_lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_res != 0 {
            // Another process is already spawning the daemon
            return Ok(());
        }

        // 2. Check if daemon is already running (holding daemon.lock)
        let daemon_lock_path = self.runtime_dir.join("daemon.lock");
        if daemon_lock_path.exists() {
            if let Ok(d_file) = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&daemon_lock_path)
            {
                let d_res =
                    unsafe { libc::flock(d_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if d_res != 0 {
                    // Daemon is already running (holding daemon.lock)
                    return Ok(());
                }
                let _ = unsafe { libc::flock(d_file.as_raw_fd(), libc::LOCK_UN) };
            }
        }

        // 3. Check if api.sock is already responding
        if self.try_connect_api_fast().is_ok() {
            return Ok(());
        }

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.daemon_log())?;

        let exe = env::var_os("TACHO_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_exe().unwrap_or_default());
        let mut cmd = Command::new(exe);
        cmd.arg("daemon")
            .env("TACHO_HOME", &self.home_dir)
            .env("TACHO_RUNTIME", &self.runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));

        unsafe {
            cmd.pre_exec(|| {
                // Detach from parent session and terminal
                libc::setsid();
                Ok(())
            });
        }

        cmd.spawn()?;
        Ok(())
    }

    /// Sends a single NDJSON request to `api.sock` and reads a single JSON response.
    pub fn send_request(&self, req: &serde_json::Value) -> io::Result<serde_json::Value> {
        let mut stream = self.connect_api()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let line = serde_json::to_string(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line)?;

        serde_json::from_str(&resp_line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Fetches an in-memory trace snapshot from the daemon and streams bytes to `w`.
    ///
    /// Connects to `trace.sock` first. If unavailable, falls back to `api.sock`.
    pub fn fetch_trace(
        &self,
        since: Option<u64>,
        from_log: bool,
        w: &mut impl Write,
    ) -> io::Result<u64> {
        let trace_sock = self.trace_sock();

        // 1. Try dedicated trace.sock only if not requesting from_log rebuild
        if !from_log {
            if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                let bytes_copied = io::copy(&mut stream, w)?;
                return Ok(bytes_copied);
            }
        }

        // 2. Fall back to api.sock
        let mut stream = self.connect_api()?;
        stream.set_read_timeout(Some(Duration::from_millis(2000)))?;
        stream.set_write_timeout(Some(Duration::from_millis(2000)))?;
        let req = serde_json::json!({
            "cmd": "trace",
            "since": since,
            "from_log": from_log,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        // Stream binary output until EOF
        let bytes_copied = io::copy(&mut stream, w)?;
        Ok(bytes_copied)
    }
}
