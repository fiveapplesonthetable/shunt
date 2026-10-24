//! Pure types and helper decisions for batch verification (R17–R23):
//! [`Outcome`], [`Blame`], [`UntiedRecord`], [`PassedVerdict`], [`BatchVerdict`],
//! [`ReusableCandidate`], [`reusable_batch_id`], [`wall_clock_expired`],
//! [`can_rerun_on_red`], [`StaleShotsAction`], [`stale_shots_step`], [`Phase`],
//! [`BatchState`], [`VerifyCtx`], [`VerifyEvent`], and [`VerifyAction`].

use super::bisect::BisectProbe;
use super::config::VerifyConfig;
use super::result::{
    describe_skips, excerpt, failed_areas_of, has_counterexample, is_not_run, missing_proof,
    skip_rebases, CheckResult, Member, SkipReason,
};
use super::status::{batch_verdict_statuses, CommitStatusPost};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Every batch outcome, typed instead of stringly-typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Green,
    Partial,
    IncompleteProof,
    FlakyGreen,
    Red,
    RedOnce,
    Infrastructure,
    NothingMerged,
    MainBroken,
    Counterexample,
    WallClock,
    GaveUp,
    HeadMoved,
    Hang,
    Lost,
    ShotsUpdated,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Green => "green",
            Outcome::Partial => "partial",
            Outcome::IncompleteProof => "incomplete-proof",
            Outcome::FlakyGreen => "flaky-green",
            Outcome::Red => "red",
            Outcome::RedOnce => "red-once",
            Outcome::Infrastructure => "infrastructure",
            Outcome::NothingMerged => "nothing-merged",
            Outcome::MainBroken => "main-broken",
            Outcome::Counterexample => "counterexample",
            Outcome::WallClock => "wall-clock",
            Outcome::GaveUp => "gave-up",
            Outcome::HeadMoved => "head-moved",
            Outcome::Hang => "hang",
            Outcome::Lost => "lost",
            Outcome::ShotsUpdated => "shots-updated",
        }
    }

    pub fn skips_adjusted(self, skipped: &[u64]) -> Self {
        if self == Outcome::Green && !skipped.is_empty() {
            Outcome::Partial
        } else {
            self
        }
    }
}

/// Blame record for a culprit PR head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blame {
    pub pr: u64,
    pub sha: String,
    pub batch_id: String,
    pub summary: String,
    pub failed_areas: Vec<String>,
    pub log_path: String,
    pub log_excerpt: String,
    pub at: i64,
}

/// Record of an untied pair `[p0, p1]` when prefix `[p0]` passed and `[p0, p1]` failed on
/// infrastructure before blaming `p1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntiedRecord {
    pub prs: Vec<u64>,
    pub shas: BTreeMap<u64, String>,
    pub batch_id: String,
    pub summary: String,
    pub at: i64,
}

/// Passed verdict for one PR head (`state.passed[pr]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassedVerdict {
    pub sha: String,
    pub batch_id: String,
    pub main_sha: String,
    pub tree: Option<String>,
    pub areas: Vec<String>,
    /// Area -> proving run label (`batch_id`, `batch_id-retry`, etc.).
    pub proved_by: BTreeMap<String, String>,
    pub at: i64,
}

/// Terminal verdict of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchVerdict {
    pub outcome: Outcome,
    pub main_sha: Option<String>,
    pub passed: BTreeMap<u64, PassedVerdict>,
    pub blamed: Vec<Blame>,
    pub skipped: Vec<u64>,
    pub skips: BTreeMap<u64, SkipReason>,
    pub rebases: BTreeMap<u64, Vec<String>>,
    pub untied: Vec<UntiedRecord>,
    pub reason: String,
}

/// R17: a previously completed batch record eligible for reuse checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReusableCandidate {
    pub batch_id: String,
    pub status: String,
    pub outcome: Option<Outcome>,
    pub main_sha: Option<String>,
    pub heads: BTreeMap<u64, String>,
    pub passed: Vec<u64>,
    pub areas: Vec<String>,
}

