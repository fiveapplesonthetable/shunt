//! Git worktree progress polling, HEAD movement classification, and file-change scanning.
//!
//! Tracks each active agent worktree's HEAD SHA (handling loose refs, `commondir`, `packed-refs`,
//! and modern Git `reftable/tables.list`) and periodically hashes modified/untracked files via
//! `git ls-files` to emit `WorktreeCommit`, `WorktreeHeadMoved`, and `WorktreeFileChange`
//! milestones without blocking on hung `git` subprocesses.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tacho_core::{CustomOp, Input};

use crate::appender::AppenderHandle;

use super::ProcSampler;

#[derive(Debug, Clone)]
pub(super) struct WorktreeSnapshot {
    pub(super) head_sha: Option<String>,
    pub(super) reftable_marker: Option<String>,
    pub(super) latest_mtime_ns: u64,
    pub(super) last_file_scan: Instant,
    pub(super) file_hash: u64,
}

impl ProcSampler {
    pub(super) fn poll_worktree_progress(
        &mut self,
        agent_id: &str,
        cwd: &str,
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        let cwd_path = Path::new(cwd);
        if !cwd_path.is_dir() {
            return;
        }
        let git_dir = resolve_git_dir(cwd_path);
        if git_dir.is_none() {
            return;
        }

        let prev_reftable = self
            .worktree_states
            .get(cwd)
            .and_then(|s| s.reftable_marker.as_deref());
        let prev_sha = self
            .worktree_states
            .get(cwd)
            .and_then(|s| s.head_sha.as_deref());
        let (head_sha, reftable_marker) = git_dir
            .as_deref()
            .map(|gd| read_git_head_sha(cwd_path, gd, prev_reftable, prev_sha))
            .unwrap_or((None, None));

        let Some(prev) = self.worktree_states.get_mut(cwd) else {
            // First time seeing this worktree: baseline snapshot
            let (latest_mtime_ns, _newest, file_hash) =
                scan_worktree_files(cwd_path, Duration::from_secs(5)).unwrap_or((0, None, 0));

            self.worktree_states.insert(
                cwd.to_string(),
                WorktreeSnapshot {
                    head_sha,
                    reftable_marker,
                    latest_mtime_ns,
                    last_file_scan: Instant::now(),
                    file_hash,
                },
            );
            return;
        };
        prev.reftable_marker = reftable_marker;

        let mut head_changed = false;
        if let Some(new_sha) = head_sha
            .as_ref()
            .filter(|s| Some(*s) != prev.head_sha.as_ref())
        {
            head_changed = true;
            let (name, args) = classify_head_move(cwd_path, prev.head_sha.as_deref(), new_sha);
            let mut args = args;
            args.insert("cwd".to_string(), cwd.to_string());
            emit_worktree_event(
                appender,
                agent_id,
                name,
                Some(new_sha.clone()),
                args,
                now_ns,
            );
            prev.head_sha = head_sha;
        }

        // Check file modifications at most once every worktree_scan_secs
        let should_scan_files = !head_changed
            && prev.last_file_scan.elapsed() >= Duration::from_secs(self.worktree_scan_secs);

        if should_scan_files {
            prev.last_file_scan = Instant::now();
            if let Ok((max_mtime_ns, newest_rel, hash)) =
                scan_worktree_files(cwd_path, Duration::from_secs(5))
            {
                if hash != prev.file_hash {
                    prev.file_hash = hash;
                    prev.latest_mtime_ns = max_mtime_ns;

                    if let Some(rel) = newest_rel {
                        let mut args = BTreeMap::new();
                        args.insert("cwd".to_string(), cwd.to_string());
                        args.insert("detail".to_string(), rel);
                        emit_worktree_event(
                            appender,
                            agent_id,
                            "WorktreeFileChange",
                            None,
                            args,
                            now_ns,
                        );
                    }
                }
            }
        }
    }
}

