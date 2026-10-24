//! System tracing management around Perfetto.
//!
//! Provides APIs to locate the `perfetto` binary, start background ring-buffer
//! tracing sessions (`tacho-system`), query status, stream snapshots via live
//! session cloning (`clone_into`), and manage tracing daemons (`traced`).

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Shipped default tracing profile config (process stats and sys stats, no root required).
pub const DEFAULT_CONFIG: &str = include_str!("../configs/system-default.cfg");

/// Shipped full tracing profile config (default plus ftrace events, requires root or tracefs permissions).
pub const FULL_CONFIG: &str = include_str!("../configs/system-full.cfg");

/// Counter for generating unique temporary clone file names.
static CLONE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Tracing profile selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Profile {
    /// Default unprivileged profile: process tree, RSS, CPU time, meminfo, vmstat, stat.
    Default,
    /// Full profile: default plus ftrace scheduler and power events (requires root).
    Full,
    /// Custom profile loaded from a configuration file.
    Path(PathBuf),
}

impl Profile {
    /// Returns the configuration text content for this profile.
    pub fn config_text(&self) -> io::Result<String> {
        match self {
            Profile::Default => Ok(DEFAULT_CONFIG.to_string()),
            Profile::Full => Ok(FULL_CONFIG.to_string()),
            Profile::Path(p) => fs::read_to_string(p),
        }
    }
}

/// Information about an active Perfetto tracing session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Unique numeric tracing session ID assigned by traced.
    pub id: u32,
    /// User ID of the consumer process that started the session.
    pub uid: u32,
    /// Session state (e.g. "STARTED", "CONFIGURED").
    pub state: String,
    /// Total buffer size in KiB across all configured buffers.
    pub buffer_size_kb: Option<u64>,
    /// Number of active data sources attached to this session.
    pub num_data_sources: Option<u32>,
    /// Unique session name (e.g. "tacho-system").
    pub name: String,
}

/// Errors returned by system tracing operations.
#[derive(Debug)]
pub enum SystemError {
    /// The `perfetto` binary could not be found.
    BinaryNotFound(String),
    /// The `traced` daemon is not running or unreachable.
    TracedNotRunning(String),
    /// An operation requiring an active session was invoked, but no session is active.
    NoActiveSession(String),
    /// Underlying I/O error.
    Io(io::Error),
    /// An external command execution failed.
    CommandFailed {
        cmd: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    /// Output parsing error.
    ParseError(String),
}

impl fmt::Display for SystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SystemError::BinaryNotFound(hint) => write!(f, "perfetto binary not found: {hint}"),
            SystemError::TracedNotRunning(msg) => write!(f, "traced not running: {msg}"),
            SystemError::NoActiveSession(msg) => write!(f, "no active session: {msg}"),
            SystemError::Io(err) => write!(f, "I/O error: {err}"),
            SystemError::CommandFailed {
                cmd,
                status,
                stderr,
            } => {
                write!(f, "command '{cmd}' failed with {status}: {stderr}")
            }
            SystemError::ParseError(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for SystemError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SystemError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SystemError {
    fn from(err: io::Error) -> Self {
        SystemError::Io(err)
    }
}

/// Perfetto command-line wrapper for managing background `tacho-system` sessions.
#[derive(Debug, Clone)]
pub struct Perfetto {
    binary: PathBuf,
    consumer_sock: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
}

impl Perfetto {
    /// Locates the `perfetto` binary:
    /// 1. Checks `$TACHO_PERFETTO` environment variable.
    /// 2. Checks directories in `$PATH`.
    /// 3. Returns an error with a one-line fix hint if not found.
    ///
    /// Also honours `$PERFETTO_CONSUMER_SOCK_NAME` if set in the environment.
    pub fn locate() -> Result<Self, SystemError> {
        if let Some(path_var) = env::var_os("TACHO_PERFETTO") {
            let p = PathBuf::from(path_var);
            if p.is_file() {
                let consumer_sock = env::var_os("PERFETTO_CONSUMER_SOCK_NAME").map(PathBuf::from);
                return Ok(Self {
                    binary: p,
                    consumer_sock,
                    runtime_dir: None,
                });
            }
            return Err(SystemError::BinaryNotFound(format!(
                "TACHO_PERFETTO is set to '{}' but the file does not exist",
                p.display()
            )));
        }

        if let Some(paths) = env::var_os("PATH") {
            for dir in env::split_paths(&paths) {
                let candidate = dir.join("perfetto");
                if candidate.is_file() {
                    let consumer_sock =
                        env::var_os("PERFETTO_CONSUMER_SOCK_NAME").map(PathBuf::from);
                    return Ok(Self {
                        binary: candidate,
                        consumer_sock,
                        runtime_dir: None,
                    });
                }
            }
        }

        Err(SystemError::BinaryNotFound(
            "perfetto binary not found on PATH; install Perfetto (see https://perfetto.dev/docs/quickstart/linux-tracing) or set $TACHO_PERFETTO".to_string(),
        ))
    }

    /// Constructs a `Perfetto` wrapper directly from an explicit binary path.
    pub fn from_binary(path: impl Into<PathBuf>) -> Self {
        let consumer_sock = env::var_os("PERFETTO_CONSUMER_SOCK_NAME").map(PathBuf::from);
        Self {
            binary: path.into(),
            consumer_sock,
            runtime_dir: None,
        }
    }

    /// Configures an explicit consumer socket path for communicating with `traced`.
    pub fn with_consumer_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.consumer_sock = Some(path.into());
        self
    }

    /// Configures an explicit runtime directory for PID, config, and sink files.
    pub fn with_runtime_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.runtime_dir = Some(path.into());
        self
    }

