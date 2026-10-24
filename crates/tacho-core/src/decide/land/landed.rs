//! Post-landing settlement, abandoned trains, in-flight members, rebase retirement, and
//! PR standing (`R-AL-1`..`R-AL-6`).

use super::assemble::LandTask;
use super::config::short;
use super::ready::merged_pr_of;
use super::rebase::Hands;
use super::train::{Train, TrainKind, TrainStatus};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

fn landed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Landed in #(\d+)").expect("static regex"))
}

/// Comment posted when closing a member PR after its combined train merges (`R-AL-1`).
pub fn landed_comment(train_pr: u64) -> String {
    format!("Landed in #{train_pr}")
}

/// The train PR number named by a `"Landed in #N"` comment, if any (`R-AL-5`).
pub fn landed_in<S: AsRef<str>>(comments: &[S]) -> Option<u64> {
    comments.iter().find_map(|body| {
        landed_re()
            .captures(body.as_ref())
            .and_then(|c| c[1].parse().ok())
    })
}

/// PR numbers in main's last `limit` (default 500) first-parent `"Merge pull request #N"`
/// commit subjects (`R-AL-2`).
pub fn merged_prs_from_subjects<S: AsRef<str>>(subjects: &[S], limit: usize) -> BTreeSet<u64> {
    subjects
        .iter()
        .take(limit)
        .filter_map(|s| merged_pr_of(s.as_ref()))
        .collect()
}

/// Combined train PR numbers that main has merged and that have not yet been settled as
/// `Landed`, in ascending order (`R-AL-1`).
pub fn newly_landed(trains: &[Train], merged: &BTreeSet<u64>) -> Vec<u64> {
    let mut out: Vec<u64> = trains
        .iter()
        .filter_map(|t| {
            let pr = t.combined_pr()?;
            (merged.contains(&pr) && t.status != TrainStatus::Landed).then_some(pr)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Combined train PR numbers closed without merging that have not yet released their members,
/// in ascending order (`R-AL-3`).
///
/// Guard: when `open_numbers.len() >= open_limit` (default 200), the code-host listing may be
/// truncated, so this check returns `Vec::new()` to avoid false abandonments.
pub fn abandoned(
    trains: &[Train],
    open_numbers: &BTreeSet<u64>,
    merged: &BTreeSet<u64>,
    open_limit: usize,
) -> Vec<u64> {
    if open_limit > 0 && open_numbers.len() >= open_limit {
        return Vec::new();
    }
    let mut out: Vec<u64> = trains
        .iter()
        .filter_map(|t| {
            let pr = t.combined_pr()?;
            (!open_numbers.contains(&pr)
                && !merged.contains(&pr)
                && t.status != TrainStatus::Landed
                && t.status != TrainStatus::Abandoned)
                .then_some(pr)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Every member of a train that is `Landed` or whose combined train PR is in `merged` (`R-AL-4`).
pub fn landed_members(trains: &[Train], merged: &BTreeSet<u64>) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for t in trains {
        let is_landed = match t.kind {
            TrainKind::Combined { pr, .. } => {
                t.status == TrainStatus::Landed || merged.contains(&pr)
            }
            TrainKind::Sequential => t.status == TrainStatus::Landed,
        };
        if is_landed {
            out.extend(t.members.iter().map(|m| m.pr));
        }
    }
    out
}

/// Member PR numbers currently carried by an open/landing sequential train or by a combined
/// train PR that has neither landed nor been abandoned (`R-AL-3`, `task_queue.in_flight_members`).
pub fn in_flight_members(trains: &[Train]) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for t in trains {
        let active = match t.kind {
            TrainKind::Sequential => t.status == TrainStatus::Open || t.status.is_landing(),
            TrainKind::Combined { .. } => {
                t.status != TrainStatus::Landed && t.status != TrainStatus::Abandoned
            }
        };
        if active {
            out.extend(t.members.iter().map(|m| m.pr));
        }
    }
    out
}

/// A retired rebase task entry recorded in task history (`R-AL-4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredRebase {
    pub key: String,
    pub pr: u64,
    pub why: String,
}

/// Removes queued `rebase-<n>` tasks and `hands[n]` for `members` (`R-AL-4`), returning
/// `(remaining_todo, remaining_hands, retired_records)` where each retired rebase records
/// `"rebase for #n: PR merged"`.
pub fn retire_rebases(
    todo: &[LandTask],
    hands: &BTreeMap<u64, Hands>,
    members: &BTreeSet<u64>,
) -> (Vec<LandTask>, BTreeMap<u64, Hands>, Vec<RetiredRebase>) {
    let mut kept_todo = Vec::new();
    let mut retired = Vec::new();
    for item in todo {
        let target_pr = members.iter().copied().find(|&n| {
            item.key == format!("rebase-{n}") || (item.kind == "rebase" && item.pr == n)
        });
        if let Some(n) = target_pr {
            let kind = if item.kind.is_empty() {
                "rebase"
            } else {
                item.kind.as_str()
            };
            retired.push(RetiredRebase {
                key: item.key.clone(),
                pr: n,
                why: format!("{kind} for #{n}: PR merged"),
            });
        } else {
            kept_todo.push(item.clone());
        }
    }
    let kept_hands: BTreeMap<u64, Hands> = hands
        .iter()
        .filter(|(n, _)| !members.contains(n))
        .map(|(&n, h)| (n, h.clone()))
        .collect();
    (kept_todo, kept_hands, retired)
}

/// Where a PR stands for the area work gate (`R-AL-5`, `plan.pr_standing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Standing {
    Merged,
    Closed,
    Passed,
    Waiting,
}

impl Standing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Passed => "passed",
            Self::Waiting => "waiting",
        }
    }
}

