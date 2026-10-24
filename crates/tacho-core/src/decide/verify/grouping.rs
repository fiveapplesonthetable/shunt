//! Batch planning by group (R1–R11): stack parents by ancestry, grouping by the
//! union of a stack's areas, one batch per group, pipeline PRs alone, and the
//! "checked once over every area it touches" rule.

use super::config::{GroupMatch, VerifyConfig};
use super::plan::{cap, ordered, root};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// An open PR head waiting for a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Waiting {
    pub pr: u64,
    pub sha: String,
    pub areas: BTreeSet<String>,
    /// True when the PR changes the pipeline itself ([`touches_ci`]).
    pub ci: bool,
}

/// A batch [`plan_batches`] wants started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedBatch {
    pub group: String,
    pub members: Vec<u64>,
    pub areas: Vec<String>,
    pub reason: String,
}

/// R1: each PR's parent is the nearest other PR (different number and sha) whose head is an
/// ancestor of its head. "Nearest" is the candidate the most other candidates lie below; ties
/// go to the lower PR number so the answer never depends on input order.
pub fn stack_parents(
    prs: &[(u64, String)],
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> BTreeMap<u64, u64> {
    let mut out = BTreeMap::new();
    for (child, child_sha) in prs {
        let below: Vec<&(u64, String)> = prs
            .iter()
            .filter(|(n, s)| n != child && s != child_sha && is_ancestor(s, child_sha))
            .collect();
        let nearest = below
            .iter()
            .map(|(n, s)| {
                let depth = below
                    .iter()
                    .filter(|(q, qs)| q != n && is_ancestor(qs, s))
                    .count();
                (depth, std::cmp::Reverse(*n))
            })
            .max();
        if let Some((_, std::cmp::Reverse(n))) = nearest {
            out.insert(*child, n);
        }
    }
    out
}

/// R4: the group of a stack touching `areas`, from the configured table.
pub fn group_of(areas: &BTreeSet<String>, cfg: &VerifyConfig) -> String {
    let rest: BTreeSet<&String> = areas.difference(&cfg.group_ignore).collect();
    for rule in &cfg.groups {
        let hit = match &rule.when {
            GroupMatch::SupersetOf(want) => want.iter().all(|a| rest.contains(a)),
            GroupMatch::SubsetOf(allowed) => rest.iter().all(|a| allowed.contains(*a)),
            GroupMatch::NonEmptySubsetOf(allowed) => {
                !rest.is_empty() && rest.iter().all(|a| allowed.contains(*a))
            }
        };
        if hit {
            return rule.name.clone();
        }
    }
    cfg.fallback_group.clone()
}

/// R5: whether `files` change the pipeline itself. An invalid regex counts as a match
/// (fail closed: the PR is isolated rather than mixed into a batch).
pub fn touches_ci(files: &[String], cfg: &VerifyConfig) -> bool {
    cfg.ci_files.iter().any(|p| match Regex::new(p) {
        Ok(re) => files.iter().any(|f| re.is_match(f)),
        Err(_) => !files.is_empty(),
    })
}

/// R10: a pass counts only if it checked every touched area not left to main's CI.
pub fn covered(
    touched: &BTreeSet<String>,
    checked: &BTreeSet<String>,
    skipped: &BTreeSet<String>,
) -> bool {
    touched.difference(skipped).all(|a| checked.contains(a))
}

/// R9: a head is checked once over every area it touches, unless it failed and has not moved.
/// `passed` is `(sha, checked areas)` of the PR's last pass.
pub fn needs_check_at(
    sha: &str,
    areas: &BTreeSet<String>,
    passed: Option<(&str, &BTreeSet<String>)>,
    failed_sha: Option<&str>,
    skipped: &BTreeSet<String>,
) -> bool {
    if let Some((psha, checked)) = passed {
        if psha == sha && covered(areas, checked, skipped) {
            return false;
        }
    }
    failed_sha != Some(sha)
}

/// R11: `<group>-<now>`, or the first `-k` suffix not taken.
pub fn batch_id_for(group: &str, now: i64, taken: &BTreeSet<String>) -> String {
    let base = format!("{group}-{now}");
    if !taken.contains(&base) {
        return base;
    }
    (1..)
        .map(|k| format!("{base}-{k}"))
        .find(|id| !taken.contains(id))
        .unwrap_or(base)
}

/// A batch record tracked in `state.batches` for R12 atomic claim/deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedBatch {
    pub batch_id: String,
    pub group: String,
    pub prs: Vec<u64>,
    pub areas: Vec<String>,
    pub status: String,
    pub pid: Option<u32>,
    pub started: i64,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub finished: Option<i64>,
}

