//! Persistent fleet store helpers, extended metadata store, atomic file I/O,
//! Linux `/proc` process inspection, git plumbing helpers, and environment file loading.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tacho_daemon::{FleetState, FleetStore};

use crate::client::DaemonClient;

/// Opens the persistent [`FleetStore`] in `$TACHO_HOME/events.sqlite`.
pub fn open_fleet_store() -> io::Result<FleetStore> {
    let home = crate::paths::resolve_home();
    crate::paths::ensure_dir_0700(&home)?;
    let db = home.join("events.sqlite");
    FleetStore::open(&db).map_err(|e| io::Error::other(e.to_string()))
}

/// Writes `path` atomically: a crash leaves either the old file or the complete new one,
/// never a truncated mix (temp file in the same dir, fsync, rename).
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Current epoch time in seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current epoch time in nanoseconds.
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Best-effort emission of an instant decision event to the running daemon socket.
pub fn emit_decision_event(track: &str, name: &str, because: &str) {
    let client = DaemonClient::new();
    if client.try_connect_api_fast().is_ok() {
        let _ = client.send_request(&serde_json::json!({
            "input": {
                "type": "custom",
                "source_ts_ns": now_ns(),
                "track": track,
                "name": name,
                "op": "instant",
                "args": {
                    "because": because,
                }
            }
        }));
    }
}

/// Returns `true` if `pid` exists, `/proc/<pid>/stat` is readable, and the process is not a zombie (`Z`).
///
/// Fails closed (`false`) if `/proc/<pid>/stat` cannot be read or parsed.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc != 0 {
        return false;
    }
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(rparen) = stat.rfind(')') else {
        return false;
    };
    !stat[rparen + 1..].trim_start().starts_with('Z')
}

/// Returns `true` if `pid` is alive in `/proc/<pid>/stat`, not a zombie, and matches
/// `expected_starttime` (which must be `Some(st)` with `st > 0`).
///
/// Fails closed (`false`) if `expected_starttime` is missing/zero or `/proc/<pid>/stat` cannot be read.
pub fn pid_alive_matching(pid: u32, expected_starttime: Option<u64>) -> bool {
    let Some(expected_st) = expected_starttime.filter(|&st| st > 0) else {
        return false;
    };
    if !pid_alive(pid) {
        return false;
    }
    let Some(actual_st) = proc_pid_starttime(pid) else {
        return false;
    };
    actual_st == expected_st
}