    /// Returns the path to the resolved `perfetto` binary.
    pub fn binary_path(&self) -> &Path {
        &self.binary
    }

    /// Returns the consumer socket path if configured.
    pub fn consumer_socket(&self) -> Option<&Path> {
        self.consumer_sock.as_deref()
    }

    /// Returns the effective runtime directory ($TACHO_RUNTIME / $XDG_RUNTIME_DIR/tacho / $TACHO_HOME/run).
    fn effective_runtime_dir(&self) -> PathBuf {
        self.runtime_dir.clone().unwrap_or_else(resolve_runtime_dir)
    }

    /// Applies configured environment variables to child commands.
    fn apply_env(&self, cmd: &mut Command) {
        if let Some(ref sock) = self.consumer_sock {
            cmd.env("PERFETTO_CONSUMER_SOCK_NAME", sock);
        }
    }

    /// Returns whether the `traced` daemon is running and reachable.
    pub fn traced_running(&self) -> bool {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--query");
        self.apply_env(&mut cmd);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        match cmd.status() {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    }

    /// Queries the status of the `tacho-system` tracing session.
    pub fn status(&self) -> Result<Option<SessionInfo>, SystemError> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--query");
        self.apply_env(&mut cmd);
        let output = cmd.output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Could not connect to the traced socket") {
                return Ok(None);
            }
            return Err(SystemError::CommandFailed {
                cmd: format!("{} --query", self.binary.display()),
                status: output.status,
                stderr: stderr.to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_session_info(&stdout, "tacho-system")
    }

    /// Starts a background `RING_BUFFER` tracing session named `tacho-system`.
    ///
    /// Idempotent: if the session is already running, logs status and succeeds.
    ///
    /// Uses `perfetto --background-wait`, which exits only once every data
    /// source has started, so a successful return means `status()` will see the
    /// session; no polling is needed. Perfetto builds that predate the flag are
    /// retried with plain `--background`, whose parent exits right after the
    /// fork; there the session may take a moment to register.
    pub fn start(&self, profile: Profile) -> Result<(), SystemError> {
        if let Some(session) = self.status()? {
            eprintln!(
                "Tracing session 'tacho-system' is already running (id: {}, state: {})",
                session.id, session.state
            );
            return Ok(());
        }

        if !self.traced_running() {
            return Err(SystemError::TracedNotRunning(
                "traced daemon is not running; start traced first (see setup_instructions)"
                    .to_string(),
            ));
        }

        let runtime_dir = self.effective_runtime_dir();
        ensure_dir_0700(&runtime_dir)?;

        let cfg_text = profile.config_text()?;
        let cfg_file = runtime_dir.join("tacho-system.cfg");
        write_atomic(&cfg_file, cfg_text.as_bytes())?;

        let sink_file = runtime_dir.join("tacho-system-sink.pftrace");
        let pid_file = runtime_dir.join("tacho-system.pid");

        let run = |mode: &str| -> io::Result<std::process::Output> {
            let mut cmd = Command::new(&self.binary);
            cmd.arg(mode)
                .arg("--config")
                .arg(&cfg_file)
                .arg("-o")
                .arg(&sink_file)
                .arg("--txt")
                .stdin(Stdio::null());
            self.apply_env(&mut cmd);
            cmd.output()
        };

        let mut mode = "--background-wait";
        let mut output = run(mode)?;
        if !output.status.success() && is_unknown_option_error(&output.stderr) {
            mode = "--background";
            output = run(mode)?;
        }
        if !output.status.success() {
            return Err(SystemError::CommandFailed {
                cmd: format!("{} {mode} --config ...", self.binary.display()),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        match parse_background_pid(&stdout) {
            Some(pid) => {
                // Record the start time with the pid so `stop` can tell the
                // session process from an unrelated process that reused the pid.
                if let Some(t) = proc_start_time(pid) {
                    let record = format!("{pid} {t}\n");
                    write_atomic(&pid_file, record.as_bytes())?;
                } else {
                    let _ = fs::remove_file(&pid_file);
                }
            }
            None => {
                // A stale file from an earlier session would point `stop` at
                // whatever process now holds that pid.
                let _ = fs::remove_file(&pid_file);
                eprintln!(
                    "warning: perfetto did not print a background pid; stop will rely on traced"
                );
            }
        }
        Ok(())
    }

    /// Stops the active `tacho-system` tracing session gracefully.
    ///
    /// Reads the PID file, verifies the process is still the one that was
    /// started (same uid, same start time, or a `perfetto` command line for
    /// legacy pid files), signals it through a pidfd so the pid cannot be
    /// recycled in between, and waits for it to exit. Escalates to `SIGKILL`
    /// after a grace period.
    pub fn stop(&self) -> Result<(), SystemError> {
        if self.status()?.is_none() {
            return Ok(());
        }

        let runtime_dir = self.effective_runtime_dir();
        let pid_file = runtime_dir.join("tacho-system.pid");

        let target = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|c| PidRecord::parse(&c))
            .and_then(|rec| SessionProcess::open_verified(&rec, &self.binary));

        let mut stopped = false;
        if let Some(proc_) = &target {
            proc_.signal(libc::SIGTERM);
            stopped = proc_.wait_exit(Duration::from_secs(3));
        }
        if !stopped && self.status()?.is_none() {
            stopped = true;
        }
        if !stopped {
            if let Some(proc_) = &target {
                proc_.signal(libc::SIGKILL);
                stopped = proc_.wait_exit(Duration::from_secs(1));
            }
        }

        let _ = fs::remove_file(&pid_file);

        if !stopped {
            return Err(SystemError::CommandFailed {
                cmd: "stop".to_string(),
                status: Default::default(),
                stderr: "failed to stop tacho-system session within 5 seconds".to_string(),
            });
        }

        Ok(())
    }

    /// Creates a read-only clone of the active `tacho-system` session and streams
    /// its trace bytes into the provided writer.
    ///
    /// Returns the total number of bytes written. Fails with a clear error if no
    /// session is currently running.
    pub fn clone_into(&self, w: &mut impl Write) -> Result<u64, SystemError> {
        if self.status()?.is_none() {
            return Err(SystemError::NoActiveSession(
                "no active 'tacho-system' tracing session found to clone".to_string(),
            ));
        }

        // Clone into the private (0700) runtime dir, not the shared temp dir:
        // perfetto opens `-o` by path, so a predictable name in /tmp could be
        // pre-created as a symlink by another user.
        let runtime_dir = self.effective_runtime_dir();
        ensure_dir_0700(&runtime_dir)?;
        let counter = CLONE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = runtime_dir.join(format!(
            "tacho-clone-{}-{}-{counter}.pftrace",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        struct CleanupGuard<'a>(&'a Path);
        impl<'a> Drop for CleanupGuard<'a> {
            fn drop(&mut self) {
                let _ = fs::remove_file(self.0);
            }
        }
        let _guard = CleanupGuard(&tmp_path);

        let mut cmd = Command::new(&self.binary);
        cmd.stdin(Stdio::null());
        cmd.arg("--clone-by-name")
            .arg("tacho-system")
            .arg("-o")
            .arg(&tmp_path);
        self.apply_env(&mut cmd);

        let output = cmd.output()?;
        if !output.status.success() {
            return Err(SystemError::CommandFailed {
                cmd: format!(
                    "{} --clone-by-name tacho-system -o {}",
                    self.binary.display(),
                    tmp_path.display()
                ),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let mut file = fs::File::open(&tmp_path)?;
        let bytes_copied = io::copy(&mut file, w)?;
        Ok(bytes_copied)
    }

    /// Returns the setup instructions to run `traced` and `traced_probes`.
    pub fn setup_instructions(sudo: bool) -> String {
        setup_instructions(sudo)
    }
}

/// Returns whether `traced` is running, locating `perfetto` automatically if needed.
pub fn traced_running() -> bool {
    if let Ok(perfetto) = Perfetto::locate() {
        perfetto.traced_running()
    } else {
        false
    }
}

/// Returns human-readable setup instructions for starting Perfetto daemons.
///
/// Never executes sudo directly.
pub fn setup_instructions(sudo: bool) -> String {
    if sudo {
        r#"# To start Perfetto system tracing daemons as root (required for full profile with ftrace):
sudo tracebox traced --background
sudo tracebox traced_probes --background

# Verify daemons are running:
perfetto --query
"#
        .to_string()
    } else {
        r#"# To start Perfetto system tracing daemons unprivileged (enables default profile: process and sys stats):
tracebox traced --background
tracebox traced_probes --background

# Verify daemons are running:
perfetto --query
"#
        .to_string()
    }
}

/// Resolves `$TACHO_RUNTIME`:
/// 1. `$TACHO_RUNTIME`
/// 2. `$TACHO_HOME/run` (when `$TACHO_HOME` is explicitly set; falls back to `/tmp/tacho-<hash>` if path > 100 bytes)
/// 3. `$XDG_RUNTIME_DIR/tacho`
/// 4. `$HOME/.local/state/tacho/run`
fn resolve_runtime_dir() -> PathBuf {
    if let Ok(val) = env::var("TACHO_RUNTIME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val);
        }
    }
    if let Ok(val) = env::var("TACHO_HOME") {
        if !val.trim().is_empty() {
            let candidate = PathBuf::from(val.trim()).join("run");
            if candidate.join("trace.sock").as_os_str().len() <= 100 {
                return candidate;
            }
            let mut h: u64 = 0xcbf29ce484222325;
            for &b in val.trim().as_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            return PathBuf::from(format!("/tmp/tacho-{:016x}", h));
        }
    }
    if let Ok(val) = env::var("XDG_RUNTIME_DIR") {
        if !val.trim().is_empty() {
            return PathBuf::from(val).join("tacho");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/state/tacho/run")
}

/// Creates a directory recursively with mode 0700 if it does not exist,
/// or sets mode 0700 on existing directories.
pub fn ensure_dir_0700(path: &Path) -> io::Result<()> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.exists() {
        DirBuilder::new().mode(0o700).recursive(true).create(path)?;
    }
    // The runtime dir may fall back to a predictable `/tmp/tacho-<hash>` path;
    // refuse one that another user pre-created (or a symlink planted there)
    // instead of writing our config, pid and trace files into it.
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a directory owned by uid {euid}; refusing to use it",
                path.display()
            ),
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `data` to `path` via a temp file and rename, so readers never see a
/// partially written file and a crash leaves either the old or the new content.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// True when perfetto rejected a command-line flag (old builds without
/// `--background-wait`).
fn is_unknown_option_error(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    s.contains("unrecognized option")
        || s.contains("unknown option")
        || s.contains("invalid option")
}

/// Extracts the pid perfetto prints for a background session: the first
/// non-empty stdout line, which must be a positive integer.
fn parse_background_pid(stdout: &str) -> Option<u32> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.parse::<u32>()
        .ok()
        .filter(|&p| p > 0 && p <= i32::MAX as u32)
}

/// Process start time (field 22 of `/proc/<pid>/stat`, in clock ticks since
/// boot). Together with the pid it identifies a process uniquely.
fn proc_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_start_time(&stat)
}

/// `comm` (field 2) may contain spaces and parentheses, so fields are counted
/// from after the last `)`.
fn parse_stat_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After `)`: field 3 (state) is index 0, so field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Contents of the pid file: `"<pid> [<start_time>]"`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PidRecord {
    pid: u32,
    start_time: Option<u64>,
}

impl PidRecord {
    fn parse(content: &str) -> Option<Self> {
        let mut it = content.split_whitespace();
        let pid = it
            .next()?
            .parse::<u32>()
            .ok()
            .filter(|&p| p > 0 && p <= i32::MAX as u32)?;
        let start_time = match it.next() {
            Some(t) => Some(t.parse().ok()?),
            None => None,
        };
        if it.next().is_some() {
            return None;
        }
        Some(Self { pid, start_time })
    }
}

/// A pidfd for the recorded session process, opened only after the process
/// has been verified. Signals go through the pidfd, so they can never reach a
/// different process that later reuses the pid.
struct SessionProcess {
    pidfd: std::os::fd::OwnedFd,
}

impl SessionProcess {
    fn open_verified(rec: &PidRecord, _binary: &Path) -> Option<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain syscall; a non-negative return is a new fd we own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, rec.pid as libc::c_int, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` was just returned by pidfd_open and is owned by nobody else.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
        // Verify *after* opening the pidfd: if the pid is recycled after this
        // check, the pidfd still refers to the verified process.
        if !is_pid_owned_by_current_user(rec.pid) {
            return None;
        }
        let identity_ok = match rec.start_time {
            Some(t) if t > 0 => proc_start_time(rec.pid) == Some(t),
            _ => false,
        };
        identity_ok.then_some(Self { pidfd })
    }

    fn signal(&self, sig: libc::c_int) {
        use std::os::fd::AsRawFd;
        // SAFETY: valid pidfd; null siginfo is allowed.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }

    /// Waits for the process to exit (pidfd becomes readable), up to `grace`.
    fn wait_exit(&self, grace: Duration) -> bool {
        use std::os::fd::AsRawFd;
        let deadline = std::time::Instant::now() + grace;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as libc::c_int) };
            if rc > 0 {
                return true;
            }
            if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return false;
            }
        }
    }
}

/// Checks if a given PID is currently running and owned by the current user's UID.
fn is_pid_owned_by_current_user(pid: u32) -> bool {
    let status_path = format!("/proc/{pid}/status");
    let Ok(status) = fs::read_to_string(status_path) else {
        return false;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse::<u32>().ok())
        // geteuid cannot fail; the old /proc/self parse fell back to uid 0
        // (root) on error, which would have matched root-owned processes.
        .is_some_and(|uid| uid == unsafe { libc::geteuid() })
}

/// Parses the output of `perfetto --query` to locate a specific tracing session.
fn parse_session_info(stdout: &str, target_name: &str) -> Result<Option<SessionInfo>, SystemError> {
    let mut in_sessions = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("TRACING SESSIONS:") {
            in_sessions = true;
            continue;
        }
        if !in_sessions {
            continue;
        }
        // Skip table headers and empty lines
        if line.starts_with("ID") || line.starts_with("===") || line.is_empty() {
            continue;
        }
        // Columns: ID UID STATE BUF (#) KB DUR (s) #DS STARTED NAME
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 9 {
            let name = parts[8..].join(" ");
            if name == target_name {
                let id = parts[0].parse::<u32>().map_err(|e| {
                    SystemError::ParseError(format!(
                        "failed to parse session id '{}': {e}",
                        parts[0]
                    ))
                })?;
                let uid = parts[1].parse::<u32>().map_err(|e| {
                    SystemError::ParseError(format!("failed to parse uid '{}': {e}", parts[1]))
                })?;
                let state = parts[2].to_string();
                let buffer_size_kb = parts[4].parse::<u64>().ok();
                let num_data_sources = parts[6].parse::<u32>().ok();
                return Ok(Some(SessionInfo {
                    id,
                    uid,
                    state,
                    buffer_size_kb,
                    num_data_sources,
                    name,
                }));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that mutate process-wide env vars must not interleave.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_config_embedding() {
        assert!(DEFAULT_CONFIG.contains("unique_session_name: \"tacho-system\""));
        assert!(DEFAULT_CONFIG.contains("linux.process_stats"));
        assert!(DEFAULT_CONFIG.contains("linux.sys_stats"));
        assert!(!DEFAULT_CONFIG.contains("linux.ftrace"));

        assert!(FULL_CONFIG.contains("unique_session_name: \"tacho-system\""));
        assert!(FULL_CONFIG.contains("linux.ftrace"));
        assert!(FULL_CONFIG.contains("linux.process_stats"));
        assert!(FULL_CONFIG.contains("linux.sys_stats"));

        assert_eq!(
            Profile::Default.config_text().unwrap(),
            DEFAULT_CONFIG.to_string()
        );
        assert_eq!(
            Profile::Full.config_text().unwrap(),
            FULL_CONFIG.to_string()
        );
    }

    #[test]
    fn test_custom_profile_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let custom_cfg = temp_dir.path().join("custom.cfg");
        fs::write(&custom_cfg, "unique_session_name: \"tacho-system\"\n").unwrap();

        let profile = Profile::Path(custom_cfg);
        assert_eq!(
            profile.config_text().unwrap(),
            "unique_session_name: \"tacho-system\"\n"
        );
    }

    #[test]
    fn test_locate_missing_binary() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::set_var("TACHO_PERFETTO", "/tmp/nonexistent_perfetto_binary_xyz_123");
        let res = Perfetto::locate();
        assert!(res.is_err());
        let err = res.err().unwrap().to_string();
        assert!(err.contains("TACHO_PERFETTO is set"));
        env::remove_var("TACHO_PERFETTO");
    }

    #[test]
    fn test_locate_via_env_var() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp_dir = tempfile::tempdir().unwrap();
        let fake_perfetto = temp_dir.path().join("fake_perfetto");
        fs::write(&fake_perfetto, "#!/bin/sh\nexit 0\n").unwrap();

        env::set_var("TACHO_PERFETTO", &fake_perfetto);
        env::set_var("PERFETTO_CONSUMER_SOCK_NAME", "/tmp/fake-consumer.sock");

        let p = Perfetto::locate().expect("should locate via TACHO_PERFETTO");
        assert_eq!(p.binary_path(), fake_perfetto);
        assert_eq!(
            p.consumer_socket(),
            Some(Path::new("/tmp/fake-consumer.sock"))
        );

        env::remove_var("TACHO_PERFETTO");
        env::remove_var("PERFETTO_CONSUMER_SOCK_NAME");
    }

    #[test]
    fn test_parse_session_info() {
        let sample_output = r#"
Service: Perfetto v55.0

TRACING SESSIONS:

ID      UID     STATE      BUF (#) KB   DUR (s)   #DS  STARTED  NAME
===     ===     =====      ==========   =======   ===  =======  ====
1       430560  STARTED    (1) 32768    0         2    15:14:19 tacho-system
2       1000    CONFIGURED (1) 16384    10        1    15:15:00 other-session
"#;
        let session = parse_session_info(sample_output, "tacho-system")
            .unwrap()
            .expect("should find tacho-system");
        assert_eq!(session.id, 1);
        assert_eq!(session.uid, 430560);
        assert_eq!(session.state, "STARTED");
        assert_eq!(session.buffer_size_kb, Some(32768));
        assert_eq!(session.num_data_sources, Some(2));
        assert_eq!(session.name, "tacho-system");

        let missing = parse_session_info(sample_output, "nonexistent").unwrap();
        assert!(missing.is_none());

        let space_sample = r#"
TRACING SESSIONS:

ID      UID     STATE      BUF (#) KB   DUR (s)   #DS  STARTED  NAME
===     ===     =====      ==========   =======   ===  =======  ====
1       430560  STARTED    (1) 32768    0         2    15:14:19 session with spaces
"#;
        let spaced = parse_session_info(space_sample, "session with spaces")
            .unwrap()
            .expect("should handle session names with spaces");
        assert_eq!(spaced.name, "session with spaces");
    }

    #[test]
    fn test_setup_instructions() {
        let non_sudo = setup_instructions(false);
        assert!(non_sudo.contains("tracebox traced --background"));
        assert!(!non_sudo.contains("sudo tracebox"));

        let sudo = setup_instructions(true);
        assert!(sudo.contains("sudo tracebox traced --background"));
    }

    #[test]
    fn stat_start_time_survives_hostile_comm() {
        // comm containing spaces and ") (" must not shift the field count.
        let fields: Vec<String> = (3..=25).map(|i| i.to_string()).collect();
        let stat = format!("4242 (evil ) ( name) {}", fields.join(" "));
        assert_eq!(parse_stat_start_time(&stat), Some(22));
        assert_eq!(parse_stat_start_time("4242 (x) R 1"), None);
        assert_eq!(parse_stat_start_time("no paren at all"), None);
        let own = proc_start_time(std::process::id()).expect("own start time");
        assert_eq!(proc_start_time(std::process::id()), Some(own));
    }

    #[test]
    fn pid_record_and_background_pid_parsing() {
        assert_eq!(
            PidRecord::parse("123 456\n"),
            Some(PidRecord {
                pid: 123,
                start_time: Some(456)
            })
        );
        assert_eq!(
            PidRecord::parse("123"),
            Some(PidRecord {
                pid: 123,
                start_time: None
            })
        );
        for bad in [
            "",
            "0",
            "-1",
            "abc",
            "123 x",
            "123 4 5",
            "99999999999",
            "\u{1f600}",
        ] {
            assert_eq!(PidRecord::parse(bad), None, "{bad:?}");
        }
        assert_eq!(parse_background_pid("\n  777 \nnoise"), Some(777));
        assert_eq!(parse_background_pid("Started session\n777"), None);
        assert_eq!(parse_background_pid("0"), None);
        assert_eq!(parse_background_pid(""), None);
    }

    #[test]
    fn write_atomic_replaces_whole_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.pid");
        write_atomic(&path, &vec![b'x'; 1 << 20]).unwrap();
        write_atomic(&path, b"short").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"short");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f.pid")]);
    }

    #[test]
    fn ensure_dir_refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = ensure_dir_0700(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let fresh = dir.path().join("a/b/c");
        ensure_dir_0700(&fresh).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn session_process_refuses_recycled_pid_and_kills_verified_one() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = proc_start_time(pid).unwrap();
        let bin = Path::new("/nonexistent/perfetto-bin");

        // Same pid, different start time: a recycled pid. Must not be opened.
        let wrong = PidRecord {
            pid,
            start_time: Some(start + 1),
        };
        assert!(SessionProcess::open_verified(&wrong, bin).is_none());
        // Legacy record with a non-perfetto command line: refused too.
        let legacy = PidRecord {
            pid,
            start_time: None,
        };
        assert!(SessionProcess::open_verified(&legacy, bin).is_none());

        let right = PidRecord {
            pid,
            start_time: Some(start),
        };
        let sp = SessionProcess::open_verified(&right, bin).expect("verified");
        sp.signal(libc::SIGTERM);
        // `cat` is our child, so it turns zombie (pidfd readable) on exit.
        assert!(sp.wait_exit(Duration::from_secs(600)));
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    }
}
