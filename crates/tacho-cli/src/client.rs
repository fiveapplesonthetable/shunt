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
use std::time::Duration;

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
    /// and blocks on kernel inotify events or process exit (`pidfd`).
    pub fn connect_api(&self) -> io::Result<UnixStream> {
        let sock = self.api_sock();

        // 1. First fast attempt
        if let Ok(stream) = UnixStream::connect(&sock) {
            return Ok(stream);
        }

        ensure_dir_0700(&self.home_dir)?;
        ensure_dir_0700(&self.runtime_dir)?;
        let inotify = watch_dir(&self.runtime_dir).ok_or_else(|| {
            io::Error::other(format!(
                "inotify unavailable for watching runtime directory {:?}",
                self.runtime_dir
            ))
        })?;

        // 2. Daemon not responding; spawn it detached
        let mut spawned_child = self.spawn_daemon_detached()?;
        let child_pidfd = spawned_child.as_ref().and_then(|child| {
            use std::os::fd::FromRawFd;
            let raw =
                unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) as i32 };
            (raw >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
        });

        // 3. Block on kernel inotify events for api.sock or child pidfd exit (no timed poll).
        loop {
            if let Ok(stream) = UnixStream::connect(&sock) {
                return Ok(stream);
            }
            if let Some(ref mut child) = spawned_child {
                if let Ok(Some(status)) = child.try_wait() {
                    if let Ok(stream) = UnixStream::connect(&sock) {
                        return Ok(stream);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("tachograph daemon process exited prematurely with {status}"),
                    ));
                }
            } else if !self.is_daemon_or_spawn_locked() {
                if let Ok(stream) = UnixStream::connect(&sock) {
                    return Ok(stream);
                }
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!(
                        "tachograph daemon failed to start and respond at {:?}",
                        sock
                    ),
                ));
            }
            let mut pfds = [
                libc::pollfd {
                    fd: inotify.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: child_pidfd.as_ref().map_or(-1, |fd| fd.as_raw_fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: valid pollfd array; negative fd in pfds[1] is ignored by poll.
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            let mut buf = [0u8; 4096];
            // SAFETY: reading into a local buffer from a non-blocking fd we own.
            while unsafe { libc::read(inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0
            {
            }
        }
    }

    fn is_daemon_or_spawn_locked(&self) -> bool {
        for name in ["spawn.lock", "daemon.lock"] {
            let p = self.runtime_dir.join(name);
            if let Ok(f) = OpenOptions::new().read(true).write(true).open(&p) {
                let res = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if res != 0 {
                    return true;
                }
                let _ = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
            }
        }
        false
    }

    /// Tries connecting to `api.sock` immediately without spawning a daemon.
    pub fn try_connect_api_fast(&self) -> io::Result<UnixStream> {
        let sock = self.api_sock();
        UnixStream::connect(&sock)
    }

    /// Spawns `tacho daemon` in the background detached (`setsid`, stdio to `daemon.log`),
    /// returning immediately without waiting for initialization.
    /// Guarded by an O_EXCL/flock spawn lock in TACHO_RUNTIME and daemon.lock check.
    pub fn spawn_daemon_detached(&self) -> io::Result<Option<std::process::Child>> {
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
            return Ok(None);
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
                    return Ok(None);
                }
                let _ = unsafe { libc::flock(d_file.as_raw_fd(), libc::LOCK_UN) };
            }
        }

        // 3. Check if api.sock is already responding
        if self.try_connect_api_fast().is_ok() {
            return Ok(None);
        }

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.daemon_log())?;

        let exe = env::var_os("TACHO_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // If running within cargo test / cargo run, find `tacho` in target dir or PATH.
                // Never execute the test harness binary itself!
                if let Ok(cur) = env::current_exe() {
                    let mut dir = cur.parent();
                    while let Some(d) = dir {
                        let candidate = d.join("tacho");
                        if candidate.is_file() {
                            return candidate;
                        }
                        dir = d.parent();
                    }
                }
                PathBuf::from("tacho")
            });
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

        let child = cmd.spawn()?;
        Ok(Some(child))
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

    /// Sends a batch of `Input` events over a persistent `api.sock` connection.
    pub fn send_inputs_batch(&self, inputs: &[tacho_core::Input]) -> io::Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let mut stream = self.connect_api()?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut resp_line = String::new();

        for chunk in inputs.chunks(256) {
            let req = serde_json::json!({ "inputs": chunk });
            let line = serde_json::to_string(&req)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            stream.write_all(line.as_bytes())?;
            stream.write_all(b"\n")?;
            stream.flush()?;

            resp_line.clear();
            reader.read_line(&mut resp_line)?;
        }
        Ok(())
    }

    /// Fetches a trace snapshot from the daemon (streaming from `events.sqlite` WAL) and writes bytes to `w`.
    ///
    /// Connects to `trace.sock` first (when unfiltered and not `from_log`).
    /// Otherwise falls back to `api.sock`.
    pub fn fetch_trace(
        &self,
        since: Option<u64>,
        from_log: bool,
        w: &mut impl Write,
    ) -> io::Result<u64> {
        let trace_sock = self.trace_sock();

        // 1. Try dedicated trace.sock only if unfiltered and not requesting from_log rebuild
        if !from_log && since.is_none() {
            if let Ok(mut stream) = UnixStream::connect(&trace_sock) {
                let bytes_copied = io::copy(&mut stream, w)?;
                return Ok(bytes_copied);
            }
        }

        // 2. Fall back to api.sock if daemon is running (do not spawn daemon for read-only trace)
        let mut stream = self.try_connect_api_fast()?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
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

/// Opens a non-blocking inotify fd watching `dir` for new entries, or `None` if unavailable.
pub(crate) fn watch_dir(dir: &std::path::Path) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let path_c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: plain syscall; the result is checked before being wrapped.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return None;
    }
    // SAFETY: `raw` is a fresh fd we exclusively own.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    let mask = libc::IN_CREATE
        | libc::IN_MOVED_TO
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_CLOSE_NOWRITE
        | libc::IN_DELETE;
    // SAFETY: valid fd and NUL-terminated path.
    let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path_c.as_ptr(), mask) };
    (wd >= 0).then_some(fd)
}
