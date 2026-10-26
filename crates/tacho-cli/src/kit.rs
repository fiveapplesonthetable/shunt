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
/// wake immediately on file changes or process exit via kernel events, degrading cleanly
/// to `pidfd` + `poll` when the host is out of inotify instances.
pub(crate) struct FdEventWatcher {
    inotify_fd: Option<OwnedFd>,
    pid_fds: Vec<OwnedFd>,
}

impl FdEventWatcher {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        let inotify_fd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(Self {
            inotify_fd,
            pid_fds: Vec::new(),
        })
    }

    pub fn watch_path(&self, path: &Path) {
        let Some(ref inotify_fd) = self.inotify_fd else {
            return;
        };
        if let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) {
            let mask = libc::IN_MODIFY
                | libc::IN_CREATE
                | libc::IN_CLOSE_WRITE
                | libc::IN_MOVED_TO
                | libc::IN_DELETE
                | libc::IN_ATTRIB;
            unsafe {
                let _ = libc::inotify_add_watch(
                    std::os::unix::io::AsRawFd::as_raw_fd(inotify_fd),
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
        let mut fds = Vec::with_capacity(1 + self.pid_fds.len());
        if let Some(ref ino) = self.inotify_fd {
            fds.push(libc::pollfd {
                fd: std::os::unix::io::AsRawFd::as_raw_fd(ino),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for pfd in &self.pid_fds {
            fds.push(libc::pollfd {
                fd: std::os::unix::io::AsRawFd::as_raw_fd(pfd),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let effective_timeout = if self.inotify_fd.is_some() {
            timeout_ms
        } else {
            timeout_ms.clamp(1, 10)
        };
        let ptr = if fds.is_empty() {
            std::ptr::null_mut()
        } else {
            fds.as_mut_ptr()
        };
        let rc = unsafe { libc::poll(ptr, fds.len() as libc::nfds_t, effective_timeout) };
        if let Some(ref ino) = self.inotify_fd {
            if rc > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                let ino_raw = std::os::unix::io::AsRawFd::as_raw_fd(ino);
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

/// Records a decision event on the running daemon timeline if a daemon is active.
///
/// The decision and state band are already persisted directly in `events.sqlite` by the caller;
/// this notifies an already-running daemon without spawning an orphaned background daemon.
pub(crate) fn emit_decision_event(track: &str, name: &str, because: &str) {
    let client = DaemonClient::new();
    if client.try_connect_api_fast().is_ok() {
        if let Err(e) = client.send_request(&serde_json::json!({
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
        })) {
            eprintln!("[tacho] could not record decision event {track}/{name}: {e}");
        }
    }
}

/// Creates (mode 0700) and returns the private temp dir `<fleet home>/tmp/<sanitized_name>`.
pub(crate) fn private_tmp_dir(name: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let safe_name = tacho_core::decide::ops::config::sanitize_tmp_name(name);
    let tmp_root = crate::paths::resolve_home().join("tmp");
    fs::create_dir_all(&tmp_root)?;
    fs::set_permissions(&tmp_root, fs::Permissions::from_mode(0o700))?;
    let dir = tmp_root.join(safe_name);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Points every temp-dir convention of a spawned agent or runner at `dir`: `TMPDIR`, `TMP`,
/// `TEMP`, and the JVM's `java.io.tmpdir` (through `JAVA_TOOL_OPTIONS`, preserving any existing
/// flags).
pub(crate) fn apply_private_tmp_env(cmd: &mut Command, dir: &Path) {
    let existing_java = std::env::var("JAVA_TOOL_OPTIONS").ok();
    let dir_str = dir.to_string_lossy();
    let merged_java =
        tacho_core::decide::ops::config::merge_java_tmp_options(existing_java.as_deref(), &dir_str);
    cmd.env("TMPDIR", dir)
        .env("TMP", dir)
        .env("TEMP", dir)
        .env("JAVA_TOOL_OPTIONS", merged_java);
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

pub use tacho_daemon::exec::tidy::tree_of_worktree;

/// Creates a new SQL migration file in `migrations_dir` stamped with UTC `<YYYYMMDDHHMMSS>_<name>.sql`
/// using [`tacho_core::decide::ops::kit::format_utc_migration_filename`].
pub fn create_utc_migration_in_worktree(
    migrations_dir: &Path,
    name: &str,
    utc_epoch_secs: u64,
) -> io::Result<PathBuf> {
    let mut existing = Vec::new();
    if let Ok(entries) = fs::read_dir(migrations_dir) {
        for e in entries.flatten() {
            if let Some(n) = e.file_name().to_str() {
                existing.push(n.to_string());
            }
        }
    }
    let filename = tacho_core::decide::ops::kit::format_utc_migration_filename(
        utc_epoch_secs as i64,
        name,
        &existing,
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    fs::create_dir_all(migrations_dir)?;
    let path = migrations_dir.join(filename);
    if !path.exists() {
        write_atomic(&path, b"-- Add migration SQL here\n")?;
    }
    Ok(path)
}

/// Scans `migrations` directories in `root`, validates that filenames are in strict version order,
/// and computes the ordered [`tacho_core::decide::ops::kit::SqlxMigrationStep`] plan with SHA-384
/// digests (`_sqlx_migrations` format).
pub fn verify_and_plan_worktree_migrations(
    root: &Path,
) -> io::Result<Vec<tacho_core::decide::ops::kit::SqlxMigrationStep>> {
    let mut candidate_dirs = vec![root.join("migrations")];
    let crates_dir = root.join("crates");
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        let mut crate_mig_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("migrations"))
            .filter(|p| p.is_dir())
            .collect();
        crate_mig_dirs.sort();
        candidate_dirs.extend(crate_mig_dirs);
    }

    let mut all_planned = Vec::new();
    for dir in candidate_dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut files: Vec<(String, String)> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                let text = fs::read_to_string(&path)?;
                files.push((fname.to_string(), text));
            }
        }
        if files.is_empty() {
            continue;
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        let ordered = tacho_core::decide::ops::kit::migration_order(&names);
        if ordered != names {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "migration files in {} are out of version order",
                    dir.display()
                ),
            ));
        }
        let planned =
            tacho_core::decide::ops::kit::plan_template_migrations(&files, &BTreeMap::new())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        for entry in &planned {
            let expected_hex = tacho_core::decide::ops::kit::sha384_hex(entry.sql.as_bytes());
            if entry.checksum_hex != expected_hex {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("migration checksum mismatch for {}", entry.filename),
                ));
            }
        }
        all_planned.extend(planned);
    }
    Ok(all_planned)
}

/// Applies every migration in `migrations_dir` to a template database in version order and records
/// each entry in `_sqlx_migrations` (`version`, `description`, `success`, `checksum`, `execution_time`),
/// supporting both direct SQL file reading and `stdin` piping (`docker exec -i`).
pub fn migrate_template_database<F>(
    migrations_dir: &Path,
    stdin_mode: bool,
    mut run_sql: F,
) -> io::Result<Vec<tacho_core::decide::ops::kit::SqlxMigrationStep>>
where
    F: FnMut(Option<&Path>, Option<&str>) -> io::Result<()>,
{
    let mut files: Vec<(String, PathBuf, String)> = Vec::new();
    if migrations_dir.is_dir() {
        for entry in fs::read_dir(migrations_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                let text = fs::read_to_string(&path)?;
                files.push((fname.to_string(), path, text));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let raw_pairs: Vec<(String, String)> = files
        .iter()
        .map(|(n, _, t)| (n.clone(), t.clone()))
        .collect();
    let planned =
        tacho_core::decide::ops::kit::plan_template_migrations(&raw_pairs, &BTreeMap::new())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let create_table_sql = "CREATE TABLE IF NOT EXISTS _sqlx_migrations (\
        version BIGINT PRIMARY KEY, \
        description TEXT NOT NULL, \
        installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), \
        success BOOLEAN NOT NULL, \
        checksum BYTEA NOT NULL, \
        execution_time BIGINT NOT NULL\
    );";
    run_sql(None, Some(create_table_sql))?;

    for (entry, (_, path, text)) in planned.iter().zip(files.iter()) {
        if stdin_mode {
            run_sql(None, Some(text))?;
        } else {
            run_sql(Some(path.as_path()), None)?;
        }
        let desc_escaped = entry.description.replace('\'', "''");
        let insert_sql = format!(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES ({}, '{desc_escaped}', TRUE, decode('{}', 'hex'), 0) \
             ON CONFLICT (version) DO NOTHING;",
            entry.version, entry.checksum_hex
        );
        run_sql(None, Some(&insert_sql))?;
    }
    Ok(planned)
}
