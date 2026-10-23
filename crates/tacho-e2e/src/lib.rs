//! Shared test infrastructure and oracle verification for `tacho-e2e`.
//!
//! Provides `TestEnv` for hermetic per-test sandbox isolation of `$TACHO_HOME`
//! and `$TACHO_RUNTIME`, process lifecycle controls (daemon PID discovery,
//! SIGKILL, SIGTERM), and oracle verification via `trace_processor_shell`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Locates the compiled `tacho` binary.
pub fn tacho_bin_path() -> PathBuf {
    if let Ok(p) = std::env::var("TACHO_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(|p| p.parent()) {
            let candidate = target_dir.join("tacho");
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(dir).join("debug/tacho");
        if candidate.is_file() {
            return candidate;
        }
    }

    // Workspace target fallback
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&manifest_dir);
    let ws_target = ws_root.join("target/debug/tacho");
    if ws_target.is_file() {
        return ws_target;
    }

    let status = Command::new("cargo")
        .args(["build", "-p", "tachograph", "--bin", "tacho"])
        .current_dir(ws_root)
        .status()
        .expect("failed to build tacho binary");
    assert!(
        status.success(),
        "cargo build -p tachograph --bin tacho failed"
    );

    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(|p| p.parent()) {
            let candidate = target_dir.join("tacho");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    ws_target
}

/// Locates the `trace_processor_shell` binary.
pub fn trace_processor_path() -> PathBuf {
    tacho_core::find_trace_processor()
        .expect("trace_processor_shell binary not found; run scripts/oracle.sh or set TACHO_TP")
}

