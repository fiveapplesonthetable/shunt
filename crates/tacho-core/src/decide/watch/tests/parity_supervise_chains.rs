//! Parity tests for the supervisor's pure decisions, ported from the reference suite's
//! supervise, supervise-loop and progress-gap tests.
//!
//! Covered reference behaviour not already exercised by `supervise_hourly.rs`/`progress.rs`:
//! - a batch that already ended is not counted as a duplicate of a running one;
//! - idle-chain reasons: waiting on an unmerged PR names it, a run in progress or a pending
//!   resumption keeps the chain from being idle, a stated `PR: none` names the predecessor;
//! - loop level: agents are sampled on every tick, while the hourly pass only runs when due;
//! - progress rows skip a task whose only record is a later attempt (a gap at attempt 1).
//!
//! "A dry run changes nothing" has no pure-decide counterpart: `decide_watch` only returns a plan,
//! and whether to apply it is the executor's choice.

use std::collections::{BTreeMap, BTreeSet};

use crate::decide::watch::chains::{rows, ChainTodoItem, PrRef, PrStanding, RunRecord, RunStatus};
use crate::decide::watch::config::WatchConfig;
use crate::decide::watch::procs::{duplicate_batches, BatchObs};
use crate::decide::watch::supervise::{decide_watch, due, idle_chains, Idle, WatchObs, WatchState};

fn todo() -> Vec<ChainTodoItem> {
    ["one", "two"]
        .iter()
        .map(|k| ChainTodoItem {
            key: (*k).to_string(),
            area: "app".to_string(),
            ..Default::default()
        })
        .collect()
}

fn record(status: RunStatus) -> RunRecord {
    RunRecord {
        status,
        started: 0,
        finished: (status != RunStatus::Running).then_some(0),
        ..Default::default()
    }
}

/// Idle chains over the two-item `app` chain with four free slots and an idle machine.
fn idle(
    agents: &BTreeMap<String, RunRecord>,
    standing: &BTreeMap<u64, PrStanding>,
    running: usize,
) -> Vec<Idle> {
    idle_chains(
        &todo(),
        agents,
        standing,
        &BTreeSet::new(),
        running,
        4,
        false,
        &BTreeMap::new(),
        &WatchConfig::default(),
    )
}

#[test]
fn a_batch_that_already_ended_is_not_counted_as_a_duplicate() {
    let batches = vec![
        BatchObs {
            id: "a".to_string(),
            prs: vec![1],
            started: 100,
            pid: Some(11),
            running: false,
        },
        BatchObs {
            id: "b".to_string(),
            prs: vec![1],
            started: 200,
            pid: Some(12),
            running: true,
        },
    ];
    let alive: BTreeSet<u32> = [11, 12].into_iter().collect();
    assert!(duplicate_batches(&batches, &alive).is_empty());
}

#[test]
fn a_chain_waiting_on_an_unmerged_pr_says_which() {
    let mut one = record(RunStatus::Done);
    one.pr = Some(PrRef::stated(7, "b"));
    one.pr_checked = true;
    let agents = BTreeMap::from([("one".to_string(), one)]);
    let standing = BTreeMap::from([(7, PrStanding::Waiting)]);
    let idles = idle(&agents, &standing, 0);
    assert_eq!(idles.len(), 1, "{idles:?}");
    assert_eq!(
        (idles[0].area.as_str(), idles[0].key.as_str()),
        ("app", "two")
    );
    assert!(idles[0].why.contains("#7"), "{}", idles[0].why);
}

#[test]
fn a_chain_with_a_run_in_progress_is_not_idle() {
    let agents = BTreeMap::from([("one".to_string(), record(RunStatus::Running))]);
    assert!(idle(&agents, &BTreeMap::new(), 1).is_empty());
}

#[test]
fn a_pr_none_with_no_change_is_named_as_the_known_false_block() {
    let mut one = record(RunStatus::Done);
    one.stated_no_pr = true;
    one.pr_checked = true;
    let agents = BTreeMap::from([("one".to_string(), one)]);
    let idles = idle(&agents, &BTreeMap::new(), 0);
    assert_eq!(idles.len(), 1, "{idles:?}");
    assert_eq!(idles[0].no_pr_of, "one");
}

#[test]
fn a_chain_waiting_on_a_resumption_is_not_idle() {
    let agents = BTreeMap::from([("one".to_string(), record(RunStatus::Unfinished))]);
    assert!(idle(&agents, &BTreeMap::new(), 0).is_empty());
}

/// State with one live running agent, last supervised at `t = 1000`.
fn loop_state() -> WatchState {
    let mut agent = record(RunStatus::Running);
    agent.pid = Some(10);
    agent.started = 1_000;
    WatchState {
        agents: BTreeMap::from([("app".to_string(), agent)]),
        supervised: Some(1_000),
        ..Default::default()
    }
}

fn loop_obs(now: i64, c: &WatchConfig, st: &WatchState) -> WatchObs {
    WatchObs {
        now,
        live_pids: [10].into_iter().collect(),
        run_hourly: due(st.supervised.unwrap_or(0), now, c),
        ..Default::default()
    }
}

#[test]
fn agents_are_sampled_every_tick_and_the_hourly_pass_runs_once_an_hour() {
    let c = WatchConfig::default();
    let st = loop_state();

    let before = 1_000 + c.report_interval - 60;
    let obs = loop_obs(before, &c, &st);
    assert!(!obs.run_hourly);
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.next_state.samples["app"].at, before);
    assert!(plan.hourly_lines.is_empty(), "{:?}", plan.hourly_lines);

    let on_time = 1_000 + c.report_interval;
    let obs = loop_obs(on_time, &c, &st);
    assert!(obs.run_hourly);
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.next_state.samples["app"].at, on_time);
    assert!(!plan.hourly_lines.is_empty());
}

#[test]
fn progress_rows_skip_a_task_whose_only_record_is_a_later_run() {
    let agents = BTreeMap::from([("app-2~2".to_string(), record(RunStatus::Done))]);
    let out = rows(
        &agents,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        0,
        &WatchConfig::default(),
    );
    assert!(out.is_empty(), "{out:?}");
}