/// Reads field 22 (`starttime` in clock ticks since boot) of `/proc/<pid>/stat`.
pub fn proc_pid_starttime(pid: u32) -> Option<u64> {
    let stat_path = format!("/proc/{pid}/stat");
    let content = fs::read_to_string(&stat_path).ok()?;
    let rparen = content.rfind(')')?;
    let rest = &content[rparen + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(19).and_then(|s| s.parse::<u64>().ok())
}

/// Reads `/proc/<pid>/cgroup` for strong process identity verification.
pub fn proc_pid_cgroup(pid: u32) -> Option<String> {
    let cgroup_path = format!("/proc/{pid}/cgroup");
    let content = fs::read_to_string(&cgroup_path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Verify that a PID on the system actually belongs to the target process before signalling.
///
/// Requires at least one strong identity (`expected_starttime` > 0 or non-empty `expected_cgroup`)
/// recorded at spawn time. Never falls back to `/proc/<pid>/environ` or `/proc/<pid>/cmdline`.
pub(crate) fn verify_agent_pid(
    pid: u32,
    agent_key: &str,
    expected_cgroup: Option<&str>,
    expected_starttime: Option<u64>,
) -> bool {
    let has_valid_st = expected_starttime.is_some_and(|st| st > 0);
    let has_valid_cg = expected_cgroup.is_some_and(|cg| !cg.is_empty());
    if !has_valid_st && !has_valid_cg {
        eprintln!(
            "[tacho-safety] PID {pid} for '{agent_key}' has neither recorded starttime nor cgroup; refusing signal without identity"
        );
        return false;
    }

    if !pid_alive(pid) {
        return false;
    }

    let Some(actual_st) = proc_pid_starttime(pid) else {
        return false;
    };

    if let Some(expected_st) = expected_starttime {
        if expected_st == 0 || actual_st != expected_st {
            eprintln!(
                "[tacho-safety] PID {pid} starttime ({actual_st}) does not match recorded ({expected_st}); refusing signal to reused PID"
            );
            return false;
        }
    }

    if let Some(cg) = expected_cgroup.filter(|s| !s.is_empty()) {
        let cgroup_path = format!("/proc/{pid}/cgroup");
        let Ok(actual_cg) = fs::read_to_string(&cgroup_path) else {
            return false;
        };
        if !actual_cg.contains(cg) {
            eprintln!(
                "[tacho-safety] PID {pid} cgroup does not contain expected '{cg}' for '{agent_key}'; refusing signal"
            );
            return false;
        }
    }

    true
}

/// What is known about the process recorded for an agent, for deciding whether a resume is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentProcess {
    /// No process exists for the recorded identity: no pid, the pid is gone or a zombie,
    /// or the pid was reused (recorded starttime differs from the live one).
    Gone,
    /// The recorded process is alive, its identity is verified, and it is job-control stopped.
    Stopped(u32),
    /// The recorded process is alive, its identity is verified, and it is running.
    Running,
    /// A process is alive at the recorded pid but no identity was recorded to prove it is ours.
    Unverified,
}

/// Classifies the process recorded for `agent_key` without signalling it.
pub(crate) fn classify_agent_process(
    pid: Option<u32>,
    agent_key: &str,
    expected_cgroup: Option<&str>,
    expected_starttime: Option<u64>,
) -> AgentProcess {
    let Some(pid) = pid else {
        return AgentProcess::Gone;
    };
    if !pid_alive(pid) {
        return AgentProcess::Gone;
    }
    if let Some(expected) = expected_starttime.filter(|&st| st > 0) {
        if proc_pid_starttime(pid).is_some_and(|actual| actual != expected) {
            // The pid now names a different process, so the recorded one has exited.
            return AgentProcess::Gone;
        }
    }
    if !verify_agent_pid(pid, agent_key, expected_cgroup, expected_starttime) {
        return AgentProcess::Unverified;
    }
    if proc_pid_state(pid).is_some_and(|s| s == 'T' || s == 't') {
        AgentProcess::Stopped(pid)
    } else {
        AgentProcess::Running
    }
}

/// Reads field 3 (process state letter) of `/proc/<pid>/stat`.
pub fn proc_pid_state(pid: u32) -> Option<char> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = content.rfind(')')?;
    content[rparen + 1..].trim_start().chars().next()
}

pub use crate::kit::procs::send_signal;

/// SIGKILLs the process group of a verified, stopped agent and blocks until its leader exits.
///
/// A stopped process cannot run, so SIGKILL is delivered immediately; the wait is on the
/// pidfd exit event, not a timer.
pub(crate) fn kill_stopped_and_wait(pid: u32) -> io::Result<()> {
    send_signal(-(pid as i32), libc::SIGKILL)?;
    wait_for_pid_exit_event(pid)
}

/// Blocks until `pid` exits, waiting on its pidfd. Returns immediately if it is already gone.
pub(crate) fn wait_for_pid_exit_event(pid: u32) -> io::Result<()> {
    // SAFETY: pidfd_open takes plain integers and returns a new fd or -1.
    let pidfd =
        unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as libc::c_int;
    if pidfd < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(io::Error::new(
            err.kind(),
            format!("pidfd_open({pid}) failed: {err}"),
        ));
    }
    let mut pfd = libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = loop {
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break rc;
        }
    };
    let poll_err = io::Error::last_os_error();
    // SAFETY: `pidfd` is an fd we own and close exactly once.
    unsafe { libc::close(pidfd) };
    if rc < 0 {
        return Err(io::Error::new(
            poll_err.kind(),
            format!("poll on pidfd for {pid} failed: {poll_err}"),
        ));
    }
    Ok(())
}

