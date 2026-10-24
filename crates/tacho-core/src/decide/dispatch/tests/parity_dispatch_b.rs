//! Parity cases 15..=27 (`test_dispatch.py` part 2).

use std::collections::{BTreeMap, BTreeSet};

use super::parity_dispatch::pr_start_req;
use super::*;
use crate::decide::dispatch::brief::with_settings;
use crate::decide::dispatch::compare::resolve_session;
use crate::decide::dispatch::engine::owner_step;
use crate::decide::dispatch::model::{ActionKind, StartOutcome};
use crate::decide::dispatch::resume::decide_resume;
use crate::decide::dispatch::start::decide_start;

#[test]
fn parity_015_resume_with_kept_provider_still_follows_active_session_when_unpinned() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut req = pr_start_req("task-1", "do task 1", 1094);
    req.pr = None;
    req.provider_override = Some("claude".to_string());
    req.pinned = false;
    req.worktree = "/tmp/wt-1".to_string();
    decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        None,
        true,
        1000,
        &cfg,
    );
    let r = state.runs.get_mut("task-1").unwrap();
    r.status = RunStatus::Unfinished;
    r.finished = Some(1000);
    let session = Session {
        name: "codex-prof".to_string(),
        provider: "codex".to_string(),
        model: "gpt-6-luna".to_string(),
    };
    let mut stopped = BTreeMap::new();
    stopped.insert(
        "/tmp/wt-1".to_string(),
        Stopped {
            path: "/tmp/wt-1".to_string(),
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
    assert_eq!(spec.argv[0], "codex");
    assert!(!spec.argv.contains(&"--resume".to_string()));
    assert!(spec.brief.contains("do task 1"));
    assert!(spec.brief.contains("/tmp/wt-1"));
}

#[test]
fn parity_016_active_session_cannot_be_overridden_by_caller_args() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let session = Session {
        name: "codex-prof".to_string(),
        provider: "codex".to_string(),
        model: "gpt-6-luna".to_string(),
    };
    let mut req = pr_start_req("task-x", "do x", 1094);
    req.pr = None;
    req.model = "sonnet".to_string();
    req.provider_override = Some("claude".to_string());
    req.pinned = false;
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        Some(&session),
        None,
        true,
        1000,
        &cfg,
    );
    let StartOutcome::Started(spec) = outcome else {
        panic!("expected Started");
    };
    assert_eq!(spec.argv[0], "codex");
    assert_eq!(state.runs["task-x"].provider, "codex");
}

#[test]
fn parity_017_provider_override_selects_matching_command_without_session() {
    let mut cfg = test_cfg();
    cfg.command = "codex exec --model {model} {brief}".to_string();
    let mut state = DispatchState::default();
    let mut req = pr_start_req("task-c", "do c", 1094);
    req.pr = None;
    req.provider_override = Some("claude".to_string());
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        None,
        true,
        1000,
        &cfg,
    );
    let StartOutcome::Started(spec) = outcome else {
        panic!("expected Started");
    };
    assert_eq!(spec.argv[0], "claude");
    assert_eq!(state.runs["task-c"].provider, "claude");
}

#[test]
fn parity_018_resume_checks_pr_still_open() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let open = Ok(open_facts(&"a".repeat(40), "feat"));
    let req = pr_start_req("hands-1094", "rebase #1094", 1094);
    decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        Some(&open),
        true,
        1000,
        &cfg,
    );
    let r = state.runs.get_mut("hands-1094").unwrap();
    r.status = RunStatus::Unfinished;
    r.finished = Some(1000);
    let mut merged = open_facts(&"a".repeat(40), "feat");
    merged.state = PrState::Merged;
    let mut pull_facts = BTreeMap::new();
    pull_facts.insert(1094, Ok(merged));
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
        &pull_facts,
        &BTreeSet::new(),
        false,
        true,
        1200,
        &cfg,
    );
    assert!(!state.runs.contains_key("hands-1094~2"));
    assert_eq!(state.runs["hands-1094"].status, RunStatus::Abandoned);
    assert!(actions
        .iter()
        .any(|a| matches!(a.kind, ActionKind::Abandon { .. })));
}

