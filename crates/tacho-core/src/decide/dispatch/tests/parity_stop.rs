//! Parity with the reference project's stop tests: a run a person stopped stays stopped.
//!
//! Covered reference behaviour:
//! - a stopped run is never resumed, however long ago it ended, and its task reads `stuck`;
//! - stopping targets the task's latest run and emits a `StopUnit` for its unit;
//! - a task with no running run, or with no run at all, cannot be stopped (and says why);
//! - the supervisor's own stop (`mark_stopped`) is still resumed after the backoff;
//! - state written before the `stopped` status existed reads the same;
//! - a resume the dispatcher refuses abandons the run once and is not retried every tick;
//! - a run with no resumptions left is abandoned and reported stuck, including old state.
//!
//! Not covered here: the attention line suggesting the stop command (rendering lives outside
//! `decide::dispatch`).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::model::{Action, ActionKind, RunState, Runs};
use crate::decide::dispatch::resume::{decide_resume, mark_stopped, stop_run, stuck_runs};
use crate::decide::dispatch::runs::run_state;

/// When the first run of the task started.
const NOW: i64 = 1_000;

/// One resume pass over `state` at `now`, with no holds, trains, worktree evidence, or live pids.
fn resume_pass(
    state: &mut DispatchState,
    pull_facts: &BTreeMap<u64, Result<PullFacts, String>>,
    now: i64,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    let (_, actions) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        pull_facts,
        &BTreeSet::new(),
        false,
        true,
        now,
        cfg,
    );
    actions
}

fn starts(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a.kind, ActionKind::Start(_)))
        .count()
}

/// A state whose `app-data` task has one running run in its own unit.
fn running_app_data() -> DispatchState {
    let mut state = DispatchState::default();
    let mut run = make_run(RunStatus::Running, "app-data task");
    run.started = NOW;
    run.unit = "tacho-app-data.scope".to_string();
    run.worktree = "/tmp/wt-app-data".to_string();
    state.runs.insert("app-data".to_string(), run);
    state
}

/// Marks `key` as having died unfinished at `at`.
fn die(runs: &mut Runs, key: &str, at: i64) {
    let run = runs.get_mut(key).expect("run exists");
    run.status = RunStatus::Unfinished;
    run.finished = Some(at);
}

fn stop(runs: &mut Runs, key: &str) -> Result<Action, String> {
    stop_run(key, runs, NOW)
}

#[test]
fn stopped_run_is_not_resumed_after_it_is_reaped() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    stop(&mut state.runs, "app-data").expect("a running run can be stopped");
    // Reaping leaves a person's stop alone; resume long after must not bring it back.
    let actions = resume_pass(&mut state, &BTreeMap::new(), NOW + 1_000_000, &cfg);
    assert_eq!(starts(&actions), 0);
    assert_eq!(state.runs["app-data"].status, RunStatus::Stopped);
    assert!(!state.runs.contains_key("app-data~2"));
}

#[test]
fn stopping_a_run_stops_its_unit() {
    let mut state = running_app_data();
    let action = stop(&mut state.runs, "app-data").expect("a running run can be stopped");
    match action.kind {
        ActionKind::StopUnit { key, unit, .. } => {
            assert_eq!(key, "app-data");
            assert_eq!(unit.as_deref(), Some("tacho-app-data.scope"));
        }
        other => panic!("expected StopUnit, got {other:?}"),
    }
}

#[test]
fn stopped_task_waits_for_a_person() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    stop(&mut state.runs, "app-data").expect("a running run can be stopped");
    assert_eq!(run_state("app-data", &state.runs, &cfg), RunState::Stuck);
}

#[test]
fn latest_run_of_a_task_is_the_one_stopped() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    die(&mut state.runs, "app-data", NOW);
    let actions = resume_pass(&mut state, &BTreeMap::new(), NOW + cfg.backoff_for(1), &cfg);
    assert_eq!(starts(&actions), 1);
    stop(&mut state.runs, "app-data").expect("the resumed run is running");
    assert_eq!(state.runs["app-data~2"].status, RunStatus::Stopped);
    assert_eq!(state.runs["app-data"].status, RunStatus::Unfinished);
}

