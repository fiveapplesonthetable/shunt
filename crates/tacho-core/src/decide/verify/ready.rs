//! Ready-queue re-check (R45) and PR standing status classification (R46).

use super::areas::{required_train_areas, select_areas, Tier};
use super::config::VerifyConfig;
use super::grouping::covered;
use super::plan::ordered;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// R45: recorded pass entry for a PR in `state.passed[pr]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReadyPass {
    pub sha: String,
    pub areas: BTreeSet<String>,
    #[serde(default)]
    pub batch_id: String,
    #[serde(default)]
    pub main_sha: String,
    #[serde(default)]
    pub tree: Option<String>,
    #[serde(default)]
    pub proved_by: BTreeMap<String, String>,
}

/// R45: live observation of a PR for ready-queue checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReadyPrObs {
    pub pr: u64,
    pub open: bool,
    pub draft: bool,
    pub head: String,
    /// Required Areas for this PR (when precomputed by caller; if `files` is non-empty,
    /// `pr_still_ready` also checks `required_train_areas` against `files`).
    pub areas: BTreeSet<String>,
    #[serde(default)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    pub parent: Option<u64>,
    #[serde(default)]
    pub held: bool,
    #[serde(default)]
    pub blocked: bool,
}

/// R45: check whether a PR is still ready to land right now.
///
/// Fails closed (`false`) if:
/// - the PR is not open, is a draft, has an empty head, is held, or is blocked;
/// - there is no pass record, or `pass.sha != obs.head`;
/// - `obs.files` is `Some(empty)` (diff failed to fetch or returned no files when files were expected);
/// - the pass's `areas` do not cover every required train area for the PR (`covered`);
/// - if `proved_by` is non-empty, any required area lacks an entry in `proved_by`.
pub fn pr_still_ready(
    obs: &ReadyPrObs,
    pass: Option<&ReadyPass>,
    skipped_areas: &BTreeSet<String>,
    cfg: &VerifyConfig,
) -> (bool, String) {
    if !obs.open {
        return (false, format!("PR #{} is no longer open", obs.pr));
    }
    if obs.draft {
        return (false, format!("PR #{} is a draft", obs.pr));
    }
    if obs.head.is_empty() {
        return (false, format!("PR #{} has an empty head SHA", obs.pr));
    }
    if obs.held {
        return (false, format!("PR #{} has an active hold", obs.pr));
    }
    if obs.blocked {
        return (false, format!("PR #{} is blocked", obs.pr));
    }
    let Some(p) = pass else {
        return (false, format!("PR #{} has no pass record", obs.pr));
    };
    if p.sha != obs.head {
        return (
            false,
            format!(
                "PR #{} head moved from passed {} to {}",
                obs.pr, p.sha, obs.head
            ),
        );
    }
    let req_areas = if let Some(files) = &obs.files {
        if files.is_empty() {
            return (
                false,
                format!(
                    "PR #{} file list is empty; refusing to land without diff",
                    obs.pr
                ),
            );
        }
        let sel = select_areas(files, Tier::Train, &cfg.areas);
        required_train_areas(files, &sel.areas, skipped_areas, &cfg.areas)
    } else {
        obs.areas.clone()
    };
    if !covered(&req_areas, &p.areas, skipped_areas) {
        return (
            false,
            format!(
                "PR #{} pass areas {:?} do not cover required {:?}",
                obs.pr, p.areas, req_areas
            ),
        );
    }
    if !p.proved_by.is_empty() {
        for a in req_areas.difference(skipped_areas) {
            if !p.proved_by.contains_key(a) {
                return (
                    false,
                    format!(
                        "PR #{} pass lacks structured proof for area '{}'",
                        obs.pr, a
                    ),
                );
            }
        }
    }
    (
        true,
        format!(
            "PR #{} is ready at {} (passed in {})",
            obs.pr, obs.head, p.batch_id
        ),
    )
}

/// R45: order ready PRs parent-before-child (`ordered`), filtering out any PR whose stack parent
/// is an open PR that is not itself ready in the queue.
pub fn ready_queue_order(
    candidates: &[ReadyPrObs],
    passes: &BTreeMap<u64, ReadyPass>,
    open_prs: &BTreeSet<u64>,
    skipped_areas: &BTreeSet<String>,
    cfg: &VerifyConfig,
) -> Vec<u64> {
    let mut ready_set = BTreeSet::new();
    let mut up = BTreeMap::new();
    for c in candidates {
        if let Some(p) = c.parent {
            up.insert(c.pr, p);
        }
        let (ok, _) = pr_still_ready(c, passes.get(&c.pr), skipped_areas, cfg);
        if ok {
            ready_set.insert(c.pr);
        }
    }
    // Filter out children whose parent is an open PR not in ready_set
    loop {
        let mut dropped = false;
        let current: Vec<u64> = ready_set.iter().copied().collect();
        for pr in current {
            if let Some(&parent) = up.get(&pr) {
                if open_prs.contains(&parent) && !ready_set.contains(&parent) {
                    ready_set.remove(&pr);
                    dropped = true;
                }
            }
        }
        if !dropped {
            break;
        }
    }
    let nums: Vec<u64> = ready_set.into_iter().collect();
    ordered(&nums, &up)
}