/// Wait up to `timeout` for `pid` to exit using Linux `pidfd_open` + `libc::poll`
/// (with `yield_now` fallback).
pub fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    if !pid_alive(pid) {
        return true;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let pidfd = libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) as libc::c_int;
        if pidfd >= 0 {
            let mut pfd = libc::pollfd {
                fd: pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
            let _ = libc::poll(&mut pfd, 1, ms);
            libc::close(pidfd);
            return !pid_alive(pid);
        }
    }
    let deadline = Instant::now() + timeout;
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    !pid_alive(pid)
}

/// Returns the set of agent keys whose processes are currently alive and match their recorded `(pid, starttime)`.
pub fn live_agent_keys(state: &FleetState) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for a in state.agents.values() {
        if a.finished_at.is_none() {
            if let Some(pid) = a.pid {
                if pid_alive_matching(pid, a.starttime) {
                    out.insert(a.key.clone());
                }
            }
        }
    }
    out
}

/// Stored retirement metadata for a retired task or lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetiredMeta {
    pub key: String,
    pub why: String,
    pub at: i64,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub held_for: String,
}

/// Four-state notice delivery state for `release-train` and owner notifications.
/// Once a notice enters `Uncertain`, it is never automatically retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NoticeState {
    #[default]
    Pending,
    Sending {
        head: String,
        at: i64,
    },
    Sent {
        head: String,
        at: i64,
    },
    Uncertain {
        head: String,
        at: i64,
        error: String,
    },
}

impl NoticeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            NoticeState::Pending => "Pending",
            NoticeState::Sending { .. } => "Sending",
            NoticeState::Sent { .. } => "Sent",
            NoticeState::Uncertain { .. } => "Uncertain",
        }
    }

    pub fn head(&self) -> Option<&str> {
        match self {
            NoticeState::Pending => None,
            NoticeState::Sending { head, .. }
            | NoticeState::Sent { head, .. }
            | NoticeState::Uncertain { head, .. } => Some(head.as_str()),
        }
    }
}

/// Tracked open watchdog GitHub issue state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogIssueState {
    pub number: u64,
    pub last_comment_at: i64,
}

/// Record of the last successful self-update move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMoveRecord {
    pub from_sha: String,
    pub to_sha: String,
    pub at: i64,
}

/// Recorded process identity (`pid`, `starttime`, `cgroup`) for a batch run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchPidIdentity {
    pub pid: u32,
    #[serde(default)]
    pub starttime: Option<u64>,
    #[serde(default)]
    pub cgroup: Option<String>,
}

/// Extended fleet metadata persisted atomically in `$TACHO_HOME/fleet_meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FleetMeta {
    #[serde(default)]
    pub retired: BTreeMap<String, RetiredMeta>,
    #[serde(default)]
    pub train_notices: BTreeMap<String, NoticeState>,
    #[serde(default)]
    pub watchdog_issue: Option<WatchdogIssueState>,
    #[serde(default)]
    pub last_code_moved: Option<CodeMoveRecord>,
    #[serde(default)]
    pub batch_identities: BTreeMap<String, BatchPidIdentity>,
}

pub fn record_batch_pid_identity(batch_id: &str, pid: u32) -> io::Result<BatchPidIdentity> {
    let identity = BatchPidIdentity {
        pid,
        starttime: proc_pid_starttime(pid),
        cgroup: proc_pid_cgroup(pid),
    };
    let id_clone = identity.clone();
    let batch_key = batch_id.to_string();
    mutate_fleet_meta(move |m| {
        m.batch_identities.insert(batch_key, id_clone);
    })?;
    Ok(identity)
}

pub fn fleet_meta_path() -> io::Result<PathBuf> {
    let home = crate::paths::resolve_home();
    crate::paths::ensure_dir_0700(&home)?;
    Ok(home.join("fleet_meta.json"))
}

pub fn load_fleet_meta() -> io::Result<FleetMeta> {
    let path = fleet_meta_path()?;
    if !path.is_file() {
        return Ok(FleetMeta::default());
    }
    let raw = fs::read_to_string(&path)?;
    Ok(serde_json::from_str::<FleetMeta>(&raw).unwrap_or_default())
}

pub fn save_fleet_meta(meta: &FleetMeta) -> io::Result<()> {
    let path = fleet_meta_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    write_atomic(&path, &bytes)
}

