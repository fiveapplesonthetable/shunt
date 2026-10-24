//! Parity cases 1..=14 (`test_dispatch.py` part 1).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::engine::work_step;
use crate::decide::dispatch::model::{ActionKind, StartKind, StartOutcome};
use crate::decide::dispatch::resume::decide_resume;
use crate::decide::dispatch::start::{decide_start, StartRequest};

pub(super) fn pr_start_req(key: &str, task: &str, pr: u64) -> StartRequest {
    StartRequest {
        key: key.to_string(),
        task: task.to_string(),
        model: "sonnet".to_string(),
        dry_run: false,
        area: String::new(),
        worktree: String::new(),
        base: None,
        pr: Some(pr),
        resume_session: String::new(),
        provider_override: None,
        pinned: false,
        kind: StartKind::Direct,
        new_session_id: Some("sess-uuid-1".to_string()),
    }
}

#[test]
fn parity_001_dispatch_sets_private_tmpdir() {
    let mut cfg = test_cfg();
    cfg.home = "/var/fleet-home".to_string();
    let mut state = DispatchState::default();
    let facts = Ok(open_facts(&"a".repeat(40), "feat-branch"));
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, actions) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts),
        true,
        1000,
        &cfg,
    );
    let StartOutcome::Started(spec) = outcome else {
        panic!("expected Started");
    };
    assert!(spec
        .env
        .contains(&("TMPDIR".to_string(), "/var/fleet-home/tmp".to_string())));
    assert_eq!(actions.len(), 1);
    assert!(!actions[0].reason.is_empty());
}

#[test]
fn parity_002_refuses_merged_pr() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut facts = open_facts(&"a".repeat(40), "feat");
    facts.state = PrState::Merged;
    let facts_res = Ok(facts);
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, actions) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    let msg = outcome.message("hands-1094");
    assert!(msg.contains("#1094 is merged"), "got: {msg}");
    assert!(state.runs.is_empty());
    assert!(state.refused.contains_key("hands-1094"));
    assert!(matches!(actions[0].kind, ActionKind::Refuse { .. }));
}

#[test]
fn parity_003_refuses_closed_pr() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut facts = open_facts(&"a".repeat(40), "feat");
    facts.state = PrState::Closed;
    let facts_res = Ok(facts);
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome.message("hands-1094").contains("#1094 is closed"));
}

#[test]
fn parity_004_refuses_deleted_branch() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut facts = open_facts(&"a".repeat(40), "gone-branch");
    facts.branch_exists = false;
    let facts_res = Ok(facts);
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome
        .message("hands-1094")
        .contains("gone-branch of #1094 no longer exists"));
    assert!(state.runs.is_empty());
}

#[test]
fn parity_005_refuses_missing_head() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut facts = open_facts(&"a".repeat(40), "feat");
    facts.head_exists = false;
    let facts_res = Ok(facts);
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome
        .message("hands-1094")
        .contains("head aaaaaaaa of #1094 no longer exists"));
    assert!(state.runs.is_empty());
}

#[test]
fn parity_006_reports_refusal_only_once() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut facts = open_facts(&"a".repeat(40), "feat");
    facts.state = PrState::Merged;
    let facts_res = Ok(facts);
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (first, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(!first.message("hands-1094").is_empty());
    let (second, actions2) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1001,
        &cfg,
    );
    assert_eq!(second.message("hands-1094"), "");
    assert!(actions2.is_empty());
}

#[test]
fn parity_007_refuses_when_pr_cannot_be_checked() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let facts_res: Result<PullFacts, String> = Err("gh: offline".to_string());
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome
        .message("hands-1094")
        .contains("could not check #1094"));
    assert!(state.runs.is_empty());
    assert!(!state.refused.contains_key("hands-1094"));
}

#[test]
fn parity_008_direct_start_respects_hold() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    state.holds.insert(
        "hands-1094".to_string(),
        Hold {
            why: "wait".to_string(),
            since: 900,
        },
    );
    let facts_res = Ok(open_facts(&"a".repeat(40), "feat"));
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome.message("hands-1094").contains("hands-1094 is held"));
    assert!(state.runs.is_empty());
}

#[test]
fn parity_009_direct_start_respects_zero_slots() {
    let mut cfg = test_cfg();
    cfg.slots = 0;
    let mut state = DispatchState::default();
    let facts_res = Ok(open_facts(&"a".repeat(40), "feat"));
    let req = pr_start_req("hands-1094", "rebase PR #1094", 1094);
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(outcome.message("hands-1094").contains("AGENT_SLOTS is 0"));
    assert!(state.runs.is_empty());
}