/// R17: find an existing green batch whose `main_sha`, `heads`, `passed == prs`, and
/// `areas >= want_areas` prove the exact same merge result.
pub fn reusable_batch_id(
    candidates: &[ReusableCandidate],
    main_sha: &str,
    heads: &BTreeMap<u64, String>,
    prs: &[u64],
    want_areas: &[String],
) -> Option<String> {
    if main_sha.is_empty() || heads.is_empty() || heads.values().any(|s| s.is_empty()) {
        return None;
    }
    let want_set: BTreeSet<&String> = want_areas.iter().collect();
    for c in candidates {
        if c.status == "done"
            && c.outcome == Some(Outcome::Green)
            && c.main_sha.as_deref() == Some(main_sha)
            && &c.heads == heads
            && c.passed == prs
        {
            let b_areas: BTreeSet<&String> = c.areas.iter().collect();
            if want_set.is_subset(&b_areas) {
                return Some(c.batch_id.clone());
            }
        }
    }
    None
}

/// R18: whether the batch wall-clock budget has expired (`now - started >= limit`).
pub fn wall_clock_expired(started: i64, now: i64, limit: i64) -> bool {
    now.saturating_sub(started) >= limit
}

/// R20: whether a red run on `cand` is allowed to rerun to separate a flake from a real break.
pub fn can_rerun_on_red(
    cand: &[u64],
    first: &CheckResult,
    reruns_used: u32,
    cfg: &VerifyConfig,
) -> bool {
    if reruns_used >= cfg.reruns || cand.is_empty() || is_not_run(first, &cfg.log_patterns) {
        return false;
    }
    if has_counterexample(&first.log, &cfg.log_patterns) {
        return false;
    }
    let all_cfg: Vec<String> = cfg.areas.all_areas.iter().cloned().collect();
    let failed = failed_areas_of(first, &all_cfg);
    let all_chk: Vec<String> = first.stats.keys().cloned().collect();
    let f = if failed.is_empty() {
        failed_areas_of(first, &all_chk)
    } else {
        failed
    };
    let only_no_rerun = !f.is_empty() && f.iter().all(|a| cfg.no_rerun_areas.contains(a));
    if cand.len() == 1 && (cfg.no_rerun_single_member || only_no_rerun) {
        return false;
    }
    true
}

/// R23: pure decision for stale visual golden auto-refresh when pixel areas fail on a single PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaleShotsAction {
    NotApplicable {
        reason: String,
    },
    RefreshAndInvalidate {
        pr: u64,
        sha: String,
        redraw_areas: Vec<String>,
        reason: String,
    },
}

/// R23: decide whether a failed run on `cand` should trigger a visual-golden refresh (`shots-updated`).
pub fn stale_shots_step(
    cand: &[u64],
    heads: &BTreeMap<u64, String>,
    failed_areas: &[String],
    pr_files: &[String],
    cfg: &VerifyConfig,
) -> StaleShotsAction {
    if cand.len() != 1 {
        return StaleShotsAction::NotApplicable {
            reason: "screenshot refresh only runs on single-PR candidates".to_string(),
        };
    }
    let pr = cand[0];
    let fset: BTreeSet<String> = failed_areas.iter().cloned().collect();
    if fset.is_empty() || !fset.is_subset(&cfg.areas.pixel_areas) {
        return StaleShotsAction::NotApplicable {
            reason: format!(
                "failed areas {:?} are not exclusively pixel areas",
                failed_areas
            ),
        };
    }
    let redraw = super::areas::redraw_areas(pr_files, &cfg.areas);
    if redraw.is_empty() {
        return StaleShotsAction::NotApplicable {
            reason: "PR touches no pixel-input files".to_string(),
        };
    }
    let sha = heads.get(&pr).cloned().unwrap_or_default();
    StaleShotsAction::RefreshAndInvalidate {
        pr,
        sha,
        redraw_areas: redraw.into_iter().collect(),
        reason: format!(
            "single-PR #{pr} failed only in pixel areas {:?}; refreshing goldens and invalidating proof for new commit",
            failed_areas
        ),
    }
}

/// Phase of a running batch state machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    FullRun {
        round: u32,
        cand: Vec<u64>,
        run_label: String,
    },
    RetryOnRed {
        round: u32,
        cand: Vec<u64>,
        first: CheckResult,
        first_label: String,
        retry_areas: Vec<String>,
        run_label: String,
    },
    ConfirmFlaky {
        round: u32,
        cand: Vec<u64>,
        merged: Vec<u64>,
        skipped: Vec<u64>,
        skips: BTreeMap<u64, SkipReason>,
        first_tree: Option<String>,
        proved: BTreeMap<String, String>,
        retry_areas: Vec<String>,
        run_label: String,
    },
    Bisect {
        round: u32,
        cand: Vec<u64>,
        full_failed: CheckResult,
        full_label: String,
        skipped: Vec<u64>,
        skips: BTreeMap<u64, SkipReason>,
        narrow_areas: Vec<String>,
        probes: Vec<BisectProbe>,
        probe_len: usize,
        run_label: String,
    },
    BisectRetryOnRed {
        round: u32,
        cand: Vec<u64>,
        full_failed: CheckResult,
        full_label: String,
        skipped: Vec<u64>,
        skips: BTreeMap<u64, SkipReason>,
        narrow_areas: Vec<String>,
        probes: Vec<BisectProbe>,
        probe_len: usize,
        first_probe: CheckResult,
        run_label: String,
    },
    Done(BatchVerdict),
}