/// Decides how a worktree HEAD change reads in the trace.
///
/// A pure fast-forward from the previous HEAD is `WorktreeCommit` with the number of new
/// commits; anything else (checkout, reset, rebase) is `WorktreeHeadMoved`. With no previous
/// HEAD (first commit seen) it counts the commits reachable from the new one.
fn classify_head_move(
    cwd: &Path,
    old_sha: Option<&str>,
    new_sha: &str,
) -> (&'static str, BTreeMap<String, String>) {
    let count = |range: &str| -> Option<u64> {
        run_git(cwd, &["rev-list", "--count", range], Duration::from_secs(2))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let mut args = BTreeMap::new();
    let Some(old_sha) = old_sha else {
        args.insert("sha".to_string(), new_sha.to_string());
        args.insert(
            "commits".to_string(),
            count(new_sha).unwrap_or(1).to_string(),
        );
        return ("WorktreeCommit", args);
    };
    let ahead = count(&format!("{}..{}", old_sha, new_sha)).unwrap_or(0);
    let behind = count(&format!("{}..{}", new_sha, old_sha)).unwrap_or(0);
    if behind == 0 && ahead > 0 {
        args.insert("sha".to_string(), new_sha.to_string());
        args.insert("commits".to_string(), ahead.to_string());
        ("WorktreeCommit", args)
    } else {
        args.insert("from".to_string(), old_sha.to_string());
        args.insert("to".to_string(), new_sha.to_string());
        ("WorktreeHeadMoved", args)
    }
}

/// Emits a worktree progress instant on the agent's track; the recorder turns these into
/// worktree slices and commit counters.
fn emit_worktree_event(
    appender: &AppenderHandle,
    agent_id: &str,
    name: &str,
    id: Option<String>,
    args: BTreeMap<String, String>,
    now_ns: u64,
) {
    let _ = appender.send_input(
        Input::Custom {
            source_ts_ns: Some(now_ns),
            track: format!("agent:{}", agent_id),
            op: CustomOp::Instant,
            name: Some(name.to_string()),
            id,
            args,
            value: None,
            flow_out: None,
            flow_in: None,
        },
        now_ns,
    );
}

fn resolve_git_dir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = cwd.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        if let Ok(content) = fs::read_to_string(&dot_git) {
            if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                let gitdir_path = Path::new(rest.trim());
                if gitdir_path.is_absolute() {
                    return Some(gitdir_path.to_path_buf());
                } else {
                    return Some(cwd.join(gitdir_path));
                }
            }
        }
    }
    None
}

/// Executes a git command in `cwd` with a strict timeout, killing it on expiration.
///
/// Runs a read-only `git` command in `cwd` with `--no-optional-locks` (so a timeout `SIGKILL`
/// never leaves `.git/index.lock` behind) and a bounded timeout.
///
/// Stdout is drained on a helper thread while we wait, so output larger than the pipe
/// buffer (a big `ls-files`) cannot deadlock git against us. Git runs in its own process
/// group so a timeout kills any helpers it spawned too.
pub fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> io::Result<String> {
    run_git_with_ceiling(cwd, args, timeout, None)
}

/// Runs `git` confined strictly to `cwd` as its own worktree toplevel (`cwd/.git` must exist,
/// `GIT_CEILING_DIRECTORIES` prevents walking up into any parent repository, and
/// `rev-parse --show-toplevel` must equal `cwd`).
pub fn run_git_in_worktree(cwd: &Path, args: &[&str], timeout: Duration) -> io::Result<String> {
    if cwd.as_os_str().is_empty() || !cwd.join(".git").exists() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a git worktree toplevel (missing .git)", cwd.display()),
        ));
    }
    let canon = cwd.canonicalize()?;
    let ceiling = canon.parent().unwrap_or(&canon);
    let toplevel = run_git_with_ceiling(&canon, &["rev-parse", "--show-toplevel"], timeout, Some(ceiling))?;
    let top_canon = Path::new(toplevel.trim()).canonicalize()?;
    if top_canon != canon {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing git in {}: toplevel is {}",
                canon.display(),
                top_canon.display()
            ),
        ));
    }
    run_git_with_ceiling(&canon, args, timeout, Some(ceiling))
}

fn run_git_with_ceiling(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
    ceiling: Option<&Path>,
) -> io::Result<String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    if let Some(c) = ceiling {
        cmd.env("GIT_CEILING_DIRECTORIES", c);
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::Builder::new()
        .name("tacho-git-stdout".to_string())
        .spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).map(|_| out)
        })?;

    let exited = wait_child_exit(&child, timeout)?;
    if !exited {
        // Still unreaped, so the group id cannot have been recycled.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    }
    let status = child.wait()?;
    let out = reader
        .join()
        .map_err(|_| io::Error::other("git stdout reader panicked"))??;
    if !exited {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "git command timed out",
        ));
    }
    if !status.success() {
        return Err(io::Error::other(format!(
            "git command failed: status {:?}",
            status.code()
        )));
    }
    String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Waits up to `timeout` for `child` to exit without reaping it. Returns whether it exited.
pub(crate) fn wait_child_exit(child: &std::process::Child, timeout: Duration) -> io::Result<bool> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) as i32 };
    if pidfd < 0 {
        // No pidfd (pre-5.3 kernel): fall back to waitid(WNOWAIT) on a helper thread.
        let pid = child.id();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("tacho-git-wait".to_string())
            .spawn(move || {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                while unsafe {
                    libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
                } != 0
                    && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                {
                }
                let _ = tx.send(());
            })?;
        return Ok(rx.recv_timeout(timeout).is_ok());
    }
    // SAFETY: pidfd_open returned a fresh fd that we own.
    let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(pidfd) };
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&pidfd),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            n if n > 0 => return Ok(true),
            0 => return Ok(false),
            _ => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
}

