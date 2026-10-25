//! Parity tests for what happens to a `main-broken` batch on later ticks.
//!
//! A batch is `main-broken` when main itself fails the checks without the batch's PRs. The
//! reference keeps such a batch (and keeps its PRs from being re-batched) until either:
//!
//! - every member is merged or closed, when the batch is simply forgotten; or
//! - main's last completed CI run is green on a commit *after* the one the batch was checked
//!   against, when the batch is forgotten and its still-open members are batched again on the
//!   new main (merged members are dropped).
//!
//! While main is still red, or the green run is on the batch's own (old) main, the batch stays.
//! These cases drive the pure fleet [`decide`] tick.

use crate::decide::dispatch::PullFacts;
use crate::decide::verify::{BatchRun, MainCiObs};
use crate::decide::{decide, DecisionPlan, DecisionState, WorldObservation};
use std::collections::{BTreeMap, BTreeSet};

const OLD_MAIN: &str = "main0";
const NEW_MAIN: &str = "main1";

fn open_pr(n: u64) -> PullFacts {
    PullFacts {
        number: n,
        head: format!("head{n}"),
        base: "main".into(),
        branch: format!("pr{n}"),
        state: "OPEN".into(),
        files: vec![format!("crates/app/src/pr{n}.rs")],
        ..Default::default()
    }
}

/// State holding one `main-broken` batch of #1 and #2, checked against [`OLD_MAIN`].
fn main_broken_state() -> DecisionState {
    DecisionState {
        batches: vec![BatchRun {
            id: "b001".into(),
            group: "default".into(),
            prs: vec![1, 2],
            shas: BTreeMap::from([(1, "head1".into()), (2, "head2".into())]),
            main_sha: OLD_MAIN.into(),
            status: "main-broken".into(),
            started_at: 10,
            finished_at: Some(20),
            log: "test money::tests::adds ... FAILED".into(),
            ..Default::default()
        }],
        last_tick_ts: Some(30),
        ..Default::default()
    }
}

fn tick(
    state: &DecisionState,
    main: &str,
    open: &[u64],
    merged: &[u64],
    main_ci: Option<MainCiObs>,
) -> DecisionPlan {
    decide(
        state,
        &WorldObservation {
            now: 40,
            main_sha: main.into(),
            open_prs: open.iter().copied().map(open_pr).collect(),
            merged_prs: merged.iter().copied().collect::<BTreeSet<_>>(),
            main_ci,
            ..Default::default()
        },
    )
}

/// Main's last completed run, green on `sha`, which descends from [`OLD_MAIN`].
fn green_on(sha: &str) -> Option<MainCiObs> {
    Some(MainCiObs {
        sha: sha.into(),
        ok: true,
        descends_from: BTreeSet::from([OLD_MAIN.to_string()]),
    })
}

fn old_batch_status(plan: &DecisionPlan) -> Option<&str> {
    plan.next_state
        .batches
        .iter()
        .find(|b| b.id == "b001")
        .map(|b| b.status.as_str())
}

fn planned_prs(plan: &DecisionPlan) -> Vec<Vec<u64>> {
    plan.batch_plans.iter().map(|p| p.prs.clone()).collect()
}

#[test]
fn it_stays_while_main_is_still_red() {
    let red = Some(MainCiObs {
        sha: NEW_MAIN.into(),
        ok: false,
        descends_from: BTreeSet::from([OLD_MAIN.to_string()]),
    });
    let plan = tick(&main_broken_state(), NEW_MAIN, &[1, 2], &[], red);
    assert_eq!(old_batch_status(&plan), Some("main-broken"));
    assert_eq!(planned_prs(&plan), Vec::<Vec<u64>>::new());
}

#[test]
fn it_stays_while_the_green_run_is_not_on_a_newer_commit() {
    let plan = tick(
        &main_broken_state(),
        OLD_MAIN,
        &[1, 2],
        &[],
        green_on(OLD_MAIN),
    );
    assert_eq!(old_batch_status(&plan), Some("main-broken"));
    assert_eq!(planned_prs(&plan), Vec::<Vec<u64>>::new());
}

#[test]
fn it_is_re_batched_on_the_new_main_once_main_is_green_and_the_merged_member_is_forgotten() {
    let plan = tick(
        &main_broken_state(),
        NEW_MAIN,
        &[1],
        &[2],
        green_on(NEW_MAIN),
    );
    assert_eq!(old_batch_status(&plan), None);
    assert_eq!(plan.cleared_main_broken.len(), 1);
    assert_eq!(plan.cleared_main_broken[0].release, vec![1]);
    assert!(plan.cleared_main_broken[0]
        .reason
        .contains("main recovered"));
    assert_eq!(planned_prs(&plan), vec![vec![1]]);
}

#[test]
fn it_is_dropped_when_every_member_is_merged_or_closed() {
    let plan = tick(&main_broken_state(), NEW_MAIN, &[], &[1, 2], None);
    assert_eq!(old_batch_status(&plan), None);
    assert!(plan.cleared_main_broken[0]
        .reason
        .contains("merged or closed"));
}

#[test]
fn it_stays_when_main_ci_is_unknown() {
    let plan = tick(&main_broken_state(), NEW_MAIN, &[1, 2], &[], None);
    assert_eq!(old_batch_status(&plan), Some("main-broken"));
    assert_eq!(planned_prs(&plan), Vec::<Vec<u64>>::new());
}

#[test]
fn a_member_that_moved_head_is_checked_again() {
    let mut state = main_broken_state();
    state.batches[0].shas.insert(1, "older1".into());
    let plan = tick(&state, NEW_MAIN, &[1, 2], &[], None);
    assert_eq!(old_batch_status(&plan), Some("main-broken"));
    assert_eq!(planned_prs(&plan), vec![vec![1]]);
}

#[test]
fn observe_main_ci_reports_unknown_without_a_completed_run_and_asks_only_about_broken_mains() {
    use crate::decide::ops::snapshot::MainCiRun;
    use crate::decide::verify::unbreak::observe_main_ci;
    let batches = main_broken_state().batches;
    assert_eq!(observe_main_ci(None, &batches, |_, _| true), None);
    let running = MainCiRun {
        sha: NEW_MAIN.into(),
        ..Default::default()
    };
    assert_eq!(observe_main_ci(Some(&running), &batches, |_, _| true), None);
    let done = MainCiRun {
        sha: NEW_MAIN.into(),
        conclusion: "success".into(),
        ..Default::default()
    };
    let asked = std::cell::RefCell::new(Vec::new());
    let obs = observe_main_ci(Some(&done), &batches, |a, b| {
        asked.borrow_mut().push((a.to_string(), b.to_string()));
        true
    });
    assert_eq!(obs, green_on(NEW_MAIN));
    assert_eq!(
        asked.into_inner(),
        vec![(OLD_MAIN.to_string(), NEW_MAIN.to_string())]
    );
}
