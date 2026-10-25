//! Worker kit commands: `start`, `prove`, `job`, `wait`, `kill`, `pr`, `note`, `starve`.
//!
//! Organized into focused submodules under `kit/`:
//! - [`start`]: worktree provisioning, pinned 40-hex base SHA resolution, collision-free 20-port
//!   block allocation, `.tachoenv` generation, idempotent re-run, and bounded install cap (exit 3).
//! - [`prove`]: diff-to-area/unit check mapping, screenshot scope filtering, sequential execution
//!   stopping at first failure, and `.tachoproof/summary.json` with worktree git tree hash.
//! - [`pr`]: preflight refusal (empty diff, missing/failed/stale proof, dirty tree, merge commits,
//!   trailers, `"Generated with"` lines, session links, stray screenshots unless `shots_anyway`),
//!   `--no-verify` (`--force-with-lease`) push, and PR body formatting with proof commands & output.
//! - [`procs`]: detached background process groups (`job`) with `(pid, starttime)` metadata,
//!   kernel event waiting (`wait` by job name or numeric PID), and verified `kill` (`starttime`,
//!   `/proc/<pid>/cwd` worktree ownership, and `pgid == pid`).
//! - [`starve`]: `<= 5 min` (300s) CPU starvation runner (`4` burners per CPU on `<= 1/8` fleet CPUs,
//!   default highest 4 CPUs, refuses during merge-check batch, stops burners when batch starts).

pub mod pr;
pub mod procs;
pub mod prove;
pub mod start;
pub mod starve;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use tacho_core::decide::verify::parse_env;
use tacho_daemon::{FleetState, FleetStore};

use crate::client::DaemonClient;

pub use pr::execute_pr;
pub use procs::{execute_job, execute_kill, execute_note, execute_wait, MAX_WAIT_SECS};
pub use prove::{changed_files_since, execute_prove};
pub use start::execute_start;
pub use starve::execute_starve;

/// Event-driven watcher using Linux `inotify` + `pidfd_open` + `poll` so wait loops
/// wake immediately on file changes or process exit via kernel events.
pub(crate) struct FdEventWatcher {
    inotify_fd: OwnedFd,
    pid_fds: Vec<OwnedFd>,
}