#[test]
fn parity_019_exhausted_agent_escalates_once_with_log_tail() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let mut r1 = make_run(RunStatus::Unfinished, "task");
    r1.finished = Some(1000);
    let log_30: String = (0..30)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut r2 = make_run(RunStatus::Unfinished, "task");
    r2.finished = Some(1300);
    r2.summary = log_30.clone();
    state.runs.insert("hands-1094".to_string(), r1);
    state.runs.insert("hands-1094~2".to_string(), r2);

    let mut obs = DispatchObs::default();
    obs.logs.insert("hands-1094~2".to_string(), log_30);
    obs.pull_facts
        .insert(1094, Ok(open_facts(&"a".repeat(40), "feat")));

    let (_, resume_actions) = decide_resume(
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
        &obs.pull_facts,
        &BTreeSet::new(),
        false,
        true,
        1500,
        &cfg,
    );
    assert!(resume_actions
        .iter()
        .any(|a| matches!(a.kind, ActionKind::Abandon { .. })));

    obs.now = 1500;
    let a1 = owner_step(&mut state, &obs, &cfg);
    obs.now = 1510;
    let a2 = owner_step(&mut state, &obs, &cfg);
    let comments: Vec<_> = a1
        .iter()
        .chain(a2.iter())
        .filter_map(|a| match &a.kind {
            ActionKind::CommentIssue { body, .. } => Some(body.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("line-10"));
    assert!(comments[0].contains("line-29"));
    assert!(!comments[0].contains("line-9\n"));
}

#[test]
fn parity_020_never_starts_third_run_in_full_cycle() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let facts = Ok(open_facts(&"a".repeat(40), "feat"));
    let req = pr_start_req("hands-1094", "rebase #1094", 1094);
    let mut launches = 0;
    let mut pull_facts = BTreeMap::new();
    pull_facts.insert(1094, facts.clone());

    for cycle in 0..4 {
        let now = 1000 + cycle * 500;
        let (o, _) = decide_start(
            &req,
            &mut state.runs,
            &state.holds,
            &mut state.refused,
            &mut state.blocked,
            None,
            Some(&facts),
            true,
            now,
            &cfg,
        );
        if matches!(o, StartOutcome::Started(_)) {
            launches += 1;
        }
        for r in state.runs.values_mut() {
            if r.status == RunStatus::Running {
                r.status = RunStatus::Unfinished;
                r.finished = Some(now + 10);
            }
        }
        let (_, r_acts) = decide_resume(
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
            &pull_facts,
            &BTreeSet::new(),
            false,
            true,
            now + 200,
            &cfg,
        );
        launches += r_acts
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::Start(_)))
            .count();
        for r in state.runs.values_mut() {
            if r.status == RunStatus::Running {
                r.status = RunStatus::Unfinished;
                r.finished = Some(now + 210);
            }
        }
    }
    assert_eq!(launches, 2);
}

#[test]
fn parity_021_starts_claude_with_no_background_bash_hook_settings() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let facts = Ok(open_facts(&"a".repeat(40), "feat"));
    let req = pr_start_req("hands-1094", "rebase #1094", 1094);
    let (outcome, _) = decide_start(
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
    assert!(spec.argv.contains(&"--settings".to_string()));
    let (_, content) = spec.settings.expect("settings file");
    assert!(content.contains("no-background-bash"));
}

#[test]
fn parity_022_leaves_non_claude_command_unchanged() {
    let cfg = test_cfg();
    let argv = vec!["codex".to_string(), "exec".to_string()];
    let out = with_settings(argv.clone(), "/tmp/settings.json", &cfg);
    assert_eq!(out, argv);
}

#[test]
fn parity_023_session_command_switches_provider_and_model() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let sess = resolve_session(Some("codex-fleet-session"), Some("codex"), None, &cfg).unwrap();
    state.session = Some(sess);
    let mut req = pr_start_req("hands-1094", "rebase #1094", 1094);
    req.pr = None;
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        state.session.as_ref(),
        None,
        true,
        1000,
        &cfg,
    );
    let StartOutcome::Started(spec) = outcome else {
        panic!("expected Started");
    };
    assert_eq!(spec.argv[0], "codex");
    assert_eq!(spec.model, "gpt-6-luna");
    assert_eq!(state.runs["hands-1094"].provider, "codex");
    assert_eq!(state.runs["hands-1094"].model, "gpt-6-luna");
}

#[test]
fn parity_024_session_requires_provider() {
    let cfg = test_cfg();
    let err = resolve_session(Some("some-session"), None, None, &cfg).unwrap_err();
    assert!(err.contains("requires --provider"));
}

#[test]
fn parity_025_claude_session_requires_explicit_model() {
    let cfg = test_cfg();
    let err = resolve_session(Some("claude-session"), Some("claude"), None, &cfg).unwrap_err();
    assert!(err.contains("requires --model"));
}

#[test]
fn parity_026_uses_configured_command_when_no_session_is_set() {
    let mut cfg = test_cfg();
    cfg.command = "other-agent --model {model} {brief}".to_string();
    let mut state = DispatchState::default();
    let mut req = pr_start_req("task-o", "do o", 1094);
    req.pr = None;
    let (outcome, _) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        None,
        None,
        true,
        1000,
        &cfg,
    );
    let StartOutcome::Started(spec) = outcome else {
        panic!("expected Started");
    };
    assert_eq!(&spec.argv[..3], &["other-agent", "--model", "sonnet"]);
}

#[test]
fn parity_027_resume_switches_with_active_session() {
    let cfg = test_cfg();
    let mut state = DispatchState::default();
    let codex_sess = Session {
        name: "codex-s".to_string(),
        provider: "codex".to_string(),
        model: "gpt-6-luna".to_string(),
    };
    let mut req = pr_start_req("task-switch", "do switch", 1094);
    req.pr = None;
    decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        Some(&codex_sess),
        None,
        true,
        1000,
        &cfg,
    );
    let r = state.runs.get_mut("task-switch").unwrap();
    r.status = RunStatus::Unfinished;
    r.finished = Some(1000);

    let claude_sess = Session {
        name: "claude-s".to_string(),
        provider: "claude".to_string(),
        model: "opus".to_string(),
    };
    let (_, actions) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        Some(&claude_sess),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
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
    assert_eq!(spec.provider, "claude");
    assert_eq!(spec.model, "opus");
}
