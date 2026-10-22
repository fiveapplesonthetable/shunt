//! System tracing management around Perfetto.
//!
//! Provides APIs to locate the `perfetto` binary, start background ring-buffer
//! tracing sessions (`tacho-system`), query status, stream snapshots via live
//! session cloning (`clone_into`), and manage tracing daemons (`traced`).

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
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

    /// Returns the effective runtime directory.
    fn effective_runtime_dir(&self) -> PathBuf {
        self.runtime_dir.clone().unwrap_or_else(|| {
            env::var("TACHO_RUNTIME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| env::temp_dir())
        })
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
        fs::create_dir_all(&runtime_dir)?;

        let cfg_text = profile.config_text()?;
        let cfg_file = runtime_dir.join("tacho-system.cfg");
        fs::write(&cfg_file, &cfg_text)?;

        let sink_file = runtime_dir.join("tacho-system-sink.pftrace");
        let pid_file = runtime_dir.join("tacho-system.pid");

        let mut cmd = Command::new(&self.binary);
        cmd.arg("--background")
            .arg("--config")
            .arg(&cfg_file)
            .arg("-o")
            .arg(&sink_file)
            .arg("--txt");
        self.apply_env(&mut cmd);

        let output = cmd.output()?;
        if !output.status.success() {
            return Err(SystemError::CommandFailed {
                cmd: format!("{} --background --config ...", self.binary.display()),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = stdout.lines().find(|l| !l.trim().is_empty()) {
            let trimmed = line.trim();
            if let Ok(pid) = trimmed.parse::<u32>() {
                let _ = fs::write(&pid_file, pid.to_string());
            }
        }

        // Wait up to 2 seconds for session to appear in traced service state
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            if self.status()?.is_some() {
                break;
            }
        }

        Ok(())
    }

    /// Stops the active `tacho-system` tracing session gracefully.
    ///
    /// Sends `SIGTERM` to the background process and waits for the session
    /// to disappear from traced.
    pub fn stop(&self) -> Result<(), SystemError> {
        if self.status()?.is_none() {
            return Ok(());
        }

        let runtime_dir = self.effective_runtime_dir();
        let pid_file = runtime_dir.join("tacho-system.pid");

        if let Ok(content) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }
        }

        // Wait up to 3 seconds for graceful termination
        let mut stopped = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(100));
            if self.status()?.is_none() {
                stopped = true;
                break;
            }
        }

        if !stopped {
            // Fallback: kill background perfetto processes targeting tacho-system
            let _ = Command::new("pkill")
                .args(["-TERM", "-f", "perfetto.*tacho-system"])
                .output();

            for _ in 0..20 {
                thread::sleep(Duration::from_millis(100));
                if self.status()?.is_none() {
                    stopped = true;
                    break;
                }
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

        let counter = CLONE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = env::temp_dir().join(format!(
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
            let name = parts[8];
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
                    name: name.to_string(),
                }));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        env::set_var("TACHO_PERFETTO", "/tmp/nonexistent_perfetto_binary_xyz_123");
        let res = Perfetto::locate();
        assert!(res.is_err());
        let err = res.err().unwrap().to_string();
        assert!(err.contains("TACHO_PERFETTO is set"));
        env::remove_var("TACHO_PERFETTO");
    }

    #[test]
    fn test_locate_via_env_var() {
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
    }

    #[test]
    fn test_setup_instructions() {
        let non_sudo = setup_instructions(false);
        assert!(non_sudo.contains("tracebox traced --background"));
        assert!(!non_sudo.contains("sudo tracebox"));

        let sudo = setup_instructions(true);
        assert!(sudo.contains("sudo tracebox traced --background"));
    }
}
