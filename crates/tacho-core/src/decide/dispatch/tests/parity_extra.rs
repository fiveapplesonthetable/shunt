//! Additional parity tests covering `test_holds.py`, `test_retire.py`, `test_stop.py`,
//! `test_task_provider.py`, `test_closed_pr.py`, `test_merged_pr.py`, `test_queued_pr.py`,
//! `test_fix_agent.py`, worktree slug/port allocation (R80), and licence blocking (R44).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::brief::{allocate_ports, slug_problem};
use crate::decide::dispatch::compare::{
    compare_items, parse_compare, plan_compare, provider_of, resolve,
};
use crate::decide::dispatch::engine::work_step;
use crate::decide::dispatch::holds::{
    after_met, after_problem, closed_after, held_groups, hold_lines, hold_problem, may_start,
    pending_prs, pr_waited_on, release_problem, retire, retire_problem, retired_lines, stale_holds,
    standings_to_lookup, with_hold, without_hold,
};
use crate::decide::dispatch::model::{
    ActionKind, FailureRec, FixAttemptRec, FixStep, RunState, StartKind,
};
use crate::decide::dispatch::queue::{decide_fix, decide_fixes, fix_failure_mail, fix_task};
use crate::decide::dispatch::report::mark_no_change;
use crate::decide::dispatch::resume::{abandon_run, mark_stopped, stop_run};
use crate::decide::dispatch::select::{next_work, skipped_news};
use crate::decide::dispatch::start::{blocked_line, decide_start, StartRequest};

#[test]
fn parity_holds_after_and_stale_rules() {
    let cfg = test_cfg();
    assert_eq!(pr_waited_on("pr:42"), Some(42));
    assert_eq!(pr_waited_on("#42"), None);
    assert_eq!(pr_waited_on("42"), None);

    let todo = vec![make_todo("a", "ta", "tax")];
    let runs = BTreeMap::new();
    assert!(after_problem("a").is_none());
    assert!(after_problem("pr:12").is_none());
    assert!(after_problem("pr:0")
        .unwrap()
        .contains("not a pull request"));
    assert!(after_problem("pr:abc")
        .unwrap()
        .contains("not a pull request"));

    let mut standings = BTreeMap::new();
    standings.insert(12, PrStanding::Waiting);
    let done = BTreeSet::from(["a".to_string()]);
    assert!(!after_met(
        &["a".to_string(), "pr:12".to_string()],
        &done,
        &standings
    ));
    standings.insert(12, PrStanding::Passed);
    assert!(!after_met(
        &["a".to_string(), "pr:12".to_string()],
        &done,
        &standings
    ));
    standings.insert(12, PrStanding::Merged);
    assert!(after_met(
        &["a".to_string(), "pr:12".to_string()],
        &done,
        &standings
    ));
    standings.insert(12, PrStanding::Closed);
    assert_eq!(closed_after(&["pr:12".to_string()], &standings), Some(12));

    let mut holds = BTreeMap::new();
    holds = with_hold(&holds, "a", "investigating", 1000);
    holds = with_hold(&holds, "core", "group hold", 1000);
    assert!(!may_start(&todo[0], &done, &standings, &holds));
    assert_eq!(
        held_groups(&holds, &cfg),
        BTreeSet::from(["core".to_string()])
    );
    assert_eq!(hold_lines(&holds, 1060).len(), 2);
    assert!(release_problem("missing", &holds).is_some());
    assert!(release_problem("a", &holds).is_none());
    assert!(hold_problem("core", "group hold", &todo, &BTreeMap::new(), &runs, &cfg).is_none());
    assert!(
        hold_problem("core", "group hold", &todo, &holds, &runs, &cfg)
            .unwrap()
            .contains("already held")
    );

    let stale = stale_holds(&holds, &todo, 1000 + cfg.stale_hold_secs + 1, &cfg);
    assert_eq!(stale, vec!["a".to_string()]);
    let _ = without_hold(&holds, "core");
}

#[test]
fn parity_closed_pr_in_area_chain_is_skipped_and_retired_for_after() {
    let cfg = test_cfg();
    let t1 = make_todo("t1", "task 1", "tax");
    let t2 = make_todo("t2", "task 2", "tax");
    let mut t3 = make_todo("t3", "task 3", "");
    t3.after = vec!["pr:99".to_string()];
    let todo = vec![t1, t2, t3];
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "b7".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1".to_string(), r1);
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Closed);
    standings.insert(99, PrStanding::Closed);

    assert_eq!(pending_prs(&todo), BTreeSet::from([99]));
    assert_eq!(standings_to_lookup(&todo, &runs), BTreeSet::from([7, 99]));

    let picked = next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(picked.item.key, "t2");
    assert_eq!(picked.skipped, Some(7));
    let news = skipped_news("t2", 7);
    assert!(news.contains("#7, which its area waited on, was closed without landing"));

    // work_step retires t3 because pr:99 was closed without merging
    let mut state = DispatchState {
        todo,
        runs,
        ..DispatchState::default()
    };
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.standing = standings;
    let actions = work_step(&mut state, &obs, &cfg, false);
    assert!(actions.iter().any(
        |a| matches!(a.kind, ActionKind::RetireClosedAfter { ref key, pr: 99 } if key == "t3")
    ));
    assert!(!state.todo.iter().any(|i| i.key == "t3"));
}