#[test]
fn parity_010_does_not_rerun_same_pr_head() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let head = "a".repeat(40);
    let facts_res = Ok(open_facts(&head, "feat"));
    let req1 = pr_start_req("hands-1094", "rebase v1", 1094);
    let (o1, _) = decide_start(
        &req1,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1000,
        &cfg,
    );
    assert!(matches!(o1, StartOutcome::Started(_)));
    state.runs.get_mut("hands-1094").unwrap().status = RunStatus::Done;
    let req2 = pr_start_req("hands-1094", "rebase v2", 1094);
    let (o2, actions2) = decide_start(
        &req2,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_res),
        true,
        1010,
        &cfg,
    );
    assert_eq!(o2, StartOutcome::AlreadyRanAtHead);
    assert_eq!(o2.message("hands-1094"), "");
    assert!(actions2.is_empty());
}

#[test]
fn parity_011_reruns_when_pr_head_changes() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let facts_a = Ok(open_facts(&"a".repeat(40), "feat"));
    let req1 = pr_start_req("hands-1094", "rebase v1", 1094);
    decide_start(
        &req1,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_a),
        true,
        1000,
        &cfg,
    );
    state.runs.get_mut("hands-1094").unwrap().status = RunStatus::Done;
    let facts_b = Ok(open_facts(&"b".repeat(40), "feat"));
    let req2 = pr_start_req("hands-1094", "rebase v2", 1094);
    let (o2, actions2) = decide_start(
        &req2,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts_b),
        true,
        1010,
        &cfg,
    );
    assert!(matches!(o2, StartOutcome::Started(_)));
    assert_eq!(actions2.len(), 1);
}

#[test]
fn parity_012_skips_refused_hands_and_moves_to_next_task() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    state.hands.insert(
        1094,
        HandsRec {
            sha: "a".repeat(40),
            files: vec!["x.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    state
        .todo
        .push(make_todo("app-data", "build app data", "app"));
    let mut obs = DispatchObs {
        now: 1000,
        env_live: true,
        ..DispatchObs::default()
    };
    let mut merged = open_facts(&"a".repeat(40), "feat");
    merged.state = PrState::Merged;
    obs.pull_facts.insert(1094, Ok(merged));
    let _actions = work_step(&mut state, &obs, &cfg, false);
    assert!(state.refused.contains_key("hands-1094"));
    assert!(state.runs.contains_key("app-data"));
    assert!(!state.runs.contains_key("hands-1094"));
}

#[test]
fn parity_013_resume_reuses_claude_session() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let facts = Ok(open_facts(&"a".repeat(40), "feat"));
    let mut req = pr_start_req("hands-1094", "fix the conflict in foo.rs", 1094);
    req.worktree = "/tmp/wt-1094".to_string();
    req.new_session_id = Some("sess-abc".to_string());
    decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&facts),
        true,
        1000,
        &cfg,
    );
    let r = state.runs.get_mut("hands-1094").unwrap();
    r.status = RunStatus::Unfinished;
    r.finished = Some(1000);
    let mut stopped = BTreeMap::new();
    stopped.insert(
        "/tmp/wt-1094".to_string(),
        Stopped {
            path: "/tmp/wt-1094".to_string(),
            commits: vec!["1234abc wip".to_string()],
            changes: Vec::new(),
        },
    );
    let mut pull_facts = BTreeMap::new();
    pull_facts.insert(1094, facts);
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
        &pull_facts,
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    let ActionKind::Start(spec) = &actions[0].kind else {
        panic!("expected Start");
    };
    assert_eq!(&spec.argv[..3], &["claude", "--resume", "sess-abc"]);
    assert!(!spec.brief.contains("fix the conflict in foo.rs"));
    assert!(spec.brief.contains("/tmp/wt-1094"));
}

#[test]
fn parity_014_resume_falls_back_to_task_when_switching_to_provider_without_sessions() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut run = make_run(RunStatus::Unfinished, "original task body");
    run.finished = Some(1000);
    run.worktree = "/tmp/wt-legacy".to_string();
    run.session = Some("old-claude-sess".to_string());
    state.runs.insert("legacy-task".to_string(), run);
    let session = Session {
        name: "codex-prof".to_string(),
        provider: "codex".to_string(),
        model: "gpt-5.4".to_string(),
    };
    let mut stopped = BTreeMap::new();
    stopped.insert(
        "/tmp/wt-legacy".to_string(),
        Stopped {
            path: "/tmp/wt-legacy".to_string(),
            commits: Vec::new(),
            changes: Vec::new(),
        },
    );
    let (_, actions) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        Some(&session),
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
    assert_eq!(&spec.argv[..4], &["codex", "exec", "--model", "gpt-5.4"]);
    assert!(!spec.argv.contains(&"--resume".to_string()));
    assert!(spec.brief.contains("original task body"));
    assert!(spec.brief.contains("/tmp/wt-legacy"));
    let r2 = &state.runs["legacy-task~2"];
    assert_eq!(r2.provider, "codex");
    assert_eq!(r2.model, "gpt-5.4");
    assert_eq!(r2.session.as_deref().unwrap_or(""), "");
}
