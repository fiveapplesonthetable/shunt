//! Janitor side effects chosen by `decide` (`TidyDecision`): rescue dirty or unpushed work to
//! a ref, then remove the worktree and forget it.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::watch::{ProcEntry, TidyDecision};

use crate::fleet::FleetStore;

/// Paths and ports held open by live processes on the host (`cwd`, `argv`, `/proc/<pid>/fd`).
#[derive(Debug, Clone, Default)]
pub struct LiveProcessHolds {
    pub paths: BTreeSet<String>,
    pub ports: BTreeSet<u16>,
}

/// Scans `/proc` and `procs` for any worktree paths referenced by live process `cwd`, `argv`,
/// or open file descriptors (`/proc/<pid>/fd/*`).
pub fn scan_live_process_holds(
    procs: &[ProcEntry],
    candidate_paths: &[String],
) -> LiveProcessHolds {
    let mut holds = LiveProcessHolds::default();
    if candidate_paths.is_empty() {
        return holds;
    }

    for p in procs {
        for cand in candidate_paths {
            if cand.is_empty() {
                continue;
            }
            if (!p.cwd.is_empty() && (p.cwd == *cand || p.cwd.starts_with(&format!("{cand}/"))))
                || p.args.contains(cand)
            {
                holds.paths.insert(cand.clone());
            }
        }
        let fd_dir = format!("/proc/{}/fd", p.pid);
        if let Ok(entries) = fs::read_dir(&fd_dir) {
            for entry in entries.flatten() {
                if let Ok(target) = fs::read_link(entry.path()) {
                    let target_str = target.to_string_lossy();
                    for cand in candidate_paths {
                        if !cand.is_empty()
                            && (target_str == *cand || target_str.starts_with(&format!("{cand}/")))
                        {
                            holds.paths.insert(cand.clone());
                        }
                    }
                }
            }
        }
    }

    holds
}

/// Runs `git -C <dir> <args>` confined strictly to `dir` as its own worktree toplevel and
/// returns trimmed stdout, failing on a non-zero exit so callers never act on a half-done git
/// step. Never prompts, opens an editor, pages, or walks up into an enclosing repository.
pub fn git_stdout(dir: &Path, args: &[&str]) -> io::Result<String> {
    let sub = args.first().copied().unwrap_or("");
    if !super::observe::is_git_checkout(dir) {
        return Err(io::Error::other(format!(
            "git {sub} failed: {} is not a git worktree toplevel",
            dir.display()
        )));
    }
    let canon = dir.canonicalize()?;
    let ceiling = canon.parent().unwrap_or(&canon);
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(&canon)
        .args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {sub} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Executes [`TidyDecision`]s. Returns the paths that were removed.
///
/// Error policy (shared with the rest of the executor): a failed rescue or removal is recorded
/// as a `*_failed` decision with its reason and the tree is kept for a later tick; only store
/// errors abort.
pub fn apply_tidy_decisions(
    store: &mut FleetStore,
    decisions: &[TidyDecision],
    now: i64,
    dry_run: bool,
) -> io::Result<Vec<String>> {
    let mut removed = Vec::new();
    for d in decisions {
        match d {
            TidyDecision::RemoveTree {
                path,
                rescue,
                because,
            } => {
                println!(
                    "{}RemoveTree {path} (rescue={rescue}): {because}",
                    if dry_run { "[dry-run] " } else { "" }
                );
                if dry_run {
                    continue;
                }
                if *rescue {
                    if let Err(e) = rescue_worktree(store, Path::new(path), now) {
                        let msg = format!("keeping worktree {path}: rescue failed: {e}");
                        eprintln!("[tacho-fleet] {msg}");
                        store.record_decision("watch", "rescue_failed", path, &msg)?;
                        continue;
                    }
                }
                match remove_worktree(store, path, because) {
                    Ok(()) => removed.push(path.clone()),
                    Err(e) => {
                        let msg = format!("keeping worktree {path}: {e}");
                        eprintln!("[tacho-fleet] {msg}");
                        store.record_decision("watch", "remove_worktree_failed", path, &msg)?;
                    }
                }
            }
            TidyDecision::RemoveCheckout {
                path,
                rescue,
                because,
            } => {
                println!(
                    "{}RemoveCheckout {path} (rescue={rescue}): {because}",
                    if dry_run { "[dry-run] " } else { "" }
                );
                if dry_run {
                    continue;
                }
                if *rescue {
                    if let Err(e) = rescue_worktree(store, Path::new(path), now) {
                        let msg = format!("keeping checkout {path}: rescue failed: {e}");
                        eprintln!("[tacho-fleet] {msg}");
                        store.record_decision("watch", "rescue_failed", path, &msg)?;
                        continue;
                    }
                }
                match remove_worktree(store, path, because) {
                    Ok(()) => removed.push(path.clone()),
                    Err(e) => {
                        let msg = format!("keeping checkout {path}: {e}");
                        store.record_decision("watch", "remove_worktree_failed", path, &msg)?;
                    }
                }
            }
            other => {
                println!("{}{:?}", if dry_run { "[dry-run] " } else { "" }, other);
            }
        }
    }
    Ok(removed)
}

/// Removes a verified git worktree/checkout and forgets it.
///
/// Refuses any directory that is not its own git worktree toplevel (`--show-toplevel == dir`),
/// so non-git directories are never deleted. Linked git worktrees (`.git` file) are removed via
/// `git worktree remove --force` from the parent repository; standalone git checkouts (`.git`
/// directory) whose toplevel is `dir` are removed after `is_git_checkout` verification.
fn remove_worktree(store: &mut FleetStore, path: &str, because: &str) -> io::Result<()> {
    let p = Path::new(path);
    if p.exists() {
        if !super::observe::is_git_checkout(p) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to remove {path}: directory is not a git worktree toplevel"),
            ));
        }
        if p.join(".git").is_file() {
            let parent = p
                .parent()
                .filter(|d| super::observe::inside_git_checkout(d));
            let mut cmd = Command::new("git");
            if let Some(parent) = parent {
                cmd.arg("-C").arg(parent);
            }
            let out = cmd
                .args(["worktree", "remove", "--force", path])
                .stdin(std::process::Stdio::null())
                .output()?;
            if !out.status.success() && p.exists() {
                return Err(io::Error::other(format!(
                    "git worktree remove {path} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        } else {
            std::fs::remove_dir_all(p)?;
        }
    }
    store.mutate(Some(("watch", "remove_worktree", path, because)), |st| {
        st.worktrees.retain(|_, w| w.path != path);
        for a in st.agents.values_mut() {
            if a.worktree == path {
                a.worktree.clear();
            }
        }
    })
}

/// Archives a worktree's full state (tracked, untracked, unpushed) as a commit under
/// `refs/heads/rescue/<slug>-<now>` (and `refs/tacho/rescue/<slug>-<now>`) without touching
/// `git stash` or the real git index.
pub fn rescue_worktree(store: &mut FleetStore, wt_p: &Path, now: i64) -> io::Result<String> {
    if !super::observe::is_git_checkout(wt_p) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing rescue in {}: directory is not a git worktree toplevel",
                wt_p.display()
            ),
        ));
    }
    let slug = wt_p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("worktree");
    let tree_sha = tree_of_worktree(wt_p)?;
    // An unborn branch has no HEAD, so the snapshot becomes a root commit; any other
    // rev-parse failure would silently drop history from the rescue, so it is reported.
    let has_head = git_stdout(wt_p, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_ok();
    let parent_sha = if has_head {
        git_stdout(wt_p, &["rev-parse", "HEAD"])?
    } else {
        String::new()
    };
    let message = format!("tacho rescue snapshot for {slug}");
    let mut commit_args = vec!["commit-tree", tree_sha.as_str()];
    if !parent_sha.is_empty() {
        commit_args.extend(["-p", parent_sha.as_str()]);
    }
    commit_args.extend(["-m", message.as_str()]);
    let commit_sha = git_stdout(wt_p, &commit_args)?;
    let rescue_branch_ref = format!("refs/heads/rescue/{slug}-{now}");
    let rescue_tacho_ref = format!("refs/tacho/rescue/{slug}-{now}");
    git_stdout(wt_p, &["update-ref", &rescue_branch_ref, &commit_sha])?;
    git_stdout(wt_p, &["update-ref", &rescue_tacho_ref, &commit_sha])?;
    let because = format!("archived dirty/unpushed worktree to {rescue_branch_ref} ({commit_sha})");
    store.record_decision("watch", "rescue_worktree", slug, &because)?;
    Ok(rescue_branch_ref)
}