#[test]
fn parity_retire_and_stop_commands() {
    let cfg = test_cfg();
    let todo = vec![make_todo("t1", "task 1", "tax")];
    let mut runs = BTreeMap::new();
    let mut holds = BTreeMap::new();
    holds = with_hold(&holds, "t1", "no longer needed", 900);
    let retired = BTreeMap::new();

    assert!(retire_problem("missing", "why", &todo, &runs, &cfg).is_some());
    assert!(retire_problem("t1", "no longer needed", &todo, &runs, &cfg).is_none());
    let (todo, holds, retired) = retire(&todo, &holds, &retired, "t1", "no longer needed", 1000);
    assert!(todo.is_empty());
    assert!(!holds.contains_key("t1"));
    assert_eq!(retired_lines(&retired).len(), 1);

    // stop_run:
    assert!(stop_run("t2", &mut runs, 1000).is_err());
    let mut r = make_run(RunStatus::Running, "task 2");
    r.unit = "tacho-t2.scope".to_string();
    r.pid = Some(1234);
    runs.insert("t2".to_string(), r);
    let act = stop_run("t2", &mut runs, 1050).expect("stopped");
    assert_eq!(runs["t2"].status, RunStatus::Stopped);
    assert!(matches!(act.kind, ActionKind::StopUnit { .. }));
    assert!(stop_run("t2", &mut runs, 1060).is_err());

    // mark_stopped & abandon_run:
    runs.insert("t3".to_string(), make_run(RunStatus::Running, "task 3"));
    assert!(mark_stopped("t3", "timed out", true, 1100, &mut runs));
    assert_eq!(runs["t3"].status, RunStatus::Unfinished);
    assert!(runs["t3"].nudge);
    let ab = abandon_run("t3", "cancelled", &mut runs).expect("abandoned");
    assert_eq!(runs["t3"].status, RunStatus::Abandoned);
    assert!(matches!(ab.kind, ActionKind::Abandon { .. }));
}

#[test]
fn parity_task_provider_and_compare_expansion() {
    let cfg = test_cfg();
    assert_eq!(provider_of("claude-sonnet-4-6", &cfg).unwrap(), "claude");
    assert_eq!(provider_of("opus", &cfg).unwrap(), "claude");
    assert_eq!(provider_of("gpt-5", &cfg).unwrap(), "codex");
    assert_eq!(provider_of("o3-mini", &cfg).unwrap(), "codex");
    assert!(provider_of("mystery-model", &cfg).is_err());

    assert_eq!(resolve(None, None, &cfg).unwrap(), None);
    assert_eq!(
        resolve(Some("codex"), None, &cfg).unwrap(),
        Some(("codex".to_string(), "gpt-6-luna".to_string()))
    );
    assert_eq!(
        resolve(Some("claude"), None, &cfg).unwrap(),
        Some(("claude".to_string(), "sonnet".to_string()))
    );
    assert!(resolve(Some("codex"), Some("sonnet"), &cfg).is_err());

    let pairs = parse_compare("codex:gpt-5,opus,codex:gpt-5", &cfg).unwrap();
    assert_eq!(
        pairs,
        vec![
            ("codex".to_string(), "gpt-5".to_string()),
            ("claude".to_string(), "opus".to_string()),
        ]
    );
    let items = compare_items(
        "feat",
        "build feature",
        "core",
        &["dep".to_string()],
        &pairs,
    );
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key, "feat-gpt-5");
    assert_eq!(items[0].area, "core-gpt-5");
    assert!(items[0].task.contains("[gpt-5]"));
    assert_eq!(items[1].key, "feat-opus");

    let planned = plan_compare("feat2", "build 2", "core", &[], "opus,gpt-5", &cfg).unwrap();
    assert_eq!(planned.len(), 2);
}

