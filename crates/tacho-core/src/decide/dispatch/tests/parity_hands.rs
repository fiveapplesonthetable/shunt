//! Parity cases 76..=103 (`test_hands.py` and `test_no_wasted_runs.py`).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::engine::work_step;
use crate::decide::dispatch::hands::{
    barred, barred_rebases, filter_sibling_conflicts, may_shape_pr, skips_to_hands, SkipEntry,
};
use crate::decide::dispatch::holds::hold_problem;
use crate::decide::dispatch::model::TrainView;
use crate::decide::dispatch::resume::decide_resume;
use crate::decide::dispatch::select::next_work;

fn pr_item(number: u64, branch: &str, draft: bool, mergeable: Mergeable) -> OpenPr {
    OpenPr {
        number,
        head: format!("sha-{number}"),
        branch: branch.to_string(),
        draft,
        mergeable,
    }
}

#[test]
fn parity_076_conflicting_pr_is_not_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)];
    let b = barred(
        &prs,
        &TrainView::default(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &cfg,
    );
    assert!(!b.contains(&5));
}

#[test]
fn parity_077_draft_pr_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(5, "feat-5", true, Mergeable::Conflicting)];
    let b = barred(
        &prs,
        &TrainView::default(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &cfg,
    );
    assert!(b.contains(&5));
}

#[test]
fn parity_078_mergeable_pr_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(5, "feat-5", false, Mergeable::Mergeable)];
    let b = barred(
        &prs,
        &TrainView::default(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &cfg,
    );
    assert!(b.contains(&5));
}

#[test]
fn parity_079_held_hands_key_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)];
    let mut holds = BTreeMap::new();
    holds.insert(
        "hands-5".to_string(),
        Hold {
            why: "wait".to_string(),
            since: 100,
        },
    );
    let b = barred(&prs, &TrainView::default(), &holds, &BTreeSet::new(), &cfg);
    assert!(b.contains(&5));
}

#[test]
fn parity_080_pr_in_open_train_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)];
    let mut tv = TrainView::default();
    tv.in_flight.insert(5);
    let b = barred(&prs, &tv, &BTreeMap::new(), &BTreeSet::new(), &cfg);
    assert!(b.contains(&5));
}

#[test]
fn parity_081_merged_train_member_remains_barred() {
    let cfg = test_cfg();
    let prs = vec![
        pr_item(5, "feat-5", false, Mergeable::Conflicting),
        pr_item(900, "train-900", false, Mergeable::Conflicting),
    ];
    let mut tv = TrainView::default();
    tv.landed.insert(5);
    tv.train_prs.insert(900);
    let b = barred(&prs, &tv, &BTreeMap::new(), &BTreeSet::new(), &cfg);
    assert!(b.contains(&5));
    assert!(b.contains(&900));
}

#[test]
fn parity_082_train_pr_itself_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(900, "some-branch", false, Mergeable::Conflicting)];
    let mut tv = TrainView::default();
    tv.train_prs.insert(900);
    let b = barred(&prs, &tv, &BTreeMap::new(), &BTreeSet::new(), &cfg);
    assert!(b.contains(&900));
}

