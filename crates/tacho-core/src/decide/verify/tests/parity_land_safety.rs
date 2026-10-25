//! Parity tests for the safety rules of landing a verified sequential train.
//!
//! Each case mirrors a reference landing scenario: a train of two stacked PRs (#1 then #2)
//! verified on `main0` with the merged tree `tree12`, driven through the pure
//! [`decide_land`] engine one event at a time. The rules covered are:
//!
//! - a train is refused once main has moved, and nothing is merged;
//! - a member head that moved since verification stops the landing before any merge;
//! - members merge in the order they were verified, and main ends as the verified tree;
//! - a tree that is not the verified one is posted loudly and the train is a mismatch;
//! - a merge outside a train invalidates the open train, so it can no longer land;
//! - a train's own landing does not invalidate it on the next tick;
//! - a refused merge waits for a mergeability update instead of sleeping, then retries;
//! - a PR that never merges stops the train and says where it stopped.
//!
//! These live beside the verify tests because `land` keeps its tests in flat files.

use crate::decide::land::ready::Conflicted;
use crate::decide::land::{
    decide_land, LandActionKind, LandConfig, LandDecision, LandEvent, LandView, Member, Train,
    TrainId, TrainStatus,
};
use std::collections::BTreeMap;

const MAIN: &str = "main0";
const TREE: &str = "tree12";

/// A view holding one open train of #1 and #2 (stacked), verified on [`MAIN`].
fn train_view() -> (LandView, TrainId) {
    let train = Train::make(
        MAIN,
        vec![Member::new(1, "head1"), Member::new(2, "head2")],
        TREE,
    );
    let id = train.id.clone();
    let view = LandView {
        main_sha: MAIN.into(),
        main_seen: Some(MAIN.into()),
        open_prs: BTreeMap::from([(1, "head1".into()), (2, "head2".into())]),
        trains: vec![train],
        ..Default::default()
    };
    (view, id)
}

fn cfg() -> LandConfig {
    LandConfig {
        merge_attempts: 3,
        ..Default::default()
    }
}

fn merged(id: &TrainId, pr: u64) -> LandEvent {
    LandEvent::MergeResult {
        id: id.clone(),
        pr,
        ok: true,
        err: String::new(),
        now: 100,
    }
}

fn refused(id: &TrainId, pr: u64, err: &str) -> LandEvent {
    LandEvent::MergeResult {
        id: id.clone(),
        pr,
        ok: false,
        err: err.into(),
        now: 100,
    }
}

/// The PR numbers `decision` asks the hands to merge, in order.
fn merges(decision: &LandDecision) -> Vec<u64> {
    decision
        .actions
        .iter()
        .filter_map(|a| match &a.kind {
            LandActionKind::MergePr { pr, .. } => Some(*pr),
            _ => None,
        })
        .collect()
}

fn refusal_reason(decision: &LandDecision) -> Option<&str> {
    decision.actions.iter().find_map(|a| match &a.kind {
        LandActionKind::RefuseLand { reason, .. } => Some(reason.as_str()),
        _ => None,
    })
}

fn status(view: &LandView, id: &TrainId) -> TrainStatus {
    view.trains
        .iter()
        .find(|t| &t.id == id)
        .map(|t| t.status.clone())
        .expect("train is recorded")
}

/// Lands the whole train with every merge succeeding and main ending at `main_tree`.
fn land_all(view: &LandView, id: &TrainId, main_tree: &str) -> (LandDecision, Vec<u64>) {
    let cfg = cfg();
    let mut order = Vec::new();
    let mut d = decide_land(view, &LandEvent::LandRequested { id: id.clone() }, &cfg);
    order.extend(merges(&d));
    d = decide_land(&d.next, &merged(id, 1), &cfg);
    order.extend(merges(&d));
    d = decide_land(&d.next, &merged(id, 2), &cfg);
    assert!(d
        .actions
        .iter()
        .any(|a| matches!(&a.kind, LandActionKind::VerifyMainTree { train_id } if train_id == id)));
    d = decide_land(
        &d.next,
        &LandEvent::MainTreeVerified {
            id: id.clone(),
            main_sha: "main_after".into(),
            main_tree: main_tree.into(),
            now: 120,
        },
        &cfg,
    );
    (d, order)
}

#[test]
fn a_train_is_refused_once_main_has_moved() {
    let (mut view, id) = train_view();
    view.main_sha = "elsewhere".into();
    let d = decide_land(&view, &LandEvent::LandRequested { id: id.clone() }, &cfg());
    assert!(refusal_reason(&d).is_some_and(|r| r.contains("not at")));
    assert_eq!(merges(&d), Vec::<u64>::new());
    assert_eq!(status(&d.next, &id), TrainStatus::Open);
}