/// Result of [`claim_batch`] (R12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimDecision {
    pub batch_id: String,
    pub attached_existing: bool,
    pub reason: String,
}

/// R12: atomically claim a batch or attach to an already-running batch with the exact same PR set
/// (`BTreeSet<u64>`) whose PID is still alive.
#[allow(clippy::too_many_arguments)]
pub fn claim_batch(
    batches: &mut BTreeMap<String, ClaimedBatch>,
    group: &str,
    prs: &[u64],
    areas: &[String],
    now: i64,
    caller_pid: u32,
    pid_alive: impl Fn(u32) -> bool,
) -> ClaimDecision {
    let want: BTreeSet<u64> = prs.iter().copied().collect();
    for (bid, b) in batches.iter() {
        if b.status == "running" {
            let b_prs: BTreeSet<u64> = b.prs.iter().copied().collect();
            if b_prs == want && b.pid.is_some_and(&pid_alive) {
                return ClaimDecision {
                    batch_id: bid.clone(),
                    attached_existing: true,
                    reason: format!(
                        "attached to live running batch {bid} with identical PR set {:?}",
                        prs
                    ),
                };
            }
        }
    }
    let taken: BTreeSet<String> = batches.keys().cloned().collect();
    let bid = batch_id_for(group, now, &taken);
    batches.insert(
        bid.clone(),
        ClaimedBatch {
            batch_id: bid.clone(),
            group: group.to_string(),
            prs: prs.to_vec(),
            areas: areas.to_vec(),
            status: "running".to_string(),
            pid: Some(caller_pid),
            started: now,
            outcome: None,
            finished: None,
        },
    );
    ClaimDecision {
        batch_id: bid.clone(),
        attached_existing: false,
        reason: format!(
            "claimed new batch {bid} for group {group} with PRs {:?}",
            prs
        ),
    }
}

/// R12: mark a claimed batch as `done / lost` when spawning the subprocess fails.
pub fn mark_batch_lost(batches: &mut BTreeMap<String, ClaimedBatch>, batch_id: &str, now: i64) {
    if let Some(b) = batches.get_mut(batch_id) {
        b.status = "done".to_string();
        b.outcome = Some("lost".to_string());
        b.finished = Some(now);
    }
}

/// R6–R8: batches to start now. One per group not already running, oldest group first
/// (by minimum PR number), at most `slots`. A stack is grouped by the union of all its
/// waiting members' areas so a child rides with its parent. A pipeline batch takes only
/// its first member.
pub fn plan_batches(
    waiting: &[Waiting],
    up: &BTreeMap<u64, u64>,
    running: &BTreeSet<String>,
    slots: usize,
    cfg: &VerifyConfig,
) -> Vec<PlannedBatch> {
    let mut stack_areas: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for w in waiting {
        stack_areas
            .entry(root(w.pr, up))
            .or_default()
            .extend(w.areas.iter().cloned());
    }
    let mut by_group: BTreeMap<String, Vec<&Waiting>> = BTreeMap::new();
    for w in waiting {
        let g = if w.ci {
            cfg.ci_group.clone()
        } else {
            group_of(&stack_areas[&root(w.pr, up)], cfg)
        };
        by_group.entry(g).or_default().push(w);
    }
    let mut groups: Vec<(&String, &Vec<&Waiting>)> = by_group.iter().collect();
    groups.sort_by_key(|(g, ws)| (ws.iter().map(|w| w.pr).min(), (*g).clone()));
    let mut out = Vec::new();
    for (group, ws) in groups {
        if running.contains(group) || out.len() >= slots {
            continue;
        }
        let nums: Vec<u64> = ws.iter().map(|w| w.pr).collect();
        let mut members = cap(&ordered(&nums, up), up, cfg.batch_size.max(1));
        if *group == cfg.ci_group {
            members.truncate(1);
        }
        let areas: BTreeSet<String> = ws
            .iter()
            .filter(|w| members.contains(&w.pr))
            .flat_map(|w| w.areas.iter().cloned())
            .collect();
        let reason = format!(
            "group {group} is idle and has {} waiting; checking {:?} merged in order onto main",
            ws.len(),
            members
        );
        out.push(PlannedBatch {
            group: group.clone(),
            members,
            areas: areas.into_iter().collect(),
            reason,
        });
    }
    out
}