#[test]
fn parity_083_train_branch_without_record_is_barred() {
    let cfg = test_cfg();
    let prs = vec![pr_item(
        901,
        "train-batch-12",
        false,
        Mergeable::Conflicting,
    )];
    let b = barred(
        &prs,
        &TrainView::default(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &cfg,
    );
    assert!(b.contains(&901));
}

#[test]
fn parity_084_hold_problem_allows_hands_key_before_first_run() {
    let cfg = test_cfg();
    assert!(hold_problem(
        "hands-1391",
        "reason",
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_085_hold_problem_allows_known_agent_not_in_todo() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("fix-7".to_string(), make_run(RunStatus::Running, "fix"));
    assert!(hold_problem("fix-7", "reason", &[], &BTreeMap::new(), &runs, &cfg).is_none());
}

#[test]
fn parity_086_hold_problem_allows_resumed_hands_key() {
    let cfg = test_cfg();
    assert!(hold_problem(
        "hands-1391~2",
        "reason",
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_087_hold_problem_still_rejects_non_numeric_hands_key() {
    let cfg = test_cfg();
    let prob = hold_problem(
        "hands-x",
        "reason",
        &[],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("problem");
    assert!(prob.contains("neither"));
}

#[test]
fn parity_088_work_dispatches_urgent_before_hands() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut u = make_todo("urgent-1", "urgent fix", "core");
    u.now = true;
    state.todo.push(u);
    state.hands.insert(
        5,
        HandsRec {
            sha: "sha-5".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)]);
    obs.pull_facts.insert(5, Ok(open_facts("sha-5", "feat-5")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.contains_key("urgent-1"));
}

#[test]
fn parity_089_work_dispatches_hands_before_ordinary_queue() {
    let mut cfg = test_cfg();
    cfg.slots = 1;
    let mut state = DispatchState::default();
    state
        .todo
        .push(make_todo("ordinary-1", "ordinary work", "core"));
    state.hands.insert(
        5,
        HandsRec {
            sha: "sha-5".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)]);
    obs.pull_facts.insert(5, Ok(open_facts("sha-5", "feat-5")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.contains_key("hands-5"));
    assert!(!state.runs.contains_key("ordinary-1"));
}

#[test]
fn parity_090_work_skips_held_hands() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    state.hands.insert(
        5,
        HandsRec {
            sha: "sha-5".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    state.holds.insert(
        "hands-5".to_string(),
        Hold {
            why: "hold".to_string(),
            since: 100,
        },
    );
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)]);
    obs.pull_facts.insert(5, Ok(open_facts("sha-5", "feat-5")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.is_empty());
}

#[test]
fn parity_091_work_skips_draft_hands() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    state.hands.insert(
        5,
        HandsRec {
            sha: "sha-5".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(5, "feat-5", true, Mergeable::Conflicting)]);
    obs.pull_facts.insert(5, Ok(open_facts("sha-5", "feat-5")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.is_empty());
}

#[test]
fn parity_092_work_skips_merged_train_member_hands() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    state.hands.insert(
        5,
        HandsRec {
            sha: "sha-5".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    state.trains.landed.insert(5);
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(5, "feat-5", false, Mergeable::Conflicting)]);
    obs.pull_facts.insert(5, Ok(open_facts("sha-5", "feat-5")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.is_empty());
}

#[test]
fn parity_093_barred_rebases_still_honours_holds_and_trains_when_github_fails() {
    let cfg = test_cfg();
    let mut holds = BTreeMap::new();
    holds.insert(
        "hands-5".to_string(),
        Hold {
            why: "w".to_string(),
            since: 1,
        },
    );
    let mut tv = TrainView::default();
    tv.in_flight.insert(7);
    tv.train_prs.insert(900);
    let b = barred_rebases(None, &tv, &holds, &BTreeMap::new(), &cfg);
    assert_eq!(b, BTreeSet::from([5, 7, 900]));
}

#[test]
fn parity_094_resume_skips_barred_hands_run() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut r = make_run(RunStatus::Unfinished, "rebase #5");
    r.finished = Some(1000);
    state.runs.insert("hands-5".to_string(), r);
    let barred_set = BTreeSet::from([5u64]);
    let (_, actions) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        &BTreeSet::new(),
        &barred_set,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1300,
        &cfg,
    );
    assert!(actions.is_empty());
    assert!(!state.runs.contains_key("hands-5~2"));
}

#[test]
fn parity_095_shape_never_replays_train_branch_pr_even_without_train_record() {
    let cfg = test_cfg();
    assert!(!may_shape_pr(
        905,
        "train-2026-10",
        &TrainView::default(),
        &cfg
    ));
}

#[test]
fn parity_096_queues_hands_when_passed_pr_conflicts_with_sibling_ahead_of_it() {
    let mut hands = BTreeMap::new();
    let passed = vec![1, 2];
    let conflicting = BTreeSet::from([2u64]);
    let mut heads = BTreeMap::new();
    heads.insert(2, "sha-2".to_string());
    let (ready, msgs) = filter_sibling_conflicts(&passed, &conflicting, &heads, &mut hands);
    assert_eq!(ready, vec![1]);
    assert!(hands[&2].sibling);
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].contains("#2"));
}

#[test]
fn parity_097_sibling_conflict_not_dropped_because_github_says_mergeable_with_main() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut skips = BTreeMap::new();
    skips.insert(
        2,
        SkipEntry {
            why: "CONFLICT".to_string(),
            files: vec!["src/lib.rs".to_string()],
        },
    );
    let mut heads = BTreeMap::new();
    heads.insert(2, "sha-2".to_string());
    let _ = skips_to_hands(&[2], &skips, &heads, &state.hands);
    filter_sibling_conflicts(&[1, 2], &BTreeSet::from([2u64]), &heads, &mut state.hands);
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.open_prs = Some(vec![pr_item(2, "feat-2", false, Mergeable::Mergeable)]);
    obs.pull_facts.insert(2, Ok(open_facts("sha-2", "feat-2")));
    work_step(&mut state, &obs, &cfg, false);
    assert!(state.runs.contains_key("hands-2"));
}

