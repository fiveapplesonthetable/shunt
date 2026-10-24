//! Parity cases 60..=75 (`test_resume.py`).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::engine::owner_step;
use crate::decide::dispatch::holds::without_hold;
use crate::decide::dispatch::model::{ActionKind, RunState};
use crate::decide::dispatch::resume::{
    decide_resume, digest_due, digest_mail, reap_runs, stuck_digest_lines, stuck_runs, ProcObs,
};
use crate::decide::dispatch::runs::run_state;

fn setup_dead_appdata(cfg: &mut DispatchConfig) -> (DispatchState, BTreeMap<String, Stopped>) {
    // Use a non-session command so `resume` puts the full resume brief into `task` and `argv.last()`,
    // matching `test_resume.py` (`echo` command).
    cfg.command = "echo --model {model} {brief}".to_string();
    let mut state = DispatchState::default();
    let mut r = make_run(RunStatus::Unfinished, "original app-data task");
    r.finished = Some(1000);
    r.worktree = "/tmp/wt-app-data".to_string();
    r.summary = "the connection dropped".to_string();
    state.runs.insert("app-data".to_string(), r);
    let mut stopped = BTreeMap::new();
    stopped.insert(
        "/tmp/wt-app-data".to_string(),
        Stopped {
            path: "/tmp/wt-app-data".to_string(),
            commits: vec!["999aaaa commit subject".to_string()],
            changes: vec!["?? c.txt".to_string()],
        },
    );
    (state, stopped)
}

#[test]
fn parity_060_resumes_dead_agent_in_same_worktree() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    assert_eq!(actions.len(), 1);
    let r2 = &state.runs["app-data~2"];
    assert_eq!(r2.worktree, "/tmp/wt-app-data");
    assert!(r2.task.contains("/tmp/wt-app-data"));
    assert!(r2.task.contains("commit subject"));
    assert!(r2.task.contains("?? c.txt"));
    assert!(r2.task.contains("original app-data task"));
}

#[test]
fn parity_061_resume_prompt_reaches_launched_command() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    let ActionKind::Start(spec) = &actions[0].kind else {
        panic!("expected Start");
    };
    let last_arg = spec.argv.last().unwrap();
    assert!(last_arg.contains("/tmp/wt-app-data"));
    assert!(last_arg.contains("original app-data task"));
}

#[test]
fn parity_062_waits_for_backoff_before_resuming() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1000 + 60,
        &cfg,
    );
    assert!(actions.is_empty());
    assert!(!state.runs.contains_key("app-data~2"));
}

#[test]
fn parity_063_resumed_run_gives_agent_base_key_in_env() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    let ActionKind::Start(spec) = &actions[0].kind else {
        panic!("expected Start");
    };
    assert!(spec
        .env
        .contains(&(cfg.env("AGENT_KEY"), "app-data".to_string())));
}

#[test]
fn parity_064_respects_hold_when_resuming() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    state.holds.insert(
        "app-data".to_string(),
        Hold {
            why: "paused by user".to_string(),
            since: 1050,
        },
    );
    let (_, a1) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    assert!(a1.is_empty());
    assert!(!state.runs.contains_key("app-data~2"));

    state.holds = without_hold(&state.holds, "app-data");
    let (_, a2) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    assert_eq!(a2.len(), 1);
    assert!(state.runs.contains_key("app-data~2"));
}

#[test]
fn parity_065_respects_zero_slots_when_resuming() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    cfg.slots = 0;
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    assert!(actions.is_empty());
    assert!(!state.runs.contains_key("app-data~2"));
}

#[test]
fn parity_066_stops_after_one_retry_and_marks_stuck() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    let mut r2 = make_run(RunStatus::Unfinished, "task");
    r2.finished = Some(1300);
    state.runs.insert("app-data~2".to_string(), r2);
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
        &stopped,
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        2000,
        &cfg,
    );
    assert!(!state.runs.contains_key("app-data~3"));
    assert!(actions
        .iter()
        .all(|a| !matches!(a.kind, ActionKind::Start(_))));
    assert_eq!(run_state("app-data", &state.runs, &cfg), RunState::Stuck);
}

#[test]
fn parity_067_does_not_resume_when_summary_states_pr() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("app-data".to_string(), make_run(RunStatus::Running, "task"));
    let mut logs = BTreeMap::new();
    logs.insert("app-data".to_string(), "All done.\nPR: #12\n".to_string());
    reap_runs(
        &mut runs,
        &BTreeMap::new(),
        &logs,
        &BTreeMap::new(),
        &BTreeMap::new(),
        1000,
        &cfg,
    );
    assert_eq!(runs["app-data"].status, RunStatus::Done);
    let mut refused = BTreeMap::new();
    let mut blocked = None;
    let (_, actions) = decide_resume(
        &mut runs,
        &BTreeMap::new(),
        &mut refused,
        &mut blocked,
        None,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        false,
        true,
        2000,
        &cfg,
    );
    assert!(actions.is_empty());
}

