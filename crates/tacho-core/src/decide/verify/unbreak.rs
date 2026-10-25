//! When a `main-broken` batch is finished with: its PRs are gone, or main has gone green since.
//!
//! A batch is `main-broken` when main itself failed the checks without the batch's PRs. It is
//! kept (and its PRs are not re-batched at the heads it recorded) until either every member is
//! merged or closed, or main's last completed CI run is green on a commit that descends from
//! the main the batch was checked against.

use super::BatchRun;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Main's last completed CI run, as observed by the hands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MainCiObs {
    /// Commit the run checked.
    pub sha: String,
    /// Whether the run succeeded.
    pub ok: bool,
    /// The recorded `main_sha`s of `main-broken` batches that are ancestors of [`Self::sha`].
    /// Hands fill this in (e.g. `git merge-base --is-ancestor`) for every such batch in state;
    /// a main absent from the set is treated as not an ancestor.
    #[serde(default)]
    pub descends_from: BTreeSet<String>,
}

/// A `main-broken` batch to forget, with the PRs to check again on the new main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearedBatch {
    pub batch_id: String,
    /// Still-open members that become eligible for batching again.
    pub release: Vec<u64>,
    pub reason: String,
}

/// True when `ci` is green on a commit after (and descending from) the batch's main.
pub fn main_recovered(batch: &BatchRun, ci: &MainCiObs) -> bool {
    ci.ok
        && !ci.sha.is_empty()
        && ci.sha != batch.main_sha
        && ci.descends_from.contains(&batch.main_sha)
}

/// Decide which `main-broken` batches to forget. `main_ci` of `None` means unknown: such a
/// batch is only dropped once it has no open members.
pub fn clear(
    batches: &[BatchRun],
    open: &BTreeSet<u64>,
    main_ci: Option<&MainCiObs>,
) -> Vec<ClearedBatch> {
    let mut out = Vec::new();
    for b in batches.iter().filter(|b| b.status == "main-broken") {
        let release: Vec<u64> = b.prs.iter().copied().filter(|n| open.contains(n)).collect();
        if release.is_empty() {
            out.push(ClearedBatch {
                batch_id: b.id.clone(),
                release,
                reason: "every member is merged or closed".into(),
            });
        } else if let Some(ci) = main_ci.filter(|ci| main_recovered(b, ci)) {
            out.push(ClearedBatch {
                batch_id: b.id.clone(),
                reason: format!(
                    "main recovered: CI green on {} (after {}); re-batching {:?}",
                    ci.sha, b.main_sha, release
                ),
                release,
            });
        }
    }
    out
}

/// Build the [`MainCiObs`] for a tick from main's last completed forge run.
///
/// Returns `None` (unknown) when there is no completed run. `is_ancestor(a, b)` must report
/// whether commit `a` is an ancestor of `b` (e.g. `git merge-base --is-ancestor`); it is only
/// asked about the mains of `main-broken` batches.
pub fn observe_main_ci(
    last_completed: Option<&crate::decide::ops::snapshot::MainCiRun>,
    batches: &[BatchRun],
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> Option<MainCiObs> {
    let run = last_completed.filter(|r| !r.sha.is_empty() && !r.conclusion.is_empty())?;
    let descends_from = batches
        .iter()
        .filter(|b| b.status == "main-broken" && !b.main_sha.is_empty())
        .map(|b| b.main_sha.as_str())
        .filter(|m| *m == run.sha || is_ancestor(m, &run.sha))
        .map(str::to_string)
        .collect();
    Some(MainCiObs {
        sha: run.sha.clone(),
        ok: run.conclusion.eq_ignore_ascii_case("success"),
        descends_from,
    })
}