#[test]
fn task_with_no_running_run_cannot_be_stopped() {
    let mut state = running_app_data();
    state.runs.get_mut("app-data").unwrap().status = RunStatus::Done;
    let why = stop(&mut state.runs, "app-data").expect_err("a done run cannot be stopped");
    assert!(why.contains("not running"), "{why}");
    assert_eq!(state.runs["app-data"].status, RunStatus::Done);
}

#[test]
fn task_nobody_started_cannot_be_stopped() {
    let mut runs = Runs::new();
    let why = stop(&mut runs, "nothing").expect_err("nothing to stop");
    assert!(why.contains("no run"), "{why}");
}

#[test]
fn supervisors_own_stop_is_still_resumed() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    assert!(mark_stopped(
        "app-data",
        "stopped: idle",
        false,
        NOW,
        &mut state.runs
    ));
    let actions = resume_pass(&mut state, &BTreeMap::new(), NOW + cfg.backoff_for(1), &cfg);
    assert_eq!(starts(&actions), 1);
    assert!(state.runs.contains_key("app-data~2"));
}

#[test]
fn state_written_before_the_stopped_status_existed_reads_the_same() {
    let cfg = test_cfg();
    let old: Runs = serde_json::from_str(
        r#"{"app-data": {"status": "unfinished", "task": "app-data task", "finished": 1000}}"#,
    )
    .expect("old state parses");
    assert!(old.values().all(|r| r.status != RunStatus::Stopped));
    assert_eq!(run_state("app-data", &old, &cfg), RunState::Resuming);
}

#[test]
fn resume_the_dispatcher_refuses_is_not_retried_every_tick() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    die(&mut state.runs, "app-data", NOW);
    state.runs.get_mut("app-data").unwrap().for_pr = Some(7);
    let mut closed = open_facts("abc12345", "app-data");
    closed.state = PrState::Closed;
    let facts = BTreeMap::from([(7, Ok(closed))]);

    let first = resume_pass(&mut state, &facts, NOW + cfg.backoff_for(1), &cfg);
    assert_eq!(starts(&first), 0);
    assert_eq!(state.runs["app-data"].status, RunStatus::Abandoned);

    let second = resume_pass(&mut state, &facts, NOW + 2 * cfg.backoff_for(1), &cfg);
    assert_eq!(starts(&second), 0);
    assert!(second.is_empty(), "nothing more to do: {second:?}");
    assert_eq!(run_state("app-data", &state.runs, &cfg), RunState::Stuck);
}

#[test]
fn run_with_no_resumptions_left_stops_being_unfinished() {
    let cfg = test_cfg();
    let mut state = running_app_data();
    die(&mut state.runs, "app-data", NOW);
    let actions = resume_pass(&mut state, &BTreeMap::new(), NOW + cfg.backoff_for(1), &cfg);
    assert_eq!(starts(&actions), 1);
    die(&mut state.runs, "app-data~2", NOW);
    let actions = resume_pass(&mut state, &BTreeMap::new(), NOW + 1_000_000, &cfg);
    assert_eq!(starts(&actions), 0);
    assert_eq!(state.runs["app-data~2"].status, RunStatus::Abandoned);
    let keys: Vec<String> = stuck_runs(&state.runs, NOW, &cfg)
        .into_iter()
        .map(|s| s.key)
        .collect();
    assert_eq!(keys, ["app-data"]);
}

#[test]
fn state_with_an_exhausted_unfinished_run_is_still_reported_stuck() {
    let cfg = test_cfg();
    let old: Runs = serde_json::from_str(
        r#"{"a": {"status": "unfinished", "task": "", "finished": 1},
            "a~2": {"status": "unfinished", "task": "", "finished": 2}}"#,
    )
    .expect("old state parses");
    let keys: Vec<String> = stuck_runs(&old, 5, &cfg)
        .into_iter()
        .map(|s| s.key)
        .collect();
    assert_eq!(keys, ["a"]);
}
