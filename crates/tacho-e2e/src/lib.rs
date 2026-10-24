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

    /// Blocks until `f()` holds, woken by inotify events on the sandbox
    /// directories (`home_dir` and `runtime_dir`), bounded only by
    /// [`SUITE_SAFETY_CAP`].
    pub fn wait_until<F>(&self, mut f: F) -> bool
    where
        F: FnMut() -> bool,
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        if f() {
            return true;
        }
        let deadline = Instant::now() + SUITE_SAFETY_CAP;
        // SAFETY: plain syscall.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return f();
        }
        // SAFETY: `raw` is a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mask = libc::IN_MODIFY
            | libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_DELETE;
        for dir in [&self.home_dir, &self.runtime_dir] {
            if let Ok(c_dir) = std::ffi::CString::new(dir.as_os_str().as_bytes()) {
                // SAFETY: valid fd and NUL-terminated path.
                unsafe {
                    libc::inotify_add_watch(fd.as_raw_fd(), c_dir.as_ptr(), mask);
                }
            }
        }
        loop {
            if f() {
                return true;
            }
            if !poll_readable(fd.as_raw_fd(), Some(deadline)) {
                return f();
            }
            let mut buf = [0u8; 4096];
            // SAFETY: valid fd and buffer; drains pending events.
            while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
    }

    /// Blocks until process `pid` exits, woken by its pidfd. Returns `true`
    /// once it is gone (including if it was already gone), `false` only if
    /// the safety cap [`SUITE_SAFETY_CAP`] expires first.
    pub fn wait_pid_exit(pid: u32) -> bool {
        match PidFd::open(pid) {
            Some(fd) => fd.wait_exit(Some(SUITE_SAFETY_CAP)),
            None => true,
        }
    }

    /// Blocks until `path` exists, woken by inotify on its parent directory.
    /// Returns `false` only if the safety cap expires first.
    pub fn wait_for_path(path: &Path) -> bool {
        wait_for_path_until(path, Instant::now() + SUITE_SAFETY_CAP, || true)
    }

    /// Waits for `api.sock` to appear (inotify) and accept a connection.
    ///
    /// The wait is event-driven and bounded only by [`SUITE_SAFETY_CAP`], so a
    /// slow machine cannot turn it into a spurious failure.
    pub fn wait_for_api_sock(&self) -> bool {
        let sock = self.api_sock();
        if let Some(parent) = sock.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let connectable = || std::os::unix::net::UnixStream::connect(&sock).is_ok();
        wait_for_path_until(&sock, Instant::now() + SUITE_SAFETY_CAP, connectable)
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

        let count = |table: &str| -> io::Result<usize> {
            let rows = self.query_trace(trace_path, &format!("SELECT count(*) FROM {table};"))?;
            Ok(rows
                .into_iter()
                .find(|r| !r.is_empty() && r[0].trim_matches('"') != "count(*)")
                .and_then(|r| r[0].trim_matches('"').parse().ok())
                .unwrap_or(0))
        };
        let slice_count = count("slice")?;
        let track_count = count("track")?;
        let counter_count = count("counter")?;
        let flow_count = count("flow")?;
        let args_count = count("args")?;

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

/// Upper bound on any single event-driven wait. It is a hang guard, not a
/// correctness timeout: events normally arrive in milliseconds.
pub const SUITE_SAFETY_CAP: Duration = Duration::from_secs(300);

/// An owned pidfd: signals and exit waits that cannot hit a recycled pid.
pub struct PidFd(std::os::fd::OwnedFd);

impl PidFd {
    /// Opens a pidfd for `pid`; `None` if the process no longer exists.
    pub fn open(pid: u32) -> Option<Self> {
        use std::os::fd::FromRawFd;
        let pid = libc::pid_t::try_from(pid).ok().filter(|&p| p > 0)?;
        // SAFETY: plain syscall; a non-negative result is a fresh fd we own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` was just returned by pidfd_open.
        Some(Self(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }))
    }

    /// Sends `sig`; returns whether the kernel accepted it.
    pub fn signal(&self, sig: libc::c_int) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: valid pidfd, null siginfo.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            ) == 0
        }
    }

    /// Blocks until the process exits (pidfd readable). `cap` of `None`
    /// waits forever. Returns `false` if the cap expired first.
    pub fn wait_exit(&self, cap: Option<Duration>) -> bool {
        use std::os::fd::AsRawFd;
        poll_readable(self.0.as_raw_fd(), cap.map(|c| Instant::now() + c))
    }
}

