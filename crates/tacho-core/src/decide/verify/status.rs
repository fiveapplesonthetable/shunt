//! GitHub commit status decisions (R29): pending, passed, red, undecided, crashed,
//! and branch-protection setup advice.

use super::config::VerifyConfig;
use super::verdict::Outcome;
use serde::{Deserialize, Serialize};

/// GitHub commit status state (`pending`, `success`, `failure`, `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    Pending,
    Success,
    Failure,
    Error,
}

impl StatusState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StatusState::Pending => "pending",
            StatusState::Success => "success",
            StatusState::Failure => "failure",
            StatusState::Error => "error",
        }
    }
}

/// A pure commit-status post decision for a PR head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitStatusPost {
    pub pr: u64,
    pub sha: String,
    pub context: String,
    pub state: StatusState,
    pub description: String,
    pub reason: String,
}

fn truncate_desc(desc: &str, max_len: usize) -> String {
    let limit = max_len.max(1);
    if desc.chars().count() <= limit {
        desc.to_string()
    } else {
        desc.chars().take(limit).collect()
    }
}

/// R29: outcomes that leave a PR's merge-check undecided rather than blaming it.
pub fn is_undecided_outcome(outcome: Outcome) -> bool {
    matches!(
        outcome,
        Outcome::MainBroken
            | Outcome::Infrastructure
            | Outcome::NothingMerged
            | Outcome::Counterexample
            | Outcome::RedOnce
            | Outcome::GaveUp
    )
}

/// R29: pending commit status posted for each member when a batch starts.
pub fn pending_status(
    pr: u64,
    sha: &str,
    batch_id: &str,
    members: &[u64],
    cfg: &VerifyConfig,
) -> Option<CommitStatusPost> {
    if sha.is_empty() {
        return None;
    }
    let desc = truncate_desc(
        &format!("running in {batch_id} ({} PR(s))", members.len()),
        cfg.status_max_len,
    );
    Some(CommitStatusPost {
        pr,
        sha: sha.to_string(),
        context: cfg.status_context.clone(),
        state: StatusState::Pending,
        description: desc,
        reason: format!("batch {batch_id} started with PR #{pr} at {sha}"),
    })
}

/// R29: error commit status posted when the batch process crashes without writing a verdict.
pub fn crashed_status(
    pr: u64,
    sha: &str,
    batch_id: &str,
    rc: i32,
    cfg: &VerifyConfig,
) -> Option<CommitStatusPost> {
    if sha.is_empty() {
        return None;
    }
    let desc = truncate_desc(
        &format!("{batch_id} crashed (rc={rc}) — will retry"),
        cfg.status_max_len,
    );
    Some(CommitStatusPost {
        pr,
        sha: sha.to_string(),
        context: cfg.status_context.clone(),
        state: StatusState::Error,
        description: desc,
        reason: format!("batch {batch_id} exited with rc={rc} before recording a verdict"),
    })
}

/// R29: commit status for one batch member after a batch finishes with `outcome`.
///
/// - If `sha` is empty: returns `None` (no status posted).
/// - If `passed.contains(&pr)`: `Success` (`"passed in {batch_id}"`).
/// - Else if `blamed.contains(&pr)`: `Failure` (`"failed in {batch_id}: {summary}"`, or
///   `"failed in {batch_id}"` when summary is empty).
/// - Else if `skipped.contains(&pr)`: `Error` (`"skipped in {batch_id} (conflict or fetch) — will retry"`).
/// - Else if `outcome` is undecided (`MainBroken`, `Infrastructure`, `NothingMerged`,
///   `Counterexample`, `RedOnce`, `GaveUp`): `Error` (`"{batch_id}: {outcome} — will retry"`).
/// - Else: `Error` (`"not proven in {batch_id} ({outcome}) — will retry"`).
#[allow(clippy::too_many_arguments)]
pub fn member_verdict_status(
    pr: u64,
    sha: &str,
    batch_id: &str,
    outcome: Outcome,
    passed: &[u64],
    blamed: &[u64],
    skipped: &[u64],
    blame_summary: Option<&str>,
    cfg: &VerifyConfig,
) -> Option<CommitStatusPost> {
    if sha.is_empty() {
        return None;
    }
    let (state, raw_desc, reason) = if passed.contains(&pr) {
        (
            StatusState::Success,
            format!("passed in {batch_id}"),
            format!("PR #{pr} passed verification in {batch_id}"),
        )
    } else if blamed.contains(&pr) {
        let sum = blame_summary.unwrap_or("").trim();
        let d = if sum.is_empty() {
            format!("failed in {batch_id}")
        } else {
            format!("failed in {batch_id}: {sum}")
        };
        (
            StatusState::Failure,
            d,
            format!("PR #{pr} was blamed for failure in {batch_id}"),
        )
    } else if skipped.contains(&pr) {
        (
            StatusState::Error,
            format!("skipped in {batch_id} (conflict or fetch) — will retry"),
            format!("PR #{pr} was skipped in {batch_id}"),
        )
    } else if is_undecided_outcome(outcome) {
        let out_str = outcome.as_str();
        (
            StatusState::Error,
            format!("{batch_id}: {out_str} — will retry"),
            format!("batch {batch_id} ended undecided ({out_str}) for PR #{pr}"),
        )
    } else {
        let out_str = outcome.as_str();
        (
            StatusState::Error,
            format!("not proven in {batch_id} ({out_str}) — will retry"),
            format!("PR #{pr} was not proven in {batch_id} ({out_str})"),
        )
    };
    Some(CommitStatusPost {
        pr,
        sha: sha.to_string(),
        context: cfg.status_context.clone(),
        state,
        description: truncate_desc(&raw_desc, cfg.status_max_len),
        reason,
    })
}

/// R29: compute all member commit statuses for a completed batch verdict.
#[allow(clippy::too_many_arguments)]
pub fn batch_verdict_statuses(
    batch_id: &str,
    prs: &[u64],
    heads: &std::collections::BTreeMap<u64, String>,
    outcome: Outcome,
    passed: &[u64],
    blamed: &[u64],
    skipped: &[u64],
    blame_summary: Option<&str>,
    cfg: &VerifyConfig,
) -> Vec<CommitStatusPost> {
    let mut out = Vec::new();
    for &pr in prs {
        let sha = heads.get(&pr).map(String::as_str).unwrap_or("");
        if let Some(post) = member_verdict_status(
            pr,
            sha,
            batch_id,
            outcome,
            passed,
            blamed,
            skipped,
            blame_summary,
            cfg,
        ) {
            out.push(post);
        }
    }
    out
}

/// R29: advise whether branch protection requires `cfg.status_context` (`merge-check`).
pub fn status_setup_advice(required_contexts: &[String], cfg: &VerifyConfig) -> (bool, String) {
    let present = required_contexts.iter().any(|c| c == &cfg.status_context);
    if present {
        (
            true,
            format!(
                "required status check '{}' is configured on branch protection",
                cfg.status_context
            ),
        )
    } else {
        (
            false,
            format!(
                "required status check '{}' is NOT in branch protection ({:?}); add it via gh api",
                cfg.status_context, required_contexts
            ),
        )
    }
}