#[test]
fn a_head_that_moved_since_it_was_verified_stops_the_landing() {
    let (mut view, id) = train_view();
    view.open_prs.insert(2, "changed".into());
    let d = decide_land(&view, &LandEvent::LandRequested { id: id.clone() }, &cfg());
    assert!(refusal_reason(&d).is_some_and(|r| r.contains("#2 changed")));
    assert_eq!(merges(&d), Vec::<u64>::new());
    assert_eq!(d.next.main_sha, MAIN);
}

#[test]
fn the_prs_are_merged_in_the_order_they_were_verified_and_main_ends_as_the_verified_tree() {
    let (view, id) = train_view();
    let (d, order) = land_all(&view, &id, TREE);
    assert_eq!(order, vec![1, 2]);
    assert_eq!(status(&d.next, &id), TrainStatus::Landed);
    assert!(!d
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::LoudPost { .. })));
}

#[test]
fn a_tree_that_is_not_the_verified_one_is_posted_loudly() {
    let (view, id) = train_view();
    let (d, _) = land_all(&view, &id, "sneaked_in");
    assert!(matches!(status(&d.next, &id), TrainStatus::Mismatch { .. }));
    let loud = d
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::LoudPost { subject, line, .. } => Some((subject, line)),
            _ => None,
        })
        .expect("a mismatch is posted loudly");
    assert!(loud.1.contains("not the verified"), "{}", loud.1);
    assert!(loud.0.contains("did not land as verified"), "{}", loud.0);
}

#[test]
fn a_merge_outside_a_train_invalidates_the_open_train() {
    let (view, id) = train_view();
    let cfg = cfg();
    let moved = decide_land(
        &view,
        &LandEvent::MainMoved {
            new_main: "elsewhere".into(),
            merged_since: Some(vec![9]),
            now: 50,
        },
        &cfg,
    );
    assert!(matches!(
        status(&moved.next, &id),
        TrainStatus::Invalidated { .. }
    ));
    let d = decide_land(&moved.next, &LandEvent::LandRequested { id }, &cfg);
    assert!(refusal_reason(&d).is_some_and(|r| r.contains("invalidated")));
    assert_eq!(merges(&d), Vec::<u64>::new());
}

#[test]
fn landing_a_train_does_not_invalidate_it_on_the_next_tick() {
    let (view, id) = train_view();
    let (landed, _) = land_all(&view, &id, TREE);
    let tick = decide_land(
        &landed.next,
        &LandEvent::MainMoved {
            new_main: "main_after".into(),
            merged_since: Some(vec![1, 2]),
            now: 130,
        },
        &cfg(),
    );
    assert_eq!(status(&tick.next, &id), TrainStatus::Landed);
}

#[test]
fn the_mergeability_recompute_is_retried_without_sleeping() {
    let (view, id) = train_view();
    let cfg = cfg();
    let mut attempts = Vec::new();
    let mut waits = 0;
    let mut d = decide_land(&view, &LandEvent::LandRequested { id: id.clone() }, &cfg);
    attempts.extend(merges(&d));
    for _ in 0..2 {
        d = decide_land(
            &d.next,
            &refused(&id, 1, "Pull request is not mergeable"),
            &cfg,
        );
        waits += d
            .actions
            .iter()
            .filter(|a| matches!(a.kind, LandActionKind::WaitForMergeRetry { pr: 1, .. }))
            .count();
        d = decide_land(
            &d.next,
            &LandEvent::MergeabilityChanged {
                pr: 1,
                verdict: Conflicted::No,
            },
            &cfg,
        );
        attempts.extend(merges(&d));
    }
    d = decide_land(&d.next, &merged(&id, 1), &cfg);
    assert_eq!(merges(&d), vec![2]);
    assert_eq!(attempts, vec![1, 1, 1]);
    assert_eq!(waits, 2);
    assert!(status(&d.next, &id).is_landing());
}

#[test]
fn a_pr_that_never_merges_stops_the_train_and_says_so() {
    let (view, id) = train_view();
    let cfg = cfg();
    let mut d = decide_land(&view, &LandEvent::LandRequested { id: id.clone() }, &cfg);
    d = decide_land(&d.next, &merged(&id, 1), &cfg);
    for _ in 0..cfg.merge_attempts {
        d = decide_land(&d.next, &refused(&id, 2, "not mergeable"), &cfg);
    }
    assert!(matches!(status(&d.next, &id), TrainStatus::Stopped { .. }));
    let line = d
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::LoudPost { line, .. } => Some(line.as_str()),
            _ => None,
        })
        .expect("a stopped train is posted loudly");
    assert!(line.contains("stopped at #2 after merging #1"), "{line}");
}