fn find_binary(env_var: &str, bin_name: &str) -> Option<PathBuf> {
    if let Ok(val) = std::env::var(env_var) {
        let p = PathBuf::from(val.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(bin_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locates the `perfetto` binary.
pub fn perfetto_bin_path() -> Option<PathBuf> {
    find_binary("TACHO_PERFETTO", "perfetto")
}

/// Locates the `traced` binary.
pub fn traced_bin_path() -> Option<PathBuf> {
    find_binary("TACHO_TRACED", "traced")
}

/// Locates the `traced_probes` binary.
pub fn traced_probes_bin_path() -> Option<PathBuf> {
    find_binary("TACHO_TRACED_PROBES", "traced_probes")
}

/// Path to `tests/fixtures`.
pub fn fixtures_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("could not determine workspace root");
    ws_root.join("tests/fixtures")
}

/// Path to `queries/`.
pub fn queries_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("could not determine workspace root");
    ws_root.join("queries")
}

/// High-level oracle stats returned after analyzing a Perfetto trace.
#[derive(Debug, Clone, Default)]
pub struct OracleStats {
    pub slice_count: usize,
    pub track_count: usize,
    pub counter_count: usize,
    pub flow_count: usize,
    pub args_count: usize,
    pub error_stats: Vec<String>,
}

/// Hermetic per-test sandbox environment with private `$TACHO_HOME` and `$TACHO_RUNTIME`.
pub struct TestEnv {
    pub temp_dir: TempDir,
    pub home_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub tacho_bin: PathBuf,
    pub tp_bin: PathBuf,
}

impl TestEnv {
    /// Creates a fresh, isolated test environment.
    pub fn new() -> io::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let home_dir = temp_dir.path().join("home");
        let runtime_dir = temp_dir.path().join("run");

        fs::create_dir_all(&home_dir)?;
        fs::create_dir_all(&runtime_dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&home_dir, fs::Permissions::from_mode(0o700))?;
            fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
        }

        let tacho_bin = tacho_bin_path();
        let tp_bin = trace_processor_path();

        Ok(Self {
            temp_dir,
            home_dir,
            runtime_dir,
            tacho_bin,
            tp_bin,
        })
    }

    /// Prepares a `Command` targeting the `tacho` CLI with sandbox environment variables set.
    pub fn tacho_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.tacho_bin);
        cmd.env("TACHO_HOME", &self.home_dir)
            .env("TACHO_RUNTIME", &self.runtime_dir)
            .env("TACHO_BIN", &self.tacho_bin)
            .env("TACHO_TRACE_PROCESSOR", &self.tp_bin);

        // Prepend binary directory to PATH
        if let Some(parent) = self.tp_bin.parent() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("{}:{}", parent.display(), current_path);
            cmd.env("PATH", new_path);
        }

        cmd
    }

    /// Path to `api.sock` in this sandbox.
    pub fn api_sock(&self) -> PathBuf {
        self.runtime_dir.join("api.sock")
    }

    /// Path to `trace.sock` in this sandbox.
    pub fn trace_sock(&self) -> PathBuf {
        self.runtime_dir.join("trace.sock")
    }

    /// Path to SQLite WAL database in this sandbox (supports events.sqlite or events.db).
    pub fn events_db(&self) -> PathBuf {
        let sqlite = self.home_dir.join("events.sqlite");
        if sqlite.exists() {
            sqlite
        } else {
            self.home_dir.join("events.db")
        }
    }

    /// Path to `daemon.log`.
    pub fn daemon_log(&self) -> PathBuf {
        self.home_dir.join("daemon.log")
    }

    /// Attempts to find the PID of the daemon servicing this sandbox.
    pub fn daemon_pid(&self) -> Option<u32> {
        let pid_file = self.runtime_dir.join("daemon.pid");
        if let Ok(s) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = s.trim().parse::<u32>() {
                if pid > 0 && Self::is_pid_alive(pid) {
                    return Some(pid);
                }
            }
        }

        // Fallback: scan /proc for processes holding daemon.lock in this runtime_dir
        let lock_path = self.runtime_dir.join("daemon.lock");
        if let Ok(canonical_lock) = lock_path.canonicalize() {
            if let Ok(entries) = fs::read_dir("/proc") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if let Some(pid_str) = name.to_str() {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            let fd_dir = entry.path().join("fd");
                            if let Ok(fds) = fs::read_dir(fd_dir) {
                                for fd in fds.flatten() {
                                    if let Ok(target) = fs::read_link(fd.path()) {
                                        if target == canonical_lock {
                                            return Some(pid);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Checks if a PID is still actively running.
    pub fn is_pid_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Sends a signal to the daemon process if running.
    pub fn signal_daemon(&self, sig: libc::c_int) -> bool {
        if let Some(pid) = self.daemon_pid() {
            let res = unsafe { libc::kill(pid as libc::pid_t, sig) };
            res == 0
        } else {
            false
        }
    }

    /// Sends `SIGKILL` to the daemon.
    pub fn kill_daemon_9(&self) -> bool {
        self.signal_daemon(libc::SIGKILL)
    }

    /// Sends `SIGTERM` to the daemon.
    pub fn kill_daemon_term(&self) -> bool {
        self.signal_daemon(libc::SIGTERM)
    }

    /// Sends a flush command to `api.sock` and waits synchronously for the daemon to confirm
    /// all buffered inputs are persisted to SQLite.
    pub fn flush(&self) -> bool {
        let sock = self.api_sock();
        let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
            Ok(s) => s,
            Err(_) => return false,
        };
        use std::io::{BufRead, BufReader, Write};
        let _ = stream.write_all(b"{\"cmd\":\"flush\"}\n");
        let _ = stream.flush();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            line.contains("\"status\":\"ok\"")
        } else {
            false
        }
    }

    /// Waits deterministically for a condition to become true within `timeout`.
    pub fn wait_until<F>(&self, timeout: Duration, _step: Duration, mut f: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            std::thread::yield_now();
        }
        f()
    }

    /// Waits for `api.sock` to become available and responding using kernel inotify and connect checks.
    pub fn wait_for_api_sock(&self, timeout: Duration) -> bool {
        let effective_timeout = timeout.max(Duration::from_secs(30));
        let sock = self.api_sock();
        if sock.exists() && std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return true;
        }
        let parent = match sock.parent() {
            Some(p) => p,
            None => return false,
        };
        let _ = std::fs::create_dir_all(parent);
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd >= 0 {
            if let Ok(path_c) = std::ffi::CString::new(parent.to_str().unwrap_or_default()) {
                let wd = unsafe {
                    libc::inotify_add_watch(
                        inotify_fd,
                        path_c.as_ptr(),
                        libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ATTRIB,
                    )
                };
                let start = Instant::now();
                while start.elapsed() < effective_timeout {
                    if sock.exists() && std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                        if wd >= 0 {
                            unsafe { libc::inotify_rm_watch(inotify_fd, wd) };
                        }
                        unsafe { libc::close(inotify_fd) };
                        return true;
                    }
                    let remaining = effective_timeout
                        .saturating_sub(start.elapsed())
                        .as_millis()
                        .min(i32::MAX as u128) as i32;
                    let mut pfd = libc::pollfd {
                        fd: inotify_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut pfd, 1, remaining.max(10)) };
                    if rc > 0 {
                        let mut buf = [0u8; 1024];
                        let _ = unsafe {
                            libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                    }
                }
                if wd >= 0 {
                    unsafe { libc::inotify_rm_watch(inotify_fd, wd) };
                }
            }
            unsafe { libc::close(inotify_fd) };
        }
        sock.exists() && std::os::unix::net::UnixStream::connect(&sock).is_ok()
    }

    /// Queries the trace via `trace_processor_shell` using standard SQL through stdin.
    pub fn query_trace(&self, trace_path: &Path, sql: &str) -> io::Result<Vec<Vec<String>>> {
        let mut cmd = Command::new(&self.tp_bin);
        cmd.arg(trace_path)
            .arg("-q")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(sql.as_bytes())?;
            stdin.write_all(b"\n")?;
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "trace_processor_shell failed: {stderr}"
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut rows = Vec::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("Trace loaded:")
                || trimmed.starts_with("Query executed")
                || trimmed.starts_with('[')
            {
                continue;
            }
            // Parse CSV or TSV or space-delimited
            let cols: Vec<String> = if trimmed.contains(',') {
                trimmed
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .collect()
            } else if trimmed.contains('\t') {
                trimmed
                    .split('\t')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .collect()
            } else {
                trimmed
                    .split_whitespace()
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .collect()
            };
            rows.push(cols);
        }

        Ok(rows)
    }

    /// Evaluates oracle assertions on the trace file:
    /// 1. Zero `error` or `data_loss` stats rows with value > 0.
    /// 2. Returns counts of slices, tracks, counters, flows, args.
    pub fn run_oracle(&self, trace_path: &Path) -> io::Result<OracleStats> {
        let stats_sql = "SELECT name, value, severity FROM stats WHERE (severity = 'error' OR severity = 'data_loss') AND value > 0;";
        let rows = self.query_trace(trace_path, stats_sql)?;

        let mut error_stats = Vec::new();
        for r in rows {
            if !r.is_empty() && r[0] != "name" {
                error_stats.push(r.join(" | "));
            }
        }

        let slice_rows = self.query_trace(trace_path, "SELECT count(*) FROM slice;")?;
        let slice_count = slice_rows
            .into_iter()
            .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
            .and_then(|r| r[0].trim_matches('"').parse().ok())
            .unwrap_or(0);

        let track_rows = self.query_trace(trace_path, "SELECT count(*) FROM track;")?;
        let track_count = track_rows
            .into_iter()
            .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
            .and_then(|r| r[0].trim_matches('"').parse().ok())
            .unwrap_or(0);

        let counter_rows = self.query_trace(trace_path, "SELECT count(*) FROM counter;")?;
        let counter_count = counter_rows
            .into_iter()
            .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
            .and_then(|r| r[0].trim_matches('"').parse().ok())
            .unwrap_or(0);

        let flow_rows = self.query_trace(trace_path, "SELECT count(*) FROM flow;")?;
        let flow_count = flow_rows
            .into_iter()
            .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
            .and_then(|r| r[0].trim_matches('"').parse().ok())
            .unwrap_or(0);

        let args_rows = self.query_trace(trace_path, "SELECT count(*) FROM args;")?;
        let args_count = args_rows
            .into_iter()
            .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
            .and_then(|r| r[0].trim_matches('"').parse().ok())
            .unwrap_or(0);

        Ok(OracleStats {
            slice_count,
            track_count,
            counter_count,
            flow_count,
            args_count,
            error_stats,
        })
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Stop daemon if running
        if let Some(pid) = self.daemon_pid() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            let cap = std::time::Instant::now() + Duration::from_secs(2);
            while Self::is_pid_alive(pid) && std::time::Instant::now() < cap {
                std::thread::yield_now();
            }
            if Self::is_pid_alive(pid) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_binary_env_and_path() {
        let temp = tempfile::tempdir().unwrap();
        let fake_bin = temp.path().join("fake_traced");
        fs::write(&fake_bin, b"#!/bin/sh\n").unwrap();

        std::env::set_var("TACHO_TRACED", &fake_bin);
        assert_eq!(traced_bin_path(), Some(fake_bin));
        std::env::remove_var("TACHO_TRACED");

        assert_eq!(
            find_binary("TACHO_NONEXISTENT", "nonexistent_binary_xyz"),
            None
        );
    }
}