pub fn mutate_fleet_meta<F, R>(f: F) -> io::Result<R>
where
    F: FnOnce(&mut FleetMeta) -> R,
{
    let mut meta = load_fleet_meta().unwrap_or_default();
    let out = f(&mut meta);
    save_fleet_meta(&meta)?;
    Ok(out)
}

/// Parses a systemd-compatible `EnvironmentFile` (`KEY=VALUE`, optional quotes, `#` comments).
pub fn parse_systemd_env_file(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim();
        if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val = &val[1..val.len() - 1];
        }
        map.insert(key.to_string(), val.to_string());
    }
    map
}

/// Loads environment configuration from `env_file_override`, `$TACHO_ENV_FILE`, or
/// `$TACHO_HOME/fleet.env`, merging with process environment so process env always wins.
pub fn load_fleet_env(env_file_override: Option<&Path>) -> BTreeMap<String, String> {
    let default_path = std::env::var_os("TACHO_ENV_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            let home = crate::paths::resolve_home();
            let p1 = home.join("fleet.env");
            if p1.is_file() {
                Some(p1)
            } else {
                Some(home.join("tacho.env"))
            }
        });
    let file_path = env_file_override.map(Path::to_path_buf).or(default_path);
    let mut merged = file_path
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|txt| parse_systemd_env_file(&txt))
        .unwrap_or_default();
    for (k, v) in std::env::vars() {
        merged.insert(k, v);
    }
    merged
}

/// Runs `git -C <dir> <args>` and returns trimmed stdout, failing on a non-zero exit so callers
/// never act on a half-done git step.
pub fn git_stdout(dir: &Path, args: &[&str]) -> io::Result<String> {
    // Never let git block on a prompt, an editor, or a pager: stdin is closed and every
    // interactive hook is disabled, so a git call either completes or fails.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolves the live `main` SHA from `origin` (`refs/heads/main`), falling back to local `refs/heads/main` or `HEAD`.
pub fn resolve_live_main_sha(cwd: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .args(["ls-remote", "origin", "refs/heads/main"])
        .current_dir(cwd)
        .output()
    {
        if out.status.success() {
            if let Some(sha) = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().next())
                .filter(|s| !s.is_empty())
            {
                return sha.to_string();
            }
        }
    }
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "refs/heads/main"])
        .current_dir(cwd)
        .output()
    {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                return sha;
            }
        }
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Resolves live PR head SHAs from `origin` or local branches to detect ref movement during batch/train runs.
pub fn resolve_live_pr_heads(
    cwd: &Path,
    pr_nums: &[u64],
    expected_shas: &BTreeMap<u64, String>,
    latest_state: &FleetState,
) -> BTreeMap<u64, String> {
    let mut current_shas = expected_shas.clone();
    let mut remote_refs: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(out) = Command::new("git")
        .args(["ls-remote", "origin"])
        .current_dir(cwd)
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut parts = line.split_whitespace();
                if let (Some(sha), Some(rname)) = (parts.next(), parts.next()) {
                    remote_refs.insert(rname.to_string(), sha.to_string());
                }
            }
        }
    }
    for &n in pr_nums {
        if latest_state.closed_prs.contains(&n) {
            current_shas.remove(&n);
            continue;
        }
        if let Some(sha) = remote_refs.get(&format!("refs/pull/{n}/head")) {
            current_shas.insert(n, sha.clone());
            continue;
        }
        if let Some(p) = latest_state.open_prs_cache.iter().find(|p| p.number == n) {
            if !p.branch.is_empty() {
                if let Some(sha) = remote_refs.get(&format!("refs/heads/{}", p.branch)) {
                    current_shas.insert(n, sha.clone());
                    continue;
                }
                if let Ok(out) = Command::new("git")
                    .args(["rev-parse", &format!("refs/heads/{}", p.branch)])
                    .current_dir(cwd)
                    .output()
                {
                    if out.status.success() {
                        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !sha.is_empty() {
                            current_shas.insert(n, sha);
                            continue;
                        }
                    }
                }
            }
            current_shas.insert(n, p.head.clone());
        }
    }
    current_shas
}
