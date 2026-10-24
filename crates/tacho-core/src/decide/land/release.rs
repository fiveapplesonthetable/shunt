//! Combined train release gate, required areas, two-phase mail state machine, owner letter,
//! and status row (`R-ML-1`..`R-ML-9`).

use super::assemble::{title, CI_NOTICE};
use super::config::LandConfig;
use super::ready::Passed;
use super::train::{MailState, Sha, Train};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The areas whose screenshots need redrawing on a tree that touched `files` (`R-ML-2`).
pub fn redraw_areas<S: AsRef<str>>(files: &[S], cfg: &LandConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (area, prefix) in &cfg.redraw_roots {
        if files
            .iter()
            .any(|f| f.as_ref().starts_with(prefix.as_str()))
        {
            out.insert(area.clone());
        }
    }
    out
}

/// Checks needed before readiness (`plan.check_areas`, `R-ML-2`):
/// - Removes `skipped` and `cfg.post_merge_areas` (e.g. `formal`).
/// - On a train (`train = true`), always adds `cfg.gate_area`.
/// - On a single change (`train = false`), adds `cfg.pixel_areas - skipped` when any file matches
///   `cfg.pixel_files`, and adds any `(regex, area)` from `cfg.change_file_areas` when not in `skipped`.
/// - Drops any area in `train_inputs` (for `train = true`) or `area_inputs` (for `train = false`)
///   whose regex matches no file in `files`. Invalid regexes fail closed (never drop the area).
pub fn check_areas<S: AsRef<str>>(
    selected: &BTreeSet<String>,
    files: &[S],
    skipped: &BTreeSet<String>,
    train: bool,
    cfg: &LandConfig,
) -> BTreeSet<String> {
    let mut required: BTreeSet<String> = selected
        .iter()
        .filter(|a| !skipped.contains(*a) && !cfg.post_merge_areas.contains(*a))
        .cloned()
        .collect();

    if train && !cfg.gate_area.is_empty() {
        required.insert(cfg.gate_area.clone());
    }

    if !train {
        let touches_pixels = files.iter().any(|f| {
            cfg.pixel_files
                .iter()
                .any(|p| Regex::new(p).map_or(true, |re| re.is_match(f.as_ref())))
        });
        if touches_pixels {
            for area in &cfg.pixel_areas {
                if !skipped.contains(area) {
                    required.insert(area.clone());
                }
            }
        }
        for (pattern, area) in &cfg.change_file_areas {
            if !skipped.contains(area)
                && files
                    .iter()
                    .any(|f| Regex::new(pattern).map_or(true, |re| re.is_match(f.as_ref())))
            {
                required.insert(area.clone());
            }
        }
    }

    let inputs = if train {
        &cfg.train_inputs
    } else {
        &cfg.area_inputs
    };
    for (area, pattern) in inputs {
        if let Ok(re) = Regex::new(pattern) {
            if !files.iter().any(|f| re.is_match(f.as_ref())) {
                required.remove(area);
            }
        }
    }

    required
}

/// Areas that must have passed on a combined train's merged tree before owner mail (`R-ML-2`):
/// `check_areas(selected, files, ∅, train=true)` ∪ `redraw_areas(files)`.
/// Note: configured `skip_areas` are intentionally NOT subtracted here so a skipped required area
/// still blocks release (`R-ML-3.7`).
pub fn required_areas<S: AsRef<str>>(
    files: &[S],
    selected: &BTreeSet<String>,
    cfg: &LandConfig,
) -> BTreeSet<String> {
    let empty = BTreeSet::new();
    let mut req = check_areas(selected, files, &empty, true, cfg);
    req.extend(redraw_areas(files, cfg));
    req
}

/// Batch evidence for a combined train PR (`R-ML-3`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProvedBatch {
    /// `"green"`, `"red"`, `"running"`, `"partial"`, etc.
    pub status: String,
    /// Member PRs skipped by the batch.
    #[serde(default)]
    pub skipped: Vec<u64>,
    /// Named skips or exclusions on the batch.
    #[serde(default)]
    pub skips: Vec<String>,
    /// Flaky tests recorded during the batch.
    #[serde(default)]
    pub flaky: Vec<String>,
    /// The git tree SHA the batch proved.
    #[serde(default)]
    pub proved_tree: Option<Sha>,
    /// `area -> run_id` that proved the area on `proved_tree`. An area without a non-empty run_id
    /// does not count as proved.
    #[serde(default)]
    pub proved_runs: BTreeMap<String, String>,
}

/// First reason a combined train PR cannot be mailed yet (`R-ML-3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Waiting {
    AlreadyMailed,
    NoPassAtHead,
    BatchNotGreen { status: String },
    BatchSkipped,
    BatchFlaky,
    TreeMoved,
    MissingAreas { areas: Vec<String> },
    WaitingForReview { tool: String },
}

impl fmt::Display for Waiting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyMailed => f.write_str("already mailed"),
            Self::NoPassAtHead => f.write_str("its current head has not passed a batch"),
            Self::BatchNotGreen { status } => write!(f, "its batch is {status}, not green"),
            Self::BatchSkipped => f.write_str("its batch skipped or excluded members"),
            Self::BatchFlaky => f.write_str("its batch recorded flaky tests"),
            Self::TreeMoved => {
                f.write_str("its batch proved a different tree than main would now have")
            }
            Self::MissingAreas { areas } => {
                write!(f, "no pass on this tree for {}", areas.join(", "))
            }
            Self::WaitingForReview { tool } => {
                write!(f, "waiting for review: `{tool} release-train <pr>`")
            }
        }
    }
}

