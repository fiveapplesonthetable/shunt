//! Parity cases 46–59: Hourly supervision pass (duplicate batches, idle chains, false `PR: none`,
//! unmailed green PRs, health report, fault isolation, and daily digest).

use std::collections::{BTreeMap, BTreeSet};

use crate::decide::watch::activity::Activity;
use crate::decide::watch::chains::{ChainTodoItem, PrHow, PrRef, PrStanding, RunRecord, RunStatus};
use crate::decide::watch::config::WatchConfig;
use crate::decide::watch::procs::{duplicate_batches, BatchObs, Proc, ProcId};
use crate::decide::watch::supervise::{
    area_gate, decide_owner_mail, decide_watch, due, idle_chains, AreaGate, WatchAction, WatchObs,
    WatchState,
};

fn cfg() -> WatchConfig {
    WatchConfig::default()
}

fn proc_row(pid: u32, pgid: u32) -> Proc {
    Proc {
        id: ProcId {
            pid,
            start_ticks: 100,
        },
        ppid: 1,
        pgid,
        name: "batch".to_string(),
        age: 100,
        cpu: 1.0,
        cwd: "/wt".to_string(),
        starved: false,
        argv: vec!["batch".to_string()],
        cgroup: String::new(),
    }
}

fn done_run(pr: Option<u64>, stated_no_pr: bool, no_change: bool, wt: &str) -> RunRecord {
    RunRecord {
        status: RunStatus::Done,
        started: 100,
        finished: Some(200),
        pid: Some(100),
        worktree: wt.to_string(),
        base_sha: "abc".to_string(),
        task: "work".to_string(),
        model: "claude".to_string(),
        pr: pr.map(|n| PrRef {
            number: n,
            branch: format!("b{n}"),
            how: PrHow::Stated,
        }),
        pr_checked: true,
        stated_no_pr,
        no_change,
        nudge: false,
        stop_reason: String::new(),
        summary: "done".to_string(),
    }
}

#[test]
fn case_46_due_returns_true_after_report_interval() {
    let c = cfg();
    assert!(!due(1_000, 1_000 + c.report_interval - 1, &c));
    assert!(due(1_000, 1_000 + c.report_interval, &c));
}

#[test]
fn case_47_duplicate_batches_keeps_oldest_alive_and_returns_newer_duplicates() {
    let batches = vec![
        BatchObs {
            id: "b-new".to_string(),
            running: true,
            prs: vec![20, 10],
            started: 2_000,
            pid: Some(202),
        },
        BatchObs {
            id: "b-old".to_string(),
            running: true,
            prs: vec![10, 20],
            started: 1_000,
            pid: Some(101),
        },
    ];
    let alive: BTreeSet<u32> = [101, 202].into_iter().collect();
    assert_eq!(
        duplicate_batches(&batches, &alive),
        vec![("b-new".to_string(), 202)]
    );
}

#[test]
fn case_48_duplicate_batches_ignores_batches_whose_pid_is_dead() {
    let batches = vec![
        BatchObs {
            id: "b-old-dead".to_string(),
            running: true,
            prs: vec![10],
            started: 1_000,
            pid: Some(101),
        },
        BatchObs {
            id: "b-new-live".to_string(),
            running: true,
            prs: vec![10],
            started: 2_000,
            pid: Some(202),
        },
    ];
    let alive: BTreeSet<u32> = [202].into_iter().collect();
    assert!(duplicate_batches(&batches, &alive).is_empty());
}

#[test]
fn case_49_hourly_supersedes_duplicate_batch_and_stops_leader_group() {
    let c = cfg();
    let mut st = WatchState::default();
    st.batches.insert(
        "b1".to_string(),
        BatchObs {
            id: "b1".to_string(),
            running: true,
            prs: vec![5],
            started: 1_000,
            pid: Some(101),
        },
    );
    st.batches.insert(
        "b2".to_string(),
        BatchObs {
            id: "b2".to_string(),
            running: true,
            prs: vec![5],
            started: 1_500,
            pid: Some(202),
        },
    );
    let obs = WatchObs {
        now: 5_000,
        procs: vec![proc_row(101, 101), proc_row(202, 202)],
        run_hourly: true,
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert!(!plan.next_state.batches["b2"].running);
    assert!(plan.actions.iter().any(|a| matches!(
        a,
        WatchAction::SupersedeBatch { batch, stop_group: true, .. } if batch == "b2"
    )));
}

#[test]
fn case_50_idle_chains_returns_empty_when_busy_or_slots_full() {
    let c = cfg();
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "core".to_string(),
        ..Default::default()
    }];
    assert!(idle_chains(
        &todo,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        0,
        2,
        true,
        &BTreeMap::new(),
        &c
    )
    .is_empty());
    assert!(idle_chains(
        &todo,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        2,
        2,
        false,
        &BTreeMap::new(),
        &c
    )
    .is_empty());
}

#[test]
fn case_51_idle_chains_flags_ready_head_as_orchestrator_fault() {
    let c = cfg();
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "core".to_string(),
        ..Default::default()
    }];
    let idles = idle_chains(
        &todo,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        0,
        2,
        false,
        &BTreeMap::new(),
        &c,
    );
    assert_eq!(idles.len(), 1);
    assert!(idles[0]
        .why
        .contains("could start and has not; this is a fault in tacho"));
}

#[test]
fn case_52_idle_chains_skips_held_head() {
    let c = cfg();
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "core".to_string(),
        ..Default::default()
    }];
    let holds: BTreeSet<String> = ["t1".to_string()].into_iter().collect();
    let idles = idle_chains(
        &todo,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &holds,
        0,
        2,
        false,
        &BTreeMap::new(),
        &c,
    );
    assert!(idles.is_empty());
}