/// R45: whether new PRs became ready while the current queue was landing.
pub fn ready_queue_grew(
    before: &[u64],
    now_ready: &[u64],
    landed_or_dropped: &BTreeSet<u64>,
) -> bool {
    let known: BTreeSet<u64> = before
        .iter()
        .copied()
        .chain(landed_or_dropped.iter().copied())
        .collect();
    now_ready.iter().any(|p| !known.contains(p))
}

/// R45: construct the landing CLI command (`<tool_name> land <pr> [--expect-head <sha>]`).
pub fn land_cmd(tool_name: &str, pr: u64, expect_head: Option<&str>) -> Vec<String> {
    let mut cmd = vec![tool_name.to_string(), "land".to_string(), pr.to_string()];
    if let Some(sha) = expect_head {
        if !sha.is_empty() {
            cmd.push("--expect-head".to_string());
            cmd.push(sha.to_string());
        }
    }
    cmd
}

/// R46: standing of an open PR for `status` reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PrStanding {
    Held { reason: String },
    Blocked { reason: String },
    WaitingParent { parent: u64 },
    Running { batch_id: String },
    Ready { batch_id: String },
    Failed { batch_id: String, summary: String },
    Untied { batch_id: String, with: Vec<u64> },
    Queued { group: String, areas: Vec<String> },
}

impl PrStanding {
    pub fn label(&self) -> String {
        match self {
            PrStanding::Held { reason } => {
                if reason.is_empty() {
                    "held".to_string()
                } else {
                    format!("held ({reason})")
                }
            }
            PrStanding::Blocked { reason } => {
                if reason.is_empty() {
                    "blocked".to_string()
                } else {
                    format!("blocked ({reason})")
                }
            }
            PrStanding::WaitingParent { parent } => format!("waiting on parent #{parent}"),
            PrStanding::Running { batch_id } => format!("running in {batch_id}"),
            PrStanding::Ready { batch_id } => format!("passed in {batch_id} (ready to land)"),
            PrStanding::Failed { batch_id, summary } => {
                if summary.is_empty() {
                    format!("failed in {batch_id}")
                } else {
                    format!("failed in {batch_id}: {summary}")
                }
            }
            PrStanding::Untied { batch_id, with } => {
                format!("untied in {batch_id} with {with:?}")
            }
            PrStanding::Queued { group, areas } => {
                format!("queued [{group}] ({})", areas.join(", "))
            }
        }
    }
}

/// R46: classify the standing of a single open PR from live state.
#[allow(clippy::too_many_arguments)]
pub fn pr_standing(
    obs: &ReadyPrObs,
    hold_reason: Option<&str>,
    block_reason: Option<&str>,
    running_batch: Option<&str>,
    pass: Option<&ReadyPass>,
    failed_at_head: Option<(&str, &str)>,
    untied_at_head: Option<(&str, &[u64])>,
    open_prs: &BTreeSet<u64>,
    parent_ready: bool,
    skipped_areas: &BTreeSet<String>,
    cfg: &VerifyConfig,
) -> PrStanding {
    if obs.held || hold_reason.is_some() {
        return PrStanding::Held {
            reason: hold_reason.unwrap_or("").to_string(),
        };
    }
    if obs.blocked || block_reason.is_some() {
        return PrStanding::Blocked {
            reason: block_reason.unwrap_or("").to_string(),
        };
    }
    if let Some(b) = running_batch {
        return PrStanding::Running {
            batch_id: b.to_string(),
        };
    }
    if let Some((batch_id, summary)) = failed_at_head {
        return PrStanding::Failed {
            batch_id: batch_id.to_string(),
            summary: summary.to_string(),
        };
    }
    if let Some((batch_id, with)) = untied_at_head {
        return PrStanding::Untied {
            batch_id: batch_id.to_string(),
            with: with.to_vec(),
        };
    }
    let (ready, _) = pr_still_ready(obs, pass, skipped_areas, cfg);
    if ready {
        if let Some(parent) = obs.parent {
            if open_prs.contains(&parent) && !parent_ready {
                return PrStanding::WaitingParent { parent };
            }
        }
        return PrStanding::Ready {
            batch_id: pass.map(|p| p.batch_id.clone()).unwrap_or_default(),
        };
    }
    if let Some(parent) = obs.parent {
        if open_prs.contains(&parent) && !parent_ready {
            return PrStanding::WaitingParent { parent };
        }
    }
    let group = super::grouping::group_of(&obs.areas, cfg);
    PrStanding::Queued {
        group,
        areas: obs.areas.iter().cloned().collect(),
    }
}