/// Polls `fd` for readability, retrying on EINTR, until `deadline`.
fn poll_readable(fd: libc::c_int, deadline: Option<Instant>) -> bool {
    loop {
        let timeout_ms = match deadline {
            None => -1,
            Some(d) => d
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as libc::c_int,
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            return true;
        }
        if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Blocks until `path` exists and `ready()` holds, re-checking on every
/// inotify event in the parent directory. The watch is installed before the
/// first check, so a creation in between cannot be missed.
fn wait_for_path_until(path: &Path, deadline: Instant, mut ready: impl FnMut() -> bool) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let check = |ready: &mut dyn FnMut() -> bool| path.exists() && ready();
    let Some(parent) = path.parent() else {
        return check(&mut ready);
    };
    // SAFETY: plain syscall.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return check(&mut ready);
    }
    // SAFETY: `raw` is a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let Ok(c_parent) = std::ffi::CString::new(parent.as_os_str().as_bytes()) else {
        return check(&mut ready);
    };
    // SAFETY: valid fd and NUL-terminated path.
    let wd = unsafe {
        libc::inotify_add_watch(
            fd.as_raw_fd(),
            c_parent.as_ptr(),
            libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ATTRIB | libc::IN_CLOSE_WRITE,
        )
    };
    if wd < 0 {
        return check(&mut ready);
    }
    loop {
        if check(&mut ready) {
            return true;
        }
        if !poll_readable(fd.as_raw_fd(), Some(deadline)) {
            return check(&mut ready);
        }
        let mut buf = [0u8; 4096];
        // SAFETY: valid fd and buffer; drains pending events.
        while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Stop daemon if running
        // Signal through a pidfd so a recycled pid is never hit, and wait on
        // the pidfd instead of spinning on kill(pid, 0).
        if let Some(fd) = self.daemon_pid().and_then(PidFd::open) {
            fd.signal(libc::SIGTERM);
            fd.signal(libc::SIGCONT);
            if !fd.wait_exit(Some(SUITE_SAFETY_CAP)) {
                fd.signal(libc::SIGKILL);
                fd.wait_exit(Some(SUITE_SAFETY_CAP));
            }
        }
    }
}

/// Perfetto `TrackEvent::Type` wire values.
pub const TE_SLICE_BEGIN: i32 = 1;
pub const TE_SLICE_END: i32 = 2;
pub const TE_INSTANT: i32 = 3;
pub const TE_COUNTER: i32 = 4;
pub const TE_STATE: i32 = 5;

/// Flow ordinal used by `SnapshotProjector` for inter-agent message delivery (`seq * 256 + 200`).
pub const FLOW_ORD_MESSAGE_DELIVERY: u64 = 200;

/// One decoded `TrackEvent` packet from a `.perfetto-trace` file.
#[derive(Debug, Clone)]
pub struct DecodedTrackEvent {
    pub track_uuid: u64,
    pub ty: i32,
    pub ts: u64,
    pub name: String,
    pub counter_value: Option<i64>,
    pub flow_ids: Vec<u64>,
    pub terminating_flow_ids: Vec<u64>,
}

/// A matched (or unclosed) slice on a track.
#[derive(Debug, Clone)]
pub struct DecodedSlice {
    pub track_path: String,
    pub name: String,
    pub begin_ts: u64,
    pub end_ts: Option<u64>,
    pub flow_ids: Vec<u64>,
    pub terminating_flow_ids: Vec<u64>,
}

/// Decoded Perfetto trace model for structural assertions on tracks, state events,
/// step-function counters, flow arrows, and slice boundaries.
pub struct DecodedTrace {
    /// `uuid -> (name, parent_uuid)`
    pub tracks: std::collections::HashMap<u64, (String, Option<u64>)>,
    pub events: Vec<DecodedTrackEvent>,
}

impl DecodedTrace {
    /// Reads and decodes a `.perfetto-trace` file from disk.
    pub fn from_file(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        Ok(Self::from_bytes(&bytes))
    }