#[test]
fn case_53_hourly_lifts_false_pr_none_block_when_worktree_is_empty() {
    let c = cfg();
    let mut st = WatchState::default();
    st.todo = vec![
        ChainTodoItem {
            key: "t1".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
        ChainTodoItem {
            key: "t2".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
    ];
    st.agents
        .insert("t1".to_string(), done_run(None, true, false, "/wt/t1"));

    let mut obs = WatchObs {
        now: 5_000,
        run_hourly: true,
        ..Default::default()
    };
    obs.activities
        .insert("/wt/t1".to_string(), Activity::quiet());

    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.next_state.agents["t1"].no_change);
    assert!(plan.actions.iter().any(|a| matches!(
        a,
        WatchAction::LiftPrNone { key, area, .. } if key == "t1" && area == "core"
    )));
}

#[test]
fn case_54_hourly_keeps_pr_none_block_when_worktree_has_commits_or_dirty_files() {
    let c = cfg();
    let mut st = WatchState::default();
    st.todo = vec![
        ChainTodoItem {
            key: "t1".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
        ChainTodoItem {
            key: "t2".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
    ];
    st.agents
        .insert("t1".to_string(), done_run(None, true, false, "/wt/t1"));

    let mut obs = WatchObs {
        now: 5_000,
        run_hourly: true,
        ..Default::default()
    };
    obs.activities.insert(
        "/wt/t1".to_string(),
        Activity {
            commits: 1,
            files: 0,
            last_commit: Some(4_000),
            last_change: Some(4_000),
            last_test: None,
        },
    );

    let plan = decide_watch(&st, &obs, &c);
    assert!(!plan.next_state.agents["t1"].no_change);
    assert!(plan
        .hourly_lines
        .iter()
        .any(|l| l.contains("chain core is idle at t2")));
}

#[test]
fn case_55_area_gate_stacks_on_passed_pr_or_skips_closed_pr() {
    let c = cfg();
    let todo = vec![
        ChainTodoItem {
            key: "t1".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
        ChainTodoItem {
            key: "t2".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
    ];
    let mut agents = BTreeMap::new();
    agents.insert("t1".to_string(), done_run(Some(50), false, false, "/wt/t1"));

    let mut standing = BTreeMap::new();
    standing.insert(50, PrStanding::Passed);
    assert_eq!(
        area_gate(&todo[1], &todo, &agents, &standing, &BTreeMap::new(), &c),
        AreaGate::GoPr {
            pr: 50,
            branch: "b50".to_string()
        }
    );

    standing.insert(50, PrStanding::Closed);
    assert_eq!(
        area_gate(&todo[1], &todo, &agents, &standing, &BTreeMap::new(), &c),
        AreaGate::GoMain { skipped: Some(50) }
    );
}

#[test]
fn case_56_hourly_mails_owner_once_for_unmailed_green_pr() {
    let c = cfg();
    let mut st = WatchState::default();
    st.unmailed_green_prs = vec![(88, "feat: fast parser".to_string())];
    let obs = WatchObs {
        now: 5_000,
        run_hourly: true,
        ..Default::default()
    };

    let plan1 = decide_watch(&st, &obs, &c);
    assert!(plan1.actions.iter().any(|a| matches!(
        a,
        WatchAction::MailOwner { dedup_key, .. } if dedup_key == "green-pr:88"
    )));

    let plan2 = decide_watch(&plan1.next_state, &obs, &c);
    assert!(!plan2.actions.iter().any(|a| matches!(
        a,
        WatchAction::MailOwner { dedup_key, .. } if dedup_key == "green-pr:88"
    )));
}

#[test]
fn case_57_hourly_step_fault_is_reported_without_aborting_later_steps() {
    let c = cfg();
    let st = WatchState::default();
    let mut obs = WatchObs {
        now: 5_000,
        run_hourly: true,
        cpu_percent: Some(30.0),
        load: 1.5,
        free_gb: 200,
        ..Default::default()
    };
    obs.step_faults
        .insert("duplicate batches".to_string(), "boom".to_string());

    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.next_state.supervised, Some(5_000));
    assert!(plan
        .hourly_lines
        .iter()
        .any(|l| l == "supervision: duplicate batches failed: boom"));
    assert!(plan
        .hourly_lines
        .iter()
        .any(|l| l.starts_with("health: cpu 30%")));
}

#[test]
fn case_58_hourly_updates_supervised_timestamp() {
    let c = cfg();
    let st = WatchState::default();
    let obs = WatchObs {
        now: 9_999,
        run_hourly: true,
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.next_state.supervised, Some(9_999));
}

#[test]
fn case_59_daily_digest_includes_events_and_progress_table_when_due() {
    let c = cfg();
    let mut st = WatchState::default();
    st.digest_entries
        .push(("12:00".to_string(), "merged #10".to_string()));
    let obs = WatchObs {
        now: 100_000,
        ..Default::default()
    };
    let mails = decide_owner_mail(&mut st, &obs, true, &c);
    assert_eq!(mails.len(), 1);
    match &mails[0] {
        WatchAction::MailOwner { subject, body, .. } => {
            assert_eq!(subject, "[tacho] daily digest");
            assert!(body.contains("12:00  merged #10"));
            assert!(body.contains("PROGRESS"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(st.digest_entries.is_empty());
    assert_eq!(st.digest_sent, Some(100_000));
}