/// Pure state of one batch execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchState {
    pub batch_id: String,
    pub group: String,
    pub prs: Vec<u64>,
    pub heads: BTreeMap<u64, String>,
    pub areas: Vec<String>,
    pub main_sha: String,
    pub started: i64,
    pub reruns_used: u32,
    pub blamed: Vec<Blame>,
    pub all_skipped: Vec<u64>,
    pub all_skips: BTreeMap<u64, SkipReason>,
    pub phase: Phase,
}

impl BatchState {
    pub fn new(
        batch_id: impl Into<String>,
        group: impl Into<String>,
        prs: Vec<u64>,
        heads: BTreeMap<u64, String>,
        areas: Vec<String>,
        main_sha: impl Into<String>,
        started: i64,
    ) -> Self {
        Self {
            batch_id: batch_id.into(),
            group: group.into(),
            prs,
            heads,
            areas,
            main_sha: main_sha.into(),
            started,
            reruns_used: 0,
            blamed: Vec::new(),
            all_skipped: Vec::new(),
            all_skips: BTreeMap::new(),
            phase: Phase::Idle,
        }
    }
}

/// Context passed to `step`.
#[derive(Debug, Clone, Default)]
pub struct VerifyCtx {
    pub cfg: VerifyConfig,
    pub reusable: Vec<ReusableCandidate>,
    pub existing_hands: BTreeMap<u64, String>,
    pub pr_files: BTreeMap<u64, Vec<String>>,
}

/// Input event to `step`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VerifyEvent {
    Start {
        now: i64,
    },
    RunCompleted {
        run_label: String,
        result: CheckResult,
        now: i64,
    },
    ShotsRefreshed {
        pr: u64,
        new_sha: String,
        now: i64,
    },
}

/// Side-effecting action requested by `step`, always carrying a human `reason`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyAction {
    RunCheck {
        run_label: String,
        members: Vec<Member>,
        areas: Vec<String>,
        log_path: String,
        reason: String,
    },
    RecordFlakes {
        tests: Vec<String>,
        area: String,
        batch_id: String,
        log: String,
        reason: String,
    },
    RefreshStaleShots {
        pr: u64,
        sha: String,
        areas: Vec<String>,
        reason: String,
    },
    PostCommitStatuses {
        statuses: Vec<CommitStatusPost>,
        reason: String,
    },
    ClearStaleFailures {
        prs: Vec<u64>,
        reason: String,
    },
    RecordVerdict {
        verdict: BatchVerdict,
        reason: String,
    },
}

pub(super) fn members_for(prs: &[u64], heads: &BTreeMap<u64, String>) -> Vec<Member> {
    prs.iter()
        .map(|&pr| Member {
            pr,
            sha: heads.get(&pr).cloned().unwrap_or_default(),
        })
        .collect()
}

pub(super) fn log_path_for(run_label: &str) -> String {
    format!("logs/batch-{run_label}.log")
}

pub(super) fn run_check_action(
    run_label: String,
    prs: &[u64],
    heads: &BTreeMap<u64, String>,
    areas: Vec<String>,
    reason: String,
) -> VerifyAction {
    let log_path = log_path_for(&run_label);
    VerifyAction::RunCheck {
        run_label,
        members: members_for(prs, heads),
        areas,
        log_path,
        reason,
    }
}

pub(super) fn record_skips(st: &mut BatchState, r: &CheckResult) {
    for p in r.all_skipped() {
        if !st.all_skipped.contains(&p) {
            st.all_skipped.push(p);
        }
    }
    for (p, why) in &r.skips {
        st.all_skips.insert(*p, why.clone());
    }
}