    /// Decodes a `.perfetto-trace` byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use prost::Message;
        use std::collections::HashMap;
        use tacho_proto::perfetto::protos::{self as pb, trace_packet::Data, track_event};

        let trace = pb::Trace::decode(bytes).expect("Perfetto trace bytes must decode cleanly");
        let mut tracks = HashMap::new();
        let mut names: HashMap<u64, String> = HashMap::new();
        let mut events = Vec::new();

        for packet in trace.packet {
            if let Some(interned) = &packet.interned_data {
                for n in &interned.event_names {
                    names.insert(n.iid.unwrap_or(0), n.name.clone().unwrap_or_default());
                }
            }
            match packet.data {
                Some(Data::TrackDescriptor(d)) => {
                    let name = match d.static_or_dynamic_name {
                        Some(pb::track_descriptor::StaticOrDynamicName::Name(n))
                        | Some(pb::track_descriptor::StaticOrDynamicName::StaticName(n)) => n,
                        _ => String::new(),
                    };
                    tracks.insert(d.uuid.unwrap_or(0), (name, d.parent_uuid));
                }
                Some(Data::TrackEvent(te)) => {
                    let name = match te.name_field {
                        Some(track_event::NameField::Name(n)) => n,
                        Some(track_event::NameField::NameIid(i)) => {
                            names.get(&i).cloned().unwrap_or_default()
                        }
                        None => String::new(),
                    };
                    let counter_value = match te.counter_value_field {
                        Some(track_event::CounterValueField::CounterValue(v)) => Some(v),
                        Some(track_event::CounterValueField::DoubleCounterValue(d)) => {
                            Some(d as i64)
                        }
                        None => None,
                    };
                    events.push(DecodedTrackEvent {
                        track_uuid: te.track_uuid.unwrap_or(0),
                        ty: te.r#type.unwrap_or(0),
                        ts: packet.timestamp.unwrap_or(0),
                        name,
                        counter_value,
                        flow_ids: te.flow_ids,
                        terminating_flow_ids: te.terminating_flow_ids,
                    });
                }
                _ => {}
            }
        }