impl FdEventWatcher {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            inotify_fd: unsafe { OwnedFd::from_raw_fd(fd) },
            pid_fds: Vec::new(),
        })
    }

    pub fn watch_path(&self, path: &Path) {
        if let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) {
            let mask = libc::IN_MODIFY
                | libc::IN_CREATE
                | libc::IN_CLOSE_WRITE
                | libc::IN_MOVED_TO
                | libc::IN_DELETE
                | libc::IN_ATTRIB;
            unsafe {
                let _ = libc::inotify_add_watch(
                    std::os::unix::io::AsRawFd::as_raw_fd(&self.inotify_fd),
                    c_path.as_ptr(),
                    mask,
                );
            }
        }
    }

    pub fn register_pid(&mut self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let pfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if pfd >= 0 {
            self.pid_fds
                .push(unsafe { OwnedFd::from_raw_fd(pfd as i32) });
            true
        } else {
            false
        }
    }

    /// Blocks until an inotify event, pidfd exit, or `timeout_ms` elapses.
    pub fn wait_ms(&self, timeout_ms: i32) {
        let ino_raw = std::os::unix::io::AsRawFd::as_raw_fd(&self.inotify_fd);
        let mut fds = Vec::with_capacity(1 + self.pid_fds.len());
        fds.push(libc::pollfd {
            fd: ino_raw,
            events: libc::POLLIN,
            revents: 0,
        });
        for pfd in &self.pid_fds {
            fds.push(libc::pollfd {
                fd: std::os::unix::io::AsRawFd::as_raw_fd(pfd),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if rc > 0 && (fds[0].revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 1024];
            loop {
                let n = unsafe { libc::read(ino_raw, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    break;
                }
            }
        }
    }
}

pub(crate) fn open_fleet_store() -> io::Result<FleetStore> {
    let home = crate::paths::resolve_home();
    crate::paths::ensure_dir_0700(&home)?;
    let db = home.join("events.sqlite");
    FleetStore::open(&db).map_err(|e| io::Error::other(e.to_string()))
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Records a decision event on the daemon timeline.
///
/// The event is best effort (the decision itself is already persisted by the caller), but a
/// delivery failure is reported on stderr rather than silently dropped.
pub(crate) fn emit_decision_event(track: &str, name: &str, because: &str) {
    let client = DaemonClient::new();
    let sent = client.send_request(&serde_json::json!({
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
    if let Err(e) = sent {
        eprintln!("[tacho] could not record decision event {track}/{name}: {e}");
    }
}

/// Creates (mode 0700) and returns the private temp dir `<fleet home>/tmp/<name>`.
pub(crate) fn private_tmp_dir(name: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::paths::resolve_home().join("tmp").join(name);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Points every temp-dir convention of a spawned agent or runner at `dir`: `TMPDIR`, `TMP`,
/// `TEMP`, and the JVM's `java.io.tmpdir` (through `JAVA_TOOL_OPTIONS`).
pub(crate) fn apply_private_tmp_env(cmd: &mut Command, dir: &Path) {
    cmd.env("TMPDIR", dir).env("TMP", dir).env("TEMP", dir).env(
        "JAVA_TOOL_OPTIONS",
        format!("-Djava.io.tmpdir={}", dir.display()),
    );
}

/// [`private_tmp_dir`] followed by [`apply_private_tmp_env`].
pub(crate) fn use_private_tmp(cmd: &mut Command, name: &str) -> io::Result<()> {
    let dir = private_tmp_dir(name)?;
    apply_private_tmp_env(cmd, &dir);
    Ok(())
}

pub(crate) fn is_internal_tacho_path(path: &str) -> bool {
    let p = path.trim();
    p == ".tachoenv"
        || p == ".tachoenv.tmp"
        || p.starts_with(".tachoproof/")
        || p == ".tachoproof"
        || p.starts_with(".tachorun/")
        || p == ".tachorun"
        || p.starts_with(".tacho-proof/")
        || p == ".tacho-proof"
        || p.starts_with(".tacho-run/")
        || p == ".tacho-run"
}

pub(crate) fn find_worktree_root(start: &Path) -> PathBuf {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".tachoenv").is_file() || dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        cur = dir.parent();
    }
    start.to_path_buf()
}

pub(crate) fn load_worktree_env(root: &Path) -> BTreeMap<String, String> {
    let env_path = root.join(".tachoenv");
    if let Ok(text) = fs::read_to_string(&env_path) {
        parse_env(&text)
    } else {
        BTreeMap::new()
    }
}

pub(crate) fn resolve_base_sha_for_worktree(
    root: &Path,
    explicit_base: Option<&str>,
    env_map: &BTreeMap<String, String>,
    state: &FleetState,
) -> io::Result<String> {
    if let Some(b) = explicit_base {
        let (sha, _) = start::resolve_pinned_base_sha(root, b, state)?;
        return Ok(sha);
    }
    if let Some(sha) = env_map
        .get("TACHO_BASE_SHA")
        .or_else(|| env_map.get("BASE_SHA"))
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(sha.trim().to_string());
    }
    if let Some(slug) = root.file_name().and_then(|s| s.to_str()) {
        if let Some(wt) = state.worktrees.get(slug) {
            if !wt.base_sha.trim().is_empty() {
                return Ok(wt.base_sha.trim().to_string());
            }
        }
    }
    if let Ok(out) = Command::new("git")
        .args(["merge-base", "HEAD", "main"])
        .current_dir(root)
        .output()
    {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                return Ok(sha);
            }
        }
    }
    let out = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(root)
        .output()?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()?;
    Ok(String::from_utf8_lossy(&head.stdout).trim().to_string())
}

/// Computes the exact git tree hash of the entire working tree (tracked + untracked files,
/// excluding `.tachoproof/`, `.tachorun/`, and `.tachoenv`) using a temporary git index file
/// without mutating the user's real git index.
pub fn tree_of_worktree(root: &Path) -> io::Result<String> {
    let tmp_dir = tempfile::tempdir()?;
    let tmp_index = tmp_dir.path().join("index");

    let head_tree_out = Command::new("git")
        .args(["rev-parse", "--verify", "-q", "HEAD^{tree}"])
        .current_dir(root)
        .output()?;
    if head_tree_out.status.success() {
        let head_tree = String::from_utf8_lossy(&head_tree_out.stdout)
            .trim()
            .to_string();
        if !head_tree.is_empty() {
            let rt_out = Command::new("git")
                .args(["read-tree", &head_tree])
                .current_dir(root)
                .env("GIT_INDEX_FILE", &tmp_index)
                .output()?;
            if !rt_out.status.success() {
                return Err(io::Error::other(format!(
                    "git read-tree {head_tree} in temp index failed: {}",
                    String::from_utf8_lossy(&rt_out.stderr).trim()
                )));
            }
        }
    }

    let add_out = Command::new("git")
        .args(["add", "-A"])
        .current_dir(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .output()?;
    if !add_out.status.success() {
        return Err(io::Error::other(format!(
            "git add -A in temp index failed: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        )));
    }

    // Remove internal kit files from temporary index so writing `.tachoproof` never invalidates the proof tree
    let rm_out = Command::new("git")
        .args([
            "rm",
            "-r",
            "--cached",
            "--ignore-unmatch",
            "-q",
            "--",
            ".tachoproof",
            ".tachorun",
            ".tacho-proof",
            ".tacho-run",
            ".tachoenv",
            ".tachoenv.tmp",
        ])
        .current_dir(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .output()?;
    if !rm_out.status.success() {
        return Err(io::Error::other(format!(
            "git rm --cached in temp index failed: {}",
            String::from_utf8_lossy(&rm_out.stderr).trim()
        )));
    }

    let wt_out = Command::new("git")
        .args(["write-tree"])
        .current_dir(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .output()?;
    if !wt_out.status.success() {
        return Err(io::Error::other(format!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&wt_out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&wt_out.stdout).trim().to_string())
}