fn read_git_head_sha(
    cwd: &Path,
    git_dir: &Path,
    prev_reftable: Option<&str>,
    prev_sha: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Ok(head_content) = fs::read_to_string(git_dir.join("HEAD")) else {
        return (None, None);
    };
    let trimmed = head_content.trim();
    if let Some(ref_rel) = trimmed.strip_prefix("ref:") {
        let ref_rel = ref_rel.trim();
        if ref_rel != "refs/heads/.invalid" {
            let direct_ref = git_dir.join(ref_rel);
            if let Ok(sha) = fs::read_to_string(&direct_ref) {
                let s = sha.trim();
                if !s.is_empty() {
                    return (Some(s.to_string()), None);
                }
            }
        }
        // Check commondir if this is a linked worktree
        let common_dir = fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .map(|c| {
                let p = Path::new(c.trim());
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    git_dir.join(p)
                }
            })
            .unwrap_or_else(|| git_dir.to_path_buf());
        if ref_rel != "refs/heads/.invalid" {
            if let Ok(sha) = fs::read_to_string(common_dir.join(ref_rel)) {
                let s = sha.trim();
                if !s.is_empty() {
                    return (Some(s.to_string()), None);
                }
            }
            if let Ok(packed) = fs::read_to_string(common_dir.join("packed-refs")) {
                for line in packed.lines() {
                    if line.starts_with('#') || line.starts_with('^') {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    if let (Some(sha), Some(rname)) = (parts.next(), parts.next()) {
                        if rname == ref_rel {
                            return (Some(sha.to_string()), None);
                        }
                    }
                }
            }
        }
        // Support modern Git reftable storage without spawning git on unchanged ticks
        let tables_list_path = if git_dir.join("reftable/tables.list").exists() {
            Some(git_dir.join("reftable/tables.list"))
        } else if common_dir.join("reftable/tables.list").exists() {
            Some(common_dir.join("reftable/tables.list"))
        } else {
            None
        };
        if let Some(tl_path) = tables_list_path {
            if let Ok(marker) = fs::read_to_string(tl_path) {
                if prev_reftable == Some(marker.as_str()) && prev_sha.is_some() {
                    return (prev_sha.map(|s| s.to_string()), Some(marker));
                }
                if let Ok(out) = run_git(cwd, &["rev-parse", "HEAD"], Duration::from_secs(2)) {
                    let sha = out.trim().to_string();
                    if !sha.is_empty() {
                        return (Some(sha), Some(marker));
                    }
                }
                return (prev_sha.map(|s| s.to_string()), Some(marker));
            }
        }
        (None, None)
    } else if !trimmed.is_empty() {
        (Some(trimmed.to_string()), None)
    } else {
        (None, None)
    }
}

/// Scans the worktree for modified and untracked files respecting `.gitignore`,
/// returning `(max_mtime_ns, newest_rel_path, 64-bit content hash)`.
pub fn scan_worktree_files(
    cwd: &Path,
    timeout: Duration,
) -> io::Result<(u64, Option<String>, u64)> {
    let out = run_git_in_worktree(
        cwd,
        &["ls-files", "-m", "-o", "--exclude-standard", "-z"],
        timeout,
    )?;

    let mut files = Vec::new();
    let mut max_mtime_ns = 0u64;
    let mut newest_rel = None;

    for rel_path in out.split('\0') {
        if rel_path.is_empty() {
            continue;
        }
        let full_path = cwd.join(rel_path);
        if let Ok(meta) = fs::metadata(&full_path) {
            let mtime = (meta.mtime() as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.mtime_nsec() as u64);
            let size = meta.len();
            files.push((rel_path.to_string(), mtime, size));
            if mtime > max_mtime_ns {
                max_mtime_ns = mtime;
                newest_rel = Some(rel_path.to_string());
            }
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for (path, mtime, size) in &files {
        hasher.update(path.as_bytes());
        hasher.update(&mtime.to_le_bytes());
        hasher.update(&size.to_le_bytes());
    }
    let hash = hasher.digest();

    Ok((max_mtime_ns, newest_rel, hash))
}

/// Backward-compatible wrapper for scanning latest mtime.
pub fn scan_worktree_latest_mtime_ns(cwd: &Path) -> (u64, Option<String>) {
    scan_worktree_files(cwd, Duration::from_secs(5))
        .map(|(m, p, _)| (m, p))
        .unwrap_or((0, None))
}