        Self { tracks, events }
    }

    /// Slash-joined display path of a track UUID, e.g. `tacho/agents/worker-auth/run 1/turns`.
    pub fn path(&self, uuid: u64) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(uuid);
        let mut visited = std::collections::HashSet::new();
        while let Some(u) = cur {
            if !visited.insert(u) {
                break;
            }
            let Some((name, parent)) = self.tracks.get(&u) else {
                break;
            };
            parts.push(name.clone());
            cur = *parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Finds a track UUID by its exact display path.
    pub fn track_by_path(&self, path: &str) -> Option<u64> {
        self.tracks.keys().copied().find(|&u| self.path(u) == path)
    }

    /// Returns all track display paths rooted under `tacho`.
    pub fn tacho_track_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .tracks
            .keys()
            .map(|&u| self.path(u))
            .filter(|p| p == "tacho" || p.starts_with("tacho/"))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Returns all `TYPE_STATE` (`ty == 5`) events on `track_path`.
    pub fn state_events_on(&self, track_path: &str) -> Vec<&DecodedTrackEvent> {
        let Some(uuid) = self.track_by_path(track_path) else {
            return Vec::new();
        };
        self.events
            .iter()
            .filter(|e| e.track_uuid == uuid && e.ty == TE_STATE)
            .collect()
    }

    /// Returns all `(ts, counter_value)` samples (`ty == 4`) on `track_path`.
    pub fn counter_samples_on(&self, track_path: &str) -> Vec<(u64, i64)> {
        let Some(uuid) = self.track_by_path(track_path) else {
            return Vec::new();
        };
        self.events
            .iter()
            .filter(|e| e.track_uuid == uuid && e.ty == TE_COUNTER)
            .filter_map(|e| e.counter_value.map(|v| (e.ts, v)))
            .collect()
    }

    /// Reconstructs all slices on `tacho` tracks by matching `SLICE_BEGIN` and `SLICE_END`.
    /// Panics if any `SLICE_END` has no matching `SLICE_BEGIN`.
    pub fn all_tacho_slices(&self) -> Vec<DecodedSlice> {
        use std::collections::HashMap;
        let mut stacks: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut out: Vec<DecodedSlice> = Vec::new();

        for ev in &self.events {
            let path = self.path(ev.track_uuid);
            if !(path == "tacho" || path.starts_with("tacho/")) {
                continue;
            }
            match ev.ty {
                TE_SLICE_BEGIN => {
                    stacks.entry(ev.track_uuid).or_default().push(out.len());
                    out.push(DecodedSlice {
                        track_path: path,
                        name: ev.name.clone(),
                        begin_ts: ev.ts,
                        end_ts: None,
                        flow_ids: ev.flow_ids.clone(),
                        terminating_flow_ids: ev.terminating_flow_ids.clone(),
                    });
                }
                TE_SLICE_END => {
                    let idx = stacks
                        .get_mut(&ev.track_uuid)
                        .and_then(|s| s.pop())
                        .unwrap_or_else(|| {
                            panic!("unmatched SLICE_END on track {} at ts={}", path, ev.ts)
                        });
                    out[idx].end_ts = Some(ev.ts);
                }
                _ => {}
            }
        }
        out
    }

    /// Returns all slices on a specific `track_path`.
    pub fn slices_on(&self, track_path: &str) -> Vec<DecodedSlice> {
        self.all_tacho_slices()
            .into_iter()
            .filter(|s| s.track_path == track_path)
            .collect()
    }

    /// Returns any slices on `tacho` tracks that were left unclosed at the end of the trace.
    pub fn unclosed_tacho_slices(&self) -> Vec<DecodedSlice> {
        self.all_tacho_slices()
            .into_iter()
            .filter(|s| s.end_ts.is_none())
            .collect()
    }

    /// Returns all `(origin_track::origin_name, dest_track::dest_name, flow_id)` links
    /// for `FLOW_MESSAGE_DELIVERY` (`(flow_id & 0xFF) == 200`).
    pub fn message_delivery_links(&self) -> Vec<(String, String, u64)> {
        use std::collections::HashMap;
        let label = |e: &DecodedTrackEvent| format!("{}::{}", self.path(e.track_uuid), e.name);
        let mut origins: HashMap<u64, String> = HashMap::new();
        for e in &self.events {
            for &f in &e.flow_ids {
                if (f & 0xFF) == FLOW_ORD_MESSAGE_DELIVERY {
                    origins.insert(f, label(e));
                }
            }
        }
        let mut links = Vec::new();
        for e in &self.events {
            for &f in &e.terminating_flow_ids {
                if let Some(from) = origins.get(&f) {
                    links.push((from.clone(), label(e), f));
                }
            }
        }
        links
    }

    /// Asserts that no `terminating_flow_ids` on `tacho` tracks reference an unknown `flow_id`.
    pub fn assert_no_dangling_terminating_flows(&self) {
        use std::collections::HashSet;
        let origins: HashSet<u64> = self
            .events
            .iter()
            .flat_map(|e| e.flow_ids.iter().copied())
            .collect();
        for e in &self.events {
            let path = self.path(e.track_uuid);
            if !(path == "tacho" || path.starts_with("tacho/")) {
                continue;
            }
            for &f in &e.terminating_flow_ids {
                assert!(
                    origins.contains(&f),
                    "dangling terminating_flow_id {} on {}::{}",
                    f,
                    path,
                    e.name
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfd_wait_exit_observes_child_exit() {
        let mut child = Command::new("cat").stdin(Stdio::piped()).spawn().unwrap();
        let fd = PidFd::open(child.id()).unwrap();
        // Closing stdin is the event that makes `cat` exit.
        drop(child.stdin.take());
        assert!(fd.wait_exit(None));
        assert!(child.wait().unwrap().success());
        assert!(PidFd::open(0).is_none());
        assert!(PidFd::open(u32::MAX).is_none());
    }

    #[test]
    fn wait_for_path_wakes_on_creation_from_another_thread() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("late");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let t = target.clone();
        let writer = std::thread::spawn(move || {
            rx.recv().unwrap();
            fs::write(&t, b"x").unwrap();
        });
        let waiter = {
            let target = target.clone();
            std::thread::spawn(move || TestEnv::wait_for_path(&target))
        };
        tx.send(()).unwrap();
        assert!(waiter.join().unwrap());
        writer.join().unwrap();
        // Already-existing path returns immediately.
        assert!(TestEnv::wait_for_path(&target));
    }

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