/// Checks in strict order (`R-ML-3`) whether `train` is ready to mail.
/// Returns `None` when every gate passes, or `Some(first_reason)` when blocked.
pub fn waiting_for(
    train: &Train,
    passed: Option<&Passed>,
    batch: Option<&ProvedBatch>,
    head: &str,
    tree_now: &str,
    required: &BTreeSet<String>,
    cfg: &LandConfig,
) -> Option<Waiting> {
    if matches!(
        train.mail,
        MailState::Sent { .. } | MailState::Intent { .. }
    ) {
        return Some(Waiting::AlreadyMailed);
    }
    let Some(passed) = passed.filter(|p| p.sha == head) else {
        return Some(Waiting::NoPassAtHead);
    };
    let _ = passed;
    let status = batch
        .map(|b| b.status.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("missing");
    if status != "green" {
        return Some(Waiting::BatchNotGreen {
            status: status.to_string(),
        });
    }
    let b = batch.expect("status == green implies batch is Some");
    if !b.skipped.is_empty() || !b.skips.is_empty() {
        return Some(Waiting::BatchSkipped);
    }
    if !b.flaky.is_empty() {
        return Some(Waiting::BatchFlaky);
    }
    if b.proved_tree.as_deref() != Some(tree_now) {
        return Some(Waiting::TreeMoved);
    }
    let missing: Vec<String> = required
        .iter()
        .filter(|area| b.proved_runs.get(*area).is_none_or(|run| run.is_empty()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Some(Waiting::MissingAreas { areas: missing });
    }
    if train.reviewed.as_deref() != Some(head) {
        return Some(Waiting::WaitingForReview {
            tool: cfg.tool.clone(),
        });
    }
    None
}

/// The single pinned merge command for a reviewed combined train PR (`R-ML-6`).
pub fn merge_command(pr: u64, upstream: &str, head: &str) -> String {
    format!("gh pr merge {pr} -R {upstream} --merge --match-head-commit {head}")
}

/// Subject and body of the owner mail carrying the one merge command (`R-ML-6`).
#[allow(clippy::too_many_arguments)]
pub fn letter(
    pr: u64,
    members: &[u64],
    left_out: &[u64],
    dropped: &[u64],
    ci: bool,
    upstream: &str,
    project: &str,
    head: &str,
) -> (String, String) {
    let subject = format!("{project}: merge #{pr}, {} changes", members.len());
    let mut omitted: Vec<String> = left_out
        .iter()
        .chain(dropped.iter())
        .map(|n| format!("#{n}"))
        .collect();
    omitted.dedup();
    let note = if omitted.is_empty() {
        String::new()
    } else {
        format!("\nLeft out: {}.\n", omitted.join(", "))
    };
    let mut body = format!(
        "{}, and the result compiles. Merge it with:\n\n{}\n{}\nThe merge check passed on exactly this tree and it has been reviewed.\n",
        title(members.len()),
        merge_command(pr, upstream, head),
        note,
    );
    if ci {
        body.push_str(&format!("\n{CI_NOTICE}\n"));
    }
    (subject, body)
}

/// Mail gate notice shown in `status` for a combined train PR (`R-ML-7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    MailOutcomeUncertain,
    Sent,
    ReleaseRequested,
    ReviewPending,
}

impl Notice {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MailOutcomeUncertain => "mail outcome uncertain",
            Self::Sent => "sent",
            Self::ReleaseRequested => "release requested",
            Self::ReviewPending => "review pending",
        }
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Summary of a batch run for computing [`status_row`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainBatchSummary {
    pub members: Vec<u64>,
    pub started: i64,
    pub status: String,
}

/// What `status` shows for a combined train PR (`R-ML-7`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRow {
    pub pr: u64,
    pub members: Vec<u64>,
    pub left_out: Vec<u64>,
    pub dropped: Vec<u64>,
    pub compiled: bool,
    pub verdict: String,
    pub notice: Notice,
}

/// Computes the [`StatusRow`] of a combined train PR (`R-ML-7`).
pub fn status_row(pr: u64, train: &Train, batches: &[TrainBatchSummary]) -> StatusRow {
    let verdict = batches
        .iter()
        .filter(|b| b.members.as_slice() == [pr])
        .max_by_key(|b| b.started)
        .map_or_else(|| "not started".to_string(), |b| b.status.clone());
    let notice = match &train.mail {
        MailState::Intent { .. } => Notice::MailOutcomeUncertain,
        MailState::Sent { .. } => Notice::Sent,
        MailState::NotSent | MailState::Failed { .. } => {
            if train.reviewed.as_deref().is_some_and(|r| !r.is_empty()) {
                Notice::ReleaseRequested
            } else {
                Notice::ReviewPending
            }
        }
    };
    StatusRow {
        pr,
        members: train.numbers(),
        left_out: train.left_out.iter().map(|(n, _)| *n).collect(),
        dropped: train.dropped.iter().map(|(n, _)| *n).collect(),
        compiled: train.compiled,
        verdict,
        notice,
    }
}