/// Computes the exact git tree hash of the entire working tree (tracked + untracked files,
/// excluding `.tachoproof/`, `.tachorun/`, and `.tachoenv`) using a temporary git index file
/// without mutating the user's real git index.
///
/// Refuses to run when `root` is not its own git worktree toplevel (`--show-toplevel == root`),
/// so a plain directory inside an enclosing repository can never walk up and stage the parent repo.
pub fn tree_of_worktree(root: &Path) -> io::Result<String> {
    if !super::observe::is_git_checkout(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing tree_of_worktree in {}: directory is not a git worktree toplevel",
                root.display()
            ),
        ));
    }
    let canon = root.canonicalize()?;
    let ceiling = canon.parent().unwrap_or(&canon);
    let tmp_dir = tempfile::tempdir()?;
    let tmp_index = tmp_dir.path().join("index");

    let run_temp_git = |args: &[&str]| -> io::Result<std::process::Output> {
        use std::os::unix::process::CommandExt;
        let child = Command::new("git")
            .args(args)
            .current_dir(&canon)
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()?;
        let exited =
            crate::sampler::wait_child_exit(&child, std::time::Duration::from_secs(30))?;
        if !exited {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        }
        let out = child.wait_with_output()?;
        if !exited {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git {:?} in temp index timed out", args.first()),
            ));
        }
        Ok(out)
    };

    let head_tree_out = run_temp_git(&["rev-parse", "--verify", "-q", "HEAD^{tree}"])?;
    if head_tree_out.status.success() {
        let head_tree = String::from_utf8_lossy(&head_tree_out.stdout)
            .trim()
            .to_string();
        if !head_tree.is_empty() {
            let rt_out = run_temp_git(&["read-tree", &head_tree])?;
            if !rt_out.status.success() {
                return Err(io::Error::other(format!(
                    "git read-tree {head_tree} in temp index failed: {}",
                    String::from_utf8_lossy(&rt_out.stderr).trim()
                )));
            }
        }
    }

    let add_out = run_temp_git(&["add", "-A"])?;
    if !add_out.status.success() {
        return Err(io::Error::other(format!(
            "git add -A in temp index failed: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        )));
    }

    // Remove internal kit files from temporary index so writing `.tachoproof` never invalidates the proof tree
    let rm_out = run_temp_git(&[
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
    ])?;
    if !rm_out.status.success() {
        return Err(io::Error::other(format!(
            "git rm --cached in temp index failed: {}",
            String::from_utf8_lossy(&rm_out.stderr).trim()
        )));
    }

    let wt_out = run_temp_git(&["write-tree"])?;
    if !wt_out.status.success() {
        return Err(io::Error::other(format!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&wt_out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&wt_out.stdout).trim().to_string())
}