#[test]
fn parity_068_guesses_worktree_made_during_run() {
    let mut cfg = test_cfg();
    cfg.work_root = "/tmp/fleet-work".to_string();
    let mut runs = BTreeMap::new();
    runs.insert("app-data".to_string(), make_run(RunStatus::Running, "task"));
    let mut procs = BTreeMap::new();
    procs.insert(
        "app-data".to_string(),
        ProcObs {
            alive: false,
            zombie: false,
            process_start: None,
        },
    );
    let mut made_between = BTreeMap::new();
    made_between.insert(
        "app-data".to_string(),
        vec!["/tmp/fleet-work/app-data-tree".to_string()],
    );
    reap_runs(
        &mut runs,
        &procs,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &made_between,
        1000,
        &cfg,
    );
    assert_eq!(runs["app-data"].worktree, "/tmp/fleet-work/app-data-tree");
}

#[test]
fn parity_069_owner_loop_mails_once_when_task_is_stuck() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    let mut r2 = make_run(RunStatus::Abandoned, "task");
    r2.finished = Some(1300);
    r2.worktree = "/tmp/wt-app-data".to_string();
    r2.summary = "the connection dropped\nnot resumed: no resumptions left".to_string();
    state.runs.insert("app-data~2".to_string(), r2);

    let mut obs = DispatchObs::default();
    obs.stopped_states = stopped;
    obs.now = 1500;
    let a1 = owner_step(&mut state, &obs, &cfg);
    obs.now = 1600;
    let a2 = owner_step(&mut state, &obs, &cfg);
    let mails: Vec<_> = a1
        .iter()
        .chain(a2.iter())
        .filter_map(|a| match &a.kind {
            ActionKind::MailOwner { subject, body, .. } => Some((subject.clone(), body.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(mails.len(), 1);
    let (_subj, body) = &mails[0];
    assert!(body.contains("app-data"));
    assert!(body.contains("/tmp/wt-app-data"));
    assert!(body.contains("the connection dropped"));
    assert!(body.contains("1 commits and 1 changed files"));
    assert!(body.contains("attach app-data <pr>"));
}

#[test]
fn parity_070_owner_loop_retires_stuck_task_whose_pr_is_closed() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    let mut r2 = make_run(RunStatus::Abandoned, "task");
    r2.finished = Some(1300);
    r2.pr = PrRecord::Found(FoundPr {
        number: 1433,
        branch: "b-1433".to_string(),
        how: PrHow::Stated,
    });
    state.runs.insert("app-data~2".to_string(), r2);

    let mut obs = DispatchObs::default();
    obs.stopped_states = stopped;
    obs.open_prs = Some(Vec::new());
    obs.now = 1500;
    // PR #1433 is not in obs.open_prs -> closed/merged
    let actions = owner_step(&mut state, &obs, &cfg);
    assert!(actions
        .iter()
        .all(|a| !matches!(a.kind, ActionKind::MailOwner { .. })));
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::RetireStuck { pr: 1433, .. }))
            .count(),
        1
    );
    assert_eq!(state.runs["app-data~2"].status, RunStatus::Retired);
}

#[test]
fn parity_071_owner_loop_keeps_stuck_task_whose_pr_is_still_open() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    let mut r2 = make_run(RunStatus::Unfinished, "task");
    r2.finished = Some(1300);
    r2.pr = PrRecord::Found(FoundPr {
        number: 1433,
        branch: "b-1433".to_string(),
        how: PrHow::Stated,
    });
    state.runs.insert("app-data~2".to_string(), r2);

    let mut obs = DispatchObs::default();
    obs.stopped_states = stopped;
    obs.open_prs = Some(vec![OpenPr {
        number: 1433,
        head: "abc".to_string(),
        branch: "b-1433".to_string(),
        draft: false,
        mergeable: Mergeable::Mergeable,
    }]);
    obs.now = 1500;
    let actions = owner_step(&mut state, &obs, &cfg);
    assert_eq!(state.runs["app-data~2"].status, RunStatus::Unfinished);
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::MailOwner { .. }))
            .count(),
        1
    );
}

#[test]
fn parity_072_owner_loop_stays_quiet_while_run_is_still_resumable() {
    let mut cfg = test_cfg();
    let (mut state, stopped) = setup_dead_appdata(&mut cfg);
    let mut obs = DispatchObs::default();
    obs.stopped_states = stopped;
    obs.now = 1100;
    let actions = owner_step(&mut state, &obs, &cfg);
    assert!(actions
        .iter()
        .all(|a| !matches!(a.kind, ActionKind::MailOwner { .. })));
}

#[test]
fn parity_073_digest_repeats_stuck_task_each_day() {
    let mut cfg = test_cfg();
    let (mut state, _) = setup_dead_appdata(&mut cfg);
    let mut r2 = make_run(RunStatus::Abandoned, "task");
    r2.finished = Some(100_000);
    r2.worktree = "/tmp/wt-app-data".to_string();
    state.runs.insert("app-data~2".to_string(), r2);

    let now = 100_000 + 7 * 3600;
    let stuck = stuck_runs(&state.runs, now, &cfg);
    let lines = stuck_digest_lines(&stuck, now);
    assert!(digest_due(0, now, &[], &lines));
    let (_, body) = digest_mail(&[], &lines, "", &cfg);
    assert!(body.contains("app-data has been stuck for 7 h"));
}

#[test]
fn parity_074_digest_stays_quiet_when_nothing_happened_and_nothing_stuck() {
    assert!(!digest_due(0, 200_000, &[], &[]));
}

#[test]
fn parity_075_status_reports_resuming_after_first_death() {
    let mut cfg = test_cfg();
    let (state, _) = setup_dead_appdata(&mut cfg);
    assert_eq!(run_state("app-data", &state.runs, &cfg), RunState::Resuming);
}