pub(super) fn make_blame(
    st: &BatchState,
    pr: u64,
    r: &CheckResult,
    areas: &[String],
    run_label: &str,
    now: i64,
    cfg: &VerifyConfig,
) -> Blame {
    let sha = st.heads.get(&pr).cloned().unwrap_or_default();
    let failed_areas = failed_areas_of(r, areas);
    let summary = if !r.line.is_empty() {
        r.line.clone()
    } else {
        format!("failed: {}", failed_areas.join(" "))
    };
    Blame {
        pr,
        sha,
        batch_id: st.batch_id.clone(),
        summary,
        failed_areas,
        log_path: log_path_for(run_label),
        log_excerpt: excerpt(&r.log, &cfg.log_patterns),
        at: now,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_batch(
    mut st: BatchState,
    outcome: Outcome,
    passed_prs: &[u64],
    proved_by: BTreeMap<String, String>,
    tree: Option<String>,
    untied: Vec<UntiedRecord>,
    now: i64,
    ctx: &VerifyCtx,
    reason: String,
) -> (BatchState, Vec<VerifyAction>) {
    let mut passed_map = BTreeMap::new();
    for &pr in passed_prs {
        if let Some(sha) = st.heads.get(&pr).filter(|s| !s.is_empty()) {
            passed_map.insert(
                pr,
                PassedVerdict {
                    sha: sha.clone(),
                    batch_id: st.batch_id.clone(),
                    main_sha: st.main_sha.clone(),
                    tree: tree.clone(),
                    areas: st.areas.clone(),
                    proved_by: proved_by.clone(),
                    at: now,
                },
            );
        }
    }
    let rebases = skip_rebases(
        &st.all_skipped,
        &st.all_skips,
        &st.heads,
        &ctx.existing_hands,
    );
    let blamed_nums: Vec<u64> = st.blamed.iter().map(|b| b.pr).collect();
    let blame_sum = st.blamed.first().map(|b| b.summary.as_str());
    let statuses = batch_verdict_statuses(
        &st.batch_id,
        &st.prs,
        &st.heads,
        outcome,
        passed_prs,
        &blamed_nums,
        &st.all_skipped,
        blame_sum,
        &ctx.cfg,
    );
    let verdict = BatchVerdict {
        outcome,
        main_sha: (!st.main_sha.is_empty()).then(|| st.main_sha.clone()),
        passed: passed_map,
        blamed: st.blamed.clone(),
        skipped: st.all_skipped.clone(),
        skips: st.all_skips.clone(),
        rebases,
        untied,
        reason: reason.clone(),
    };
    st.phase = Phase::Done(verdict.clone());
    let mut actions = Vec::new();
    if !passed_prs.is_empty() {
        actions.push(VerifyAction::ClearStaleFailures {
            prs: passed_prs.to_vec(),
            reason: format!("clear previous failed/untied records for passed PRs {passed_prs:?}"),
        });
    }
    if !statuses.is_empty() {
        actions.push(VerifyAction::PostCommitStatuses {
            statuses,
            reason: format!(
                "post final commit statuses for batch {} ({})",
                st.batch_id,
                outcome.as_str()
            ),
        });
    }
    actions.push(VerifyAction::RecordVerdict { verdict, reason });
    (st, actions)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn complete_green_run(
    st: BatchState,
    outcome_base: Outcome,
    merged: Vec<u64>,
    proved: BTreeMap<String, String>,
    tree: Option<String>,
    now: i64,
    ctx: &VerifyCtx,
    mut extra_actions: Vec<VerifyAction>,
    reason_prefix: &str,
) -> (BatchState, Vec<VerifyAction>) {
    let missing = missing_proof(&st.areas, &proved);
    let (s, mut acts) = if !missing.is_empty() {
        finish_batch(
            st,
            Outcome::IncompleteProof,
            &[],
            proved,
            tree,
            Vec::new(),
            now,
            ctx,
            format!("{reason_prefix}: missing structured proof for area(s) {missing:?} (R27)"),
        )
    } else {
        let outcome = outcome_base.skips_adjusted(&st.all_skipped);
        let skip_note = if st.all_skipped.is_empty() {
            String::new()
        } else {
            format!(
                "; skipped {}",
                describe_skips(&st.all_skipped, &st.all_skips)
            )
        };
        finish_batch(
            st,
            outcome,
            &merged,
            proved,
            tree,
            Vec::new(),
            now,
            ctx,
            format!("{reason_prefix}: proved {merged:?}{skip_note}"),
        )
    };
    extra_actions.append(&mut acts);
    (s, extra_actions)
}
