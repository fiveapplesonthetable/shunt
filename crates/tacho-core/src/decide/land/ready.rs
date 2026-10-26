//! The ready queue (path A input): which passes still hold, their merge order, sibling
//! conflicts, and when the owner hears about it.

use super::train::Sha;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// The forge's mergeability verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Conflicted {
    Yes,
    No,
    /// Still computing (or a value we do not recognise).
    Unknown,
}

impl Conflicted {
    /// Parse the forge's string; anything unrecognised is `Unknown` (keeps the previous answer).
    pub fn from_forge(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "CONFLICTING" => Self::Yes,
            "MERGEABLE" => Self::No,
            _ => Self::Unknown,
        }
    }
}

/// `Unknown` keeps the previous answer.
pub fn is_conflicting(m: Conflicted, was: bool) -> bool {
    match m {
        Conflicted::Yes => true,
        Conflicted::No => false,
        Conflicted::Unknown => was,
    }
}

/// A batch pass for one PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passed {
    pub sha: Sha,
    pub main: Sha,
    /// The batch's merge order.
    pub order: Vec<u64>,
    pub batch: String,
}

impl Passed {
    pub fn new(sha: &str, main: &str, order: &[u64], batch: &str) -> Self {
        Self {
            sha: sha.into(),
            main: main.into(),
            order: order.to_vec(),
            batch: batch.into(),
        }
    }
}

/// What the ready queue needs to know about an open PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyPr {
    pub number: u64,
    pub head: Sha,
    pub conflicting: bool,
}

/// A pass still holds while the head is the one that passed, the PR does not conflict, main's
/// history since the pass is readable (`merged_since` is `Some`), and main has only taken
/// members ahead of this PR in the batch's order.
pub fn still_ready(pr: &ReadyPr, passed: &Passed, merged_since: Option<&[u64]>) -> bool {
    if passed.sha != pr.head || pr.conflicting {
        return false;
    }
    let Some(merged) = merged_since else {
        return false;
    };
    let Some(pos) = passed.order.iter().position(|&n| n == pr.number) else {
        return false;
    };
    let before: BTreeSet<u64> = passed.order[..pos].iter().copied().collect();
    merged.iter().all(|n| before.contains(n))
}

fn merge_subject() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Merge pull request #(\d+)\b").expect("static regex"))
}

/// The PR number a first-parent subject merged, if it is a PR merge.
pub fn merged_pr_of(subject: &str) -> Option<u64> {
    merge_subject()
        .captures(subject)
        .and_then(|c| c[1].parse().ok())
}

/// PRs main merged since a pass. `first_parent_subjects` is `None` when the pass's main is not
/// an ancestor of main (or history is unreadable). Any first-parent commit that is not a PR
/// merge makes the answer `None`: the pass lapses.
pub fn merged_since<S: AsRef<str>>(first_parent_subjects: Option<&[S]>) -> Option<Vec<u64>> {
    first_parent_subjects?
        .iter()
        .map(|s| merged_pr_of(s.as_ref()))
        .collect()
}

/// A pass counts only when its batch ran every area the PR touches, apart from `skipped`.
pub fn covered(touched: &BTreeSet<String>, checked: &[String], skipped: &BTreeSet<String>) -> bool {
    touched
        .iter()
        .filter(|a| !skipped.contains(*a))
        .all(|a| checked.contains(a))
}

/// The areas a batch counts as checked: its `checked_areas`, or else its `areas` — except
/// that a PR touching a post-merge area gets nothing by default (it must be explicit).
pub fn checked_areas<'a>(
    checked: Option<&'a [String]>,
    batch_areas: &'a [String],
    pr_areas: &BTreeSet<String>,
    post_merge: &[String],
) -> &'a [String] {
    match checked {
        Some(c) => c,
        None if pr_areas.iter().any(|a| post_merge.contains(a)) => &[],
        None => batch_areas,
    }
}

/// Ready PRs in merge order: by when their batch started, then their place in it, then number.
pub fn ready_queue(ready: &BTreeMap<u64, Passed>, started: &BTreeMap<String, i64>) -> Vec<u64> {
    let mut keyed: Vec<(i64, usize, u64)> = ready
        .iter()
        .map(|(&n, p)| {
            let place = p.order.iter().position(|&m| m == n).unwrap_or(usize::MAX);
            (started.get(&p.batch).copied().unwrap_or(0), place, n)
        })
        .collect();
    keyed.sort();
    keyed.into_iter().map(|(_, _, n)| n).collect()
}

/// Splits the ordered queue into PRs that trial-merge onto main plus those kept before them,
/// and those that do not (siblings). `merges(ahead, n)` is the hands' `merge-tree` oracle.
pub fn split_siblings(
    queue: &[u64],
    mut merges: impl FnMut(&[u64], u64) -> bool,
) -> (Vec<u64>, Vec<u64>) {
    let (mut kept, mut siblings) = (Vec::new(), Vec::new());
    for &n in queue {
        if merges(&kept, n) {
            kept.push(n);
        } else {
            siblings.push(n);
        }
    }
    (kept, siblings)
}

/// A sibling rebase to queue: once per head (`already` maps PR -> head it was queued for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingRebase {
    pub pr: u64,
    pub head: Sha,
    pub reason: String,
}

pub fn sibling_rebases(
    siblings: &[u64],
    heads: &BTreeMap<u64, Sha>,
    already: &BTreeMap<u64, Sha>,
) -> Vec<SiblingRebase> {
    siblings
        .iter()
        .filter_map(|&n| {
            let head = heads.get(&n)?;
            (already.get(&n) != Some(head)).then(|| SiblingRebase {
                pr: n,
                head: head.clone(),
                reason: format!(
                    "#{n} passed but conflicts with a pull request queued ahead of it; a rebase is queued"
                ),
            })
        })
        .collect()
}

/// Ready drafts to mark ready for review (only a lone PR: a member of a train is made ready when
/// the train lands; marking ready starts a full CI run).
pub fn drafts_to_ready(queue: &[u64], drafts: &BTreeSet<u64>, dry_run: bool) -> Vec<u64> {
    if dry_run || queue.len() != 1 {
        return Vec::new();
    }
    queue
        .iter()
        .copied()
        .filter(|n| drafts.contains(n))
        .collect()
}

/// Combined train PRs are never in the general queue; only their own path mails them.
pub fn without_train_prs(queue: &[u64], train_prs: &BTreeSet<u64>) -> Vec<u64> {
    queue
        .iter()
        .copied()
        .filter(|n| !train_prs.contains(n))
        .collect()
}

/// The owner hears only when the list gains a PR.
pub fn grew(before: &[u64], now: &[u64]) -> bool {
    now.iter().any(|n| !before.contains(n))
}

/// The one command that lands the current train, or `""` without one.
pub fn land_command(tool: &str, train_id: &str) -> String {
    if train_id.is_empty() {
        String::new()
    } else {
        format!("{tool} land {train_id}")
    }
}

/// Body of the merge-queue issue.
pub fn queue_issue_body(tool: &str, ready: &[u64], train_id: &str) -> String {
    let mut out = format!(
        "{tool} keeps this command current. It holds only pull requests that passed on the main \
         they land on, and `{tool} land` refuses if main moved.\n\n"
    );
    if ready.is_empty() {
        out.push_str("Nothing is ready.\n");
        return out;
    }
    for n in ready {
        out.push_str(&format!("- #{n}\n"));
    }
    let cmd = land_command(tool, train_id);
    if !cmd.is_empty() {
        out.push_str(&format!("\n    {cmd}\n"));
    }
    out
}