#[test]
fn parity_098_sibling_conflict_recorded_once_per_head() {
    let mut hands = BTreeMap::new();
    let passed = vec![1, 2];
    let conflicting = BTreeSet::from([2u64]);
    let mut heads = BTreeMap::new();
    heads.insert(2, "sha-2".to_string());
    let (_, msgs1) = filter_sibling_conflicts(&passed, &conflicting, &heads, &mut hands);
    let (_, msgs2) = filter_sibling_conflicts(&passed, &conflicting, &heads, &mut hands);
    assert_eq!(msgs1.len(), 1);
    assert!(msgs2.is_empty());
}

#[test]
fn parity_099_urgent_jumps_ahead_of_queued_tasks() {
    let cfg = test_cfg();
    let a = make_todo("a", "task a", "x");
    let b = make_todo("b", "task b", "x");
    let mut u = make_todo("urgent", "urgent task", "x");
    u.now = true;
    let todo = vec![a, b, u];
    let picked = next_work(
        &todo,
        &BTreeMap::new(),
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("picked");
    assert_eq!(picked.item.key, "urgent");
}

#[test]
fn parity_100_urgent_ignores_area_lane_when_sibling_is_running() {
    let cfg = test_cfg();
    let a = make_todo("a", "task a", "x");
    let mut u = make_todo("urgent", "urgent task", "x");
    u.now = true;
    let todo = vec![a, u];
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Running, "task a"));
    let picked = next_work(
        &todo,
        &runs,
        1,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("picked");
    assert_eq!(picked.item.key, "urgent");
}

#[test]
fn parity_101_urgent_starts_even_when_all_slots_are_full() {
    let cfg = test_cfg();
    let mut u = make_todo("urgent", "urgent task", "x");
    u.now = true;
    let todo = vec![u];
    let picked = next_work(
        &todo,
        &BTreeMap::new(),
        4,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("picked");
    assert_eq!(picked.item.key, "urgent");
}

#[test]
fn parity_102_running_urgent_task_does_not_block_normal_task_in_same_area() {
    let cfg = test_cfg();
    let mut u = make_todo("urgent", "urgent task", "x");
    u.now = true;
    let a = make_todo("a", "task a", "x");
    let todo = vec![u, a];
    let mut runs = BTreeMap::new();
    runs.insert(
        "urgent".to_string(),
        make_run(RunStatus::Running, "urgent task"),
    );
    let picked = next_work(
        &todo,
        &runs,
        1,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("picked");
    assert_eq!(picked.item.key, "a");
}

#[test]
fn parity_103_urgent_still_honours_explicit_after() {
    let cfg = test_cfg();
    let mut u = make_todo("urgent", "urgent task", "x");
    u.now = true;
    u.after = vec!["pr:9".to_string()];
    let todo = vec![u];
    let mut standings = BTreeMap::new();
    standings.insert(9, PrStanding::Waiting);
    let picked = next_work(
        &todo,
        &BTreeMap::new(),
        0,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    );
    assert!(picked.is_none());
}
