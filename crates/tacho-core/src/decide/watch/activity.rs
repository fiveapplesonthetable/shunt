//! Worktree activity observation (`Activity`) and pure aggregation helpers.
//!
//! Requirements R7–R12:
//! - `commits`: count of commits in `base..HEAD`; `last_commit`: newest commit timestamp.
//! - `files`: union of `git diff --no-renames --name-only base` and untracked non-ignored files;
//!   `last_change`: max mtime across those files.
//! - `last_test`: newest of the proof summary mtime and any run log mtime whose last 4096 bytes
//!   match `\btook \d+s\b`.
//! - A missing or non-directory worktree yields `Activity::quiet()` (`(0, 0, None, None, None)`).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

static TOOK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\btook \d+s\b").unwrap());

/// Number of trailing bytes inspected in a run log for a `\btook \d+s\b` line.
pub const RUN_LOG_TAIL_BYTES: usize = 4096;

/// What a worktree shows: commits and changed files against its pinned base SHA,
/// and when a commit, file edit, or check run last occurred (`None` when never).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Activity {
    pub commits: u32,
    pub files: u32,
    pub last_commit: Option<i64>,
    pub last_change: Option<i64>,
    pub last_test: Option<i64>,
}

impl Activity {
    /// Empty activity for a missing, unknown, or untouched worktree.
    pub const fn quiet() -> Self {
        Self {
            commits: 0,
            files: 0,
            last_commit: None,
            last_change: None,
            last_test: None,
        }
    }

    /// True when the worktree has zero commits above base and zero modified/untracked files.
    pub const fn is_empty(&self) -> bool {
        self.commits == 0 && self.files == 0
    }
}

/// Whether the last 4096 bytes of a run log contain a `\btook \d+s\b` completion marker.
pub fn has_took_line(log_bytes: &[u8]) -> bool {
    let start = log_bytes.len().saturating_sub(RUN_LOG_TAIL_BYTES);
    let tail = String::from_utf8_lossy(&log_bytes[start..]);
    TOOK_RE.is_match(&tail)
}

/// Compute [`Activity`] purely from observed git outputs and file mtimes.
///
/// - `commit_times`: unix timestamps from `git log --format=%ct <base>..HEAD` (newest first).
/// - `diff_files`: paths from `git diff --no-renames --name-only <base>`.
/// - `untracked_files`: paths from `git ls-files --others --exclude-standard`.
/// - `mtimes`: map from relative path to mtime in unix seconds.
/// - `proof_summary_mtime`: mtime of `<proof_dir>/summary.json`, if present.
/// - `run_logs`: `(mtime, tail_bytes)` of `<run_dir>/*.log` files.
pub fn compute_activity(
    worktree_exists: bool,
    commit_times: &[i64],
    diff_files: &[String],
    untracked_files: &[String],
    mtimes: &BTreeMap<String, i64>,
    proof_summary_mtime: Option<i64>,
    run_logs: &[(i64, Vec<u8>)],
) -> Activity {
    if !worktree_exists {
        return Activity::quiet();
    }
    let commits = commit_times.len() as u32;
    let last_commit = commit_times.first().copied();

    let changed: BTreeSet<&str> = diff_files
        .iter()
        .chain(untracked_files.iter())
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    let files = changed.len() as u32;
    let last_change = changed.iter().filter_map(|f| mtimes.get(*f).copied()).max();

    let last_test = proof_summary_mtime
        .into_iter()
        .chain(
            run_logs
                .iter()
                .filter(|(_, bytes)| has_took_line(bytes))
                .map(|(mtime, _)| *mtime),
        )
        .max();

    Activity {
        commits,
        files,
        last_commit,
        last_change,
        last_test,
    }
}
