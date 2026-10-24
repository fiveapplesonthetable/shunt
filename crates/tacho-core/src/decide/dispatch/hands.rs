//! Rebases that need hands: which one goes to an agent next, what it is told, and which PRs are
//! barred from rebasing (R35–R39, R41).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::config::DispatchConfig;
use super::model::{HandsRec, Holds, Mergeable, OpenPr, RunStatus, Runs, TrainView};

/// Whether a `hands-N` (or `hands-N~k`) key targets a PR carried by an open train (R38).
pub fn in_open_train(key: &str, members: &BTreeSet<u64>, cfg: &DispatchConfig) -> bool {
    key.starts_with(&cfg.hands_prefix) && cfg.hands_pr(key).is_some_and(|n| members.contains(&n))
}

/// Whether a `hands-N` (or `hands-N~k`) key targets a PR in `barred` (R38).
pub fn targets_barred(key: &str, barred: &BTreeSet<u64>, cfg: &DispatchConfig) -> bool {
    key.starts_with(&cfg.hands_prefix) && cfg.hands_pr(key).is_some_and(|n| barred.contains(&n))
}

/// The brief for the agent that rebases PR `n`, which no longer applies to its base (R36).
pub fn hands_task(n: u64, hands: &HandsRec) -> String {
    let onto = match hands.parent {
        Some(p) => format!("PR #{p} head"),
        None => "upstream/main".to_string(),
    };
    let where_ = if hands.files.is_empty() {
        "the files it shares with the pull requests ahead of it in the queue".to_string()
    } else {
        hands.files.join(", ")
    };
    format!(
        "PR #{n} no longer applies to its base. Rebase it onto {onto} as plain commits, \
         resolving conflicts in {where_} by keeping both sides' intent, regenerate any \
         screenshots the change alters, run the checks for the areas it touches, and push with \
         --force-with-lease."
    )
}

/// The open PRs whose rebase must not start or resume: drafts, held ones, ones that already merge
/// (unless in `siblings`), and anything a train owns (R37).
pub fn barred(
    open_prs: &[OpenPr],
    trains: &TrainView,
    holds: &Holds,
    siblings: &BTreeSet<u64>,
    cfg: &DispatchConfig,
) -> BTreeSet<u64> {
    let mut out: BTreeSet<u64> = trains
        .in_flight
        .union(&trains.landed)
        .copied()
        .chain(trains.train_prs.iter().copied())
        .collect();
    for key in holds.keys() {
        if let Some(n) = cfg.hands_pr(key) {
            out.insert(n);
        }
    }
    for pr in open_prs {
        let n = pr.number;
        let hands_key = cfg.hands_key(n);
        if pr.draft
            || (pr.mergeable == Mergeable::Mergeable && !siblings.contains(&n))
            || holds.contains_key(&hands_key)
            || pr.branch.starts_with(&cfg.train_branch_prefix)
        {
            out.insert(n);
        }
    }
    out
}

/// Compute `barred` using `open_prs` when available, falling back to `[]` when the forge is down,
/// and deriving `siblings` from `hands` records with `sibling == true` (R37).
pub fn barred_rebases(
    open_prs: Option<&[OpenPr]>,
    trains: &TrainView,
    holds: &Holds,
    hands: &BTreeMap<u64, HandsRec>,
    cfg: &DispatchConfig,
) -> BTreeSet<u64> {
    let siblings: BTreeSet<u64> = hands
        .iter()
        .filter(|(_, h)| h.sibling)
        .map(|(&n, _)| n)
        .collect();
    barred(open_prs.unwrap_or(&[]), trains, holds, &siblings, cfg)
}

/// The PR number and brief of the next rebase to hand to an agent, or `None`: lowest PR first,
/// while there is room and the machine is not busy (R35).
pub fn next_hands(
    hands: &BTreeMap<u64, HandsRec>,
    runs: &Runs,
    running: u32,
    limit: i32,
    busy: bool,
    barred: &BTreeSet<u64>,
    cfg: &DispatchConfig,
) -> Option<(u64, String)> {
    if busy || limit <= 0 || running as i32 >= limit {
        return None;
    }
    for (&n, rec) in hands {
        if barred.contains(&n) {
            continue;
        }
        let task = hands_task(n, rec);
        let key = cfg.hands_key(n);
        let known = runs.get(&key);
        if known.is_none_or(|r| r.status != RunStatus::Running && r.task != task) {
            return Some((n, task));
        }
    }
    None
}

/// Why a merge-check skipped a PR (`skips.py`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SkipEntry {
    pub why: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// Hands records for conflicted skipped PRs that have none at their current head (R41).
pub fn skips_to_hands(
    skipped: &[u64],
    skips: &BTreeMap<u64, SkipEntry>,
    heads: &BTreeMap<u64, String>,
    existing: &BTreeMap<u64, HandsRec>,
) -> BTreeMap<u64, HandsRec> {
    let mut queued = BTreeMap::new();
    for &n in skipped {
        let Some(skip) = skips.get(&n) else {
            continue;
        };
        let Some(head) = heads.get(&n) else {
            continue;
        };
        if skip.why != "CONFLICT" || existing.get(&n).is_some_and(|h| &h.sha == head) {
            continue;
        }
        let files = if skip.files.is_empty() {
            vec!["the files it conflicts in".to_string()]
        } else {
            skip.files.clone()
        };
        queued.insert(
            n,
            HandsRec {
                sha: head.clone(),
                files,
                parent: None,
                sibling: false,
            },
        );
    }
    queued
}

/// Filter `ready_order` by removing `conflicting` sibling PRs, queueing `hands[n]` with
/// `sibling: true` once per head, and returning `(filtered_order, announcements)` (R41, cases 96–98).
pub fn filter_sibling_conflicts(
    ready_order: &[u64],
    conflicting: &BTreeSet<u64>,
    heads: &BTreeMap<u64, String>,
    hands: &mut BTreeMap<u64, HandsRec>,
) -> (Vec<u64>, Vec<String>) {
    let filtered: Vec<u64> = ready_order
        .iter()
        .copied()
        .filter(|n| !conflicting.contains(n))
        .collect();
    let mut said = Vec::new();
    for &n in ready_order {
        if !conflicting.contains(&n) {
            continue;
        }
        let Some(head) = heads.get(&n) else {
            continue;
        };
        if hands.get(&n).is_some_and(|h| &h.sha == head) {
            continue;
        }
        hands.insert(
            n,
            HandsRec {
                sha: head.clone(),
                files: Vec::new(),
                parent: None,
                sibling: true,
            },
        );
        said.push(format!(
            "#{n} passed but conflicts with a pull request queued ahead of it; a rebase is queued"
        ));
    }
    (filtered, said)
}

/// Whether `shape` is allowed to replay an open PR (bars train PRs and `train-` branches even
/// before a train record exists; test 95).
pub fn may_shape_pr(
    pr_number: u64,
    branch: &str,
    trains: &TrainView,
    cfg: &DispatchConfig,
) -> bool {
    !trains.train_prs.contains(&pr_number)
        && !trains.in_flight.contains(&pr_number)
        && !branch.starts_with(&cfg.train_branch_prefix)
}
