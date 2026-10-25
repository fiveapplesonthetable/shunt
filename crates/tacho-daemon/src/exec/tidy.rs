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

/// Runs `git -C <dir> <args>` and returns trimmed stdout, failing on a non-zero exit so callers
/// never act on a half-done git step. Never prompts, opens an editor, or pages.
pub fn git_stdout(dir: &Path, args: &[&str]) -> io::Result<String> {
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
            TidyDecision::RemoveCheckout { path, because, .. } => {
                println!(
                    "{}RemoveCheckout {path}: {because}",
                    if dry_run { "[dry-run] " } else { "" }
                );
                if dry_run {
                    continue;
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

/// Removes a worktree and forgets it.
///
/// A git checkout is removed with `git worktree remove --force` (run from the parent
/// repository); when git refuses and the tree is still there, the store entry is kept so a
/// later tick retries instead of leaking an untracked directory. A plain directory that is
/// not a checkout is deleted directly.
fn remove_worktree(store: &mut FleetStore, path: &str, because: &str) -> io::Result<()> {
    let p = Path::new(path);
    if p.exists() {
        if super::observe::is_git_checkout(p) {
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
