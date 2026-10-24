//! Rebase hand-off and shape/replay decisions: which conflicting PR an agent rebases next,
//! which may not be touched, and when a branch is replayed onto its base.

use super::config::LandConfig;
use super::train::{Sha, Train, TrainStatus};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A queued rebase (`hands[n]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Hands {
    pub sha: Sha,
    /// Files that conflict; empty means "the files it shares with PRs queued ahead".
    pub files: Vec<String>,
    /// Stacked parent PR, if any.
    pub parent: Option<u64>,
    /// A sibling rebase (mergeable alone, conflicts behind the queue).
    pub sibling: bool,
}

impl Hands {
    pub fn files(files: &[String]) -> Self {
        Self {
            files: files.to_vec(),
            ..Self::default()
        }
    }
}

/// The rebase brief.
pub fn hands_task(n: u64, h: &Hands) -> String {
    let onto = h
        .parent
        .map_or_else(|| "upstream/main".to_string(), |p| format!("PR #{p} head"));
    let place = if h.files.is_empty() {
        "the files it shares with the pull requests ahead of it in the queue".to_string()
    } else {
        h.files.join(", ")
    };
    format!(
        "PR #{n} no longer applies to its base. Rebase it onto {onto} as plain commits, resolving \
         conflicts in {place} by keeping both sides' intent, regenerate any screenshots the change \
         alters, run the checks for the areas it touches, and push with --force-with-lease."
    )
}

/// Facts about an open PR for barring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarPr {
    pub number: u64,
    pub draft: bool,
    pub mergeable: bool,
    pub branch: String,
}

/// Inputs to [`barred`].
#[derive(Debug, Clone)]
pub struct BarInputs<'a> {
    pub open: &'a [BarPr],
    pub in_flight: &'a BTreeSet<u64>,
    pub landed_members: &'a BTreeSet<u64>,
    pub train_prs: &'a BTreeSet<u64>,
    /// PR numbers with a held `hands-<n>` task.
    pub held_hands: &'a BTreeSet<u64>,
    pub siblings: &'a BTreeSet<u64>,
    pub train_branch_prefix: &'a str,
}

/// PRs whose rebase may not start or resume, each with why.
pub fn barred(i: &BarInputs<'_>) -> BTreeMap<u64, &'static str> {
    let mut out = BTreeMap::new();
    for &n in i.in_flight {
        out.insert(n, "it is in a train in flight");
    }
    for &n in i.landed_members {
        out.insert(n, "it landed in a train");
    }
    for &n in i.train_prs {
        out.insert(n, "it is a train pull request");
    }
    for &n in i.held_hands {
        out.insert(n, "its rebase is held");
    }
    for p in i.open {
        let why = if p.draft {
            Some("it is a draft")
        } else if p.mergeable && !i.siblings.contains(&p.number) {
            Some("it already merges")
        } else if !i.train_branch_prefix.is_empty() && p.branch.starts_with(i.train_branch_prefix) {
            Some("it is a train pull request")
        } else {
            None
        };
        if let Some(w) = why {
            out.entry(p.number).or_insert(w);
        }
    }
    out
}

/// A rebase agent's last known state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandsAgent {
    pub running: bool,
    pub task: String,
}

/// The next rebase to hand out: lowest PR first, one at a time, while there is room and the
/// machine is not busy. The same brief is never tried twice; a new conflict starts anew.
pub fn next_hands(
    hands: &BTreeMap<u64, Hands>,
    agents: &BTreeMap<u64, HandsAgent>,
    running: usize,
    limit: usize,
    busy: bool,
    barred: &BTreeSet<u64>,
) -> Option<(u64, String)> {
    if busy || running >= limit {
        return None;
    }
    hands
        .iter()
        .filter(|(n, _)| !barred.contains(n))
        .find_map(|(&n, h)| {
            let task = hands_task(n, h);
            match agents.get(&n) {
                None => Some((n, task)),
                Some(a) if !a.running && a.task != task => Some((n, task)),
                Some(_) => None,
            }
        })
}

/// Whether a `hands-<n>` run (or a `-r<k>` resume of it, or `~<k>`) targets a PR in an open train.
pub fn in_open_train(key: &str, members: &BTreeSet<u64>) -> bool {
    let base = match key.rsplit_once("-r").or_else(|| key.rsplit_once('~')) {
        Some((b, k)) if !k.is_empty() && k.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => key,
    };
    base.strip_prefix("hands-")
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| members.contains(&n))
}

/// Recorded state of an automated `replay` for one PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplayRecord {
    pub sha: Sha,
    pub pushed: Option<Sha>,
    pub base: Option<Sha>,
    pub running: bool,
}

/// Facts about an open PR for the `shape` pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapePr {
    pub number: u64,
    pub sha: Sha,
    pub branch: String,
    pub draft: bool,
    pub updated_age_secs: i64,
    pub conflicting: bool,
    pub has_merge_commits: bool,
}

/// Whether an open PR should be included in the `read` snapshot (`tick.read`):
/// a draft updated within `recent_secs` (default 20 minutes) is left alone because an agent is
/// still working on it — UNLESS its head is one we pushed in `replays`, or it is a registered
/// combined train PR with `source_heads`.
pub fn keep_in_shape_read(
    pr: &ShapePr,
    replays: &BTreeMap<u64, ReplayRecord>,
    trains: &[Train],
    recent_secs: i64,
) -> bool {
    if replays
        .values()
        .any(|r| r.pushed.as_deref() == Some(pr.sha.as_str()))
    {
        return true;
    }
    if trains.iter().any(|t| {
        t.combined_pr() == Some(pr.number) && !t.base.is_empty() && !t.source_heads.is_empty()
    }) {
        return true;
    }
    !(pr.draft && pr.updated_age_secs < recent_secs)
}

/// Whether `pr_num` at `pr_sha` is frozen because a combined train assembled from that exact
/// source head has a running batch on the train's recorded head (`tick._frozen_train_member`).
pub fn frozen_train_member(
    pr_num: u64,
    pr_sha: &str,
    trains: &[Train],
    open_heads: &BTreeMap<u64, Sha>,
    running_batch_members: &[(u64, Sha)],
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> bool {
    for t in trains {
        let Some(train_pr) = t.combined_pr() else {
            continue;
        };
        if t.source_heads.get(&pr_num).map(String::as_str) != Some(pr_sha) {
            continue;
        }
        let train_head = &t.base;
        if train_head.is_empty() || open_heads.get(&train_pr) != Some(train_head) {
            continue;
        }
        let batch_running_at_head = running_batch_members
            .iter()
            .any(|(m, h)| *m == train_pr && h == train_head);
        if batch_running_at_head && is_ancestor(pr_sha, train_head) {
            return true;
        }
    }
    false
}

/// Whether `shape` should spawn a `replay` for `pr` onto `base` (`tick.shape`).
#[allow(clippy::too_many_arguments)]
pub fn should_replay(
    pr: &ShapePr,
    base: &str,
    main: &str,
    trains: &[Train],
    hands: &BTreeMap<u64, Hands>,
    replays: &BTreeMap<u64, ReplayRecord>,
    open_heads: &BTreeMap<u64, Sha>,
    running_batch_members: &[(u64, Sha)],
    is_ancestor: impl Fn(&str, &str) -> bool,
    cfg: &LandConfig,
) -> bool {
    if trains.iter().any(|t| t.combined_pr() == Some(pr.number)) || cfg.is_train_branch(&pr.branch)
    {
        return false;
    }
    if trains.iter().any(|t| {
        t.status == TrainStatus::Open
            && t.base == main
            && t.members
                .iter()
                .filter(|m| m.pr == pr.number)
                .map(|m| m.head.as_str())
                .eq([pr.sha.as_str()])
    }) {
        return false;
    }
    if frozen_train_member(
        pr.number,
        &pr.sha,
        trains,
        open_heads,
        running_batch_members,
        is_ancestor,
    ) {
        return false;
    }
    if !(pr.conflicting || pr.has_merge_commits) {
        return false;
    }
    if hands.get(&pr.number).is_some_and(|h| h.sha == pr.sha) {
        return false;
    }
    if let Some(running) = replays.get(&pr.number) {
        if running.pushed.as_deref() == Some(pr.sha.as_str())
            && running.base.as_deref() == Some(base)
        {
            return false;
        }
        if running.sha == pr.sha && running.running {
            return false;
        }
    }
    true
}

/// Outcome of replaying a PR's commits onto its base (`R-RB-5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayOutcome {
    /// Tree matches head and head has no merge commits; nothing needed pushing.
    Clean,
    /// Replayed and pushed `new_sha` with `--force-with-lease`.
    Replayed { new_sha: Sha },
    /// Hand-written files conflicted; sorted with hand-written files before generated ones.
    Hands { files: Vec<String> },
}

/// Decides what to do when a cherry-pick during `replay` conflicts on `conflicted` (`R-RB-5`):
/// - `Ok(files)` when every conflicted file is generated (`take --ours` / `rm`, then `--continue` or `--skip`).
/// - `Err(sorted_files)` when any file is hand-written (abort and record `Hands`, ordering
///   hand-written files before generated files as in `tick.settle_replay`).
pub fn resolve_cherry_pick_conflict(
    conflicted: &[String],
    cfg: &LandConfig,
) -> Result<Vec<String>, Vec<String>> {
    if cfg.generated_areas(conflicted).is_some() {
        Ok(conflicted.to_vec())
    } else {
        let mut sorted = conflicted.to_vec();
        sorted.sort_by_key(|f| cfg.generated_area(f).is_some());
        Err(sorted)
    }
}

/// Classifies a completed cherry-pick sequence (`R-RB-5`).
pub fn classify_replay_finish(
    new_sha: &str,
    new_tree: &str,
    head_tree: &str,
    has_merge_commits: bool,
) -> ReplayOutcome {
    if new_tree == head_tree && !has_merge_commits {
        ReplayOutcome::Clean
    } else {
        ReplayOutcome::Replayed {
            new_sha: new_sha.to_string(),
        }
    }
}