#[test]
fn parity_queued_pr_withdrawn_when_refused() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut item = make_todo("rebase-55", "rebase PR #55", "");
    item.kind = ItemKind::Rebase;
    item.pr = Some(55);
    state.todo.push(item);

    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    let mut closed = open_facts("abcdef123456", "feat-55");
    closed.state = PrState::Closed;
    obs.pull_facts.insert(55, Ok(closed));

    let actions = work_step(&mut state, &obs, &cfg, false);
    assert!(state.todo.is_empty());
    assert_eq!(state.task_history.len(), 1);
    assert_eq!(state.task_history["rebase-55"].status, RunStatus::Retired);
    assert!(!state.refused.contains_key("rebase-55"));
    assert!(actions
        .iter()
        .any(|a| matches!(a.kind, ActionKind::Withdraw { pr: 55, .. })));
}

#[test]
fn parity_fix_agent_lifecycle() {
    let cfg = test_cfg();
    let fail = FailureRec {
        sha: "111122223333".to_string(),
        why: "cargo test failed".to_string(),
        areas: vec!["core".to_string()],
        tests: vec!["test_foo".to_string()],
        log: "assertion failed".to_string(),
    };
    assert_eq!(decide_fix(&fail, None, RunState::Queued), FixStep::Start);
    let rec = FixAttemptRec {
        sha: "111122223333".to_string(),
        told: false,
    };
    assert_eq!(
        decide_fix(&fail, Some(&rec), RunState::Running),
        FixStep::Wait
    );
    assert_eq!(decide_fix(&fail, Some(&rec), RunState::Done), FixStep::Tell);
    let fail_moved = FailureRec {
        sha: "999988887777".to_string(),
        ..fail.clone()
    };
    assert_eq!(
        decide_fix(&fail_moved, Some(&rec), RunState::Running),
        FixStep::Tell
    );
    let rec_told = FixAttemptRec {
        sha: "111122223333".to_string(),
        told: true,
    };
    assert_eq!(
        decide_fix(&fail, Some(&rec_told), RunState::Done),
        FixStep::Done
    );

    let brief = fix_task(77, "b77", &fail, &cfg);
    assert!(brief.contains("PR #77"));
    let (subj, body) = fix_failure_mail(77, &fail_moved, true, &cfg);
    assert!(subj.contains("#77"));
    assert!(body.contains("pushed a fix"));

    let mut runs = BTreeMap::new();
    let holds = BTreeMap::new();
    let mut refused = BTreeMap::new();
    let mut blocked = None;
    let mut fix_attempts = BTreeMap::new();
    let mut failures = BTreeMap::new();
    failures.insert(77u64, fail);
    let mut branches_by_pr = BTreeMap::new();
    branches_by_pr.insert(77, "b77".to_string());
    let mut pull_facts = BTreeMap::new();
    pull_facts.insert(77, Ok(open_facts("111122223333", "b77")));

    let acts1 = decide_fixes(
        &failures,
        &mut fix_attempts,
        &BTreeSet::new(),
        &mut runs,
        &holds,
        &mut refused,
        &mut blocked,
        None,
        &branches_by_pr,
        &pull_facts,
        false,
        true,
        1000,
        &cfg,
    );
    assert_eq!(acts1.len(), 2);
    assert!(runs.contains_key("fix-77"));
    assert!(fix_attempts.contains_key(&77));
}

#[test]
fn parity_worktree_slug_port_allocator_and_licence_block() {
    let cfg = test_cfg();
    assert!(slug_problem("ab").is_some());
    assert!(slug_problem("Bad_Slug").is_some());
    assert!(slug_problem("good-slug-1").is_none());

    let held = BTreeSet::from([20000u16]);
    let busy = BTreeSet::from([20020u16]);
    assert_eq!(allocate_ports(&held, |p| busy.contains(&p)), Some(20040));

    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Done, "ta"));
    assert!(mark_no_change("a", &mut runs));
    assert!(runs["a"].no_change);

    // Missing live licence blocks start and emits Blocked once:
    let holds = BTreeMap::new();
    let mut refused = BTreeMap::new();
    let mut blocked = None;
    let req = StartRequest {
        key: "a-lic".to_string(),
        task: "do work".to_string(),
        model: "sonnet".to_string(),
        dry_run: false,
        area: String::new(),
        worktree: String::new(),
        base: None,
        pr: None,
        resume_session: String::new(),
        provider_override: None,
        pinned: false,
        kind: StartKind::Direct,
        new_session_id: None,
    };
    let (out, acts) = decide_start(
        &req,
        &mut runs,
        &holds,
        &mut refused,
        &mut blocked,
        None,
        None,
        false,
        3660,
        &cfg,
    );
    assert!(out.message("a-lic").contains("LIVE is not set"));
    assert_eq!(acts.len(), 1);
    assert!(matches!(acts[0].kind, ActionKind::Blocked { .. }));
    let bl = blocked_line(blocked.as_ref(), "01:01");
    assert!(bl.contains("daemon cannot start agents:"));
}