impl fmt::Display for Standing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pure standing classification (`plan.pr_standing`, `R-AL-5`).
pub fn standing(head: &str, passed_sha: Option<&str>, merged: bool, closed: bool) -> Standing {
    if merged {
        Standing::Merged
    } else if closed {
        Standing::Closed
    } else if passed_sha == Some(head) {
        Standing::Passed
    } else {
        Standing::Waiting
    }
}

/// Resolves a PR's [`Standing`] from code-host and git observations (`tick.standings`, `R-AL-5`):
/// - If `same_change_as_passed` holds (its patch-id against main matches the passed sha's),
///   its effective head is treated as `passed_sha`.
/// - A closed PR counts as `Merged` when it has a `"Landed in #N"` comment or `change_on_main`
///   holds; otherwise `Closed` (never `Waiting`).
#[allow(clippy::too_many_arguments)]
pub fn resolve_standing<S: AsRef<str>>(
    head: &str,
    passed_sha: Option<&str>,
    same_change_as_passed: bool,
    in_merged_prs: bool,
    is_ancestor_of_main: bool,
    closed: bool,
    comments: &[S],
    change_on_main: bool,
) -> Standing {
    let effective_head = match passed_sha {
        Some(ps) if ps != head && same_change_as_passed => ps,
        _ => head,
    };
    let landed = in_merged_prs
        || is_ancestor_of_main
        || (closed && (landed_in(comments).is_some() || change_on_main));
    standing(effective_head, passed_sha, landed, closed)
}

/// News line when a combined train PR lands (`R-AL-1`).
pub fn train_landed_news(train_pr: u64, members: &[u64]) -> String {
    let list = members
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("#{train_pr} landed; {list} closed as landed in it")
}

/// News line when a combined train PR is closed without merging (`R-AL-3`).
pub fn train_abandoned_news(train_pr: u64) -> String {
    format!("#{train_pr} was closed without merging; its members are free for another train")
}

/// News line when main moves outside our own landing (`R-AL-6`).
pub fn main_moved_news(new_main: &str, merged_since: Option<&[u64]>) -> String {
    let suffix = match merged_since.filter(|m| !m.is_empty()) {
        Some(ms) => {
            let list = ms
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!(" with {list}")
        }
        None => String::new(),
    };
    format!("main moved to {}{suffix}", short(new_main, 8))
}
