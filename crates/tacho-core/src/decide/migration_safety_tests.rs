use super::*;

#[test]
fn imported_stopped_work_does_not_schedule_an_automatic_retry() {
    let agent: dispatch::AgentRun = serde_json::from_value(serde_json::json!({
        "key":"work", "finished_at":1, "exit_code":1, "outcome":"stopped"
    }))
    .unwrap();
    let agents = BTreeMap::from([("work".into(), agent)]);
    assert!(!dispatch::needs_resume(
        &agents,
        &[],
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        "work",
        10_000
    ));
}

#[test]
fn imported_completed_work_can_satisfy_a_dependency_without_inventing_a_pr() {
    let agent: dispatch::AgentRun = serde_json::from_value(serde_json::json!({
        "key":"work", "finished_at":1, "outcome":"completed"
    }))
    .unwrap();
    let agents = BTreeMap::from([("work".into(), agent)]);
    assert!(dispatch::done_keys(&agents, &BTreeSet::new()).contains("work"));
}

#[test]
fn imported_no_pr_reviews_do_not_satisfy_dependencies_until_confirmed() {
    let agent: dispatch::AgentRun = serde_json::from_value(serde_json::json!({
        "key":"work", "finished_at":1, "exit_code":0, "pr_none":true, "outcome":"awaiting_pr_review"
    }))
    .unwrap();
    let agents = BTreeMap::from([("work".into(), agent.clone())]);
    assert!(!dispatch::done_keys(&agents, &BTreeSet::new()).contains("work"));
    assert!(dispatch::pr_problem(&agent, Some(0)).is_some());
}

#[test]
fn a_project_can_require_confirmation_of_new_no_pr_reports() {
    let mut state = DecisionState {
        max_parallel: 0,
        review_no_pr: true,
        ..Default::default()
    };
    state.agents.insert(
        "worker".into(),
        dispatch::AgentRun {
            key: "worker".into(),
            started_at: 1,
            ..Default::default()
        },
    );
    let plan = decide(
        &state,
        &WorldObservation {
            now: 100,
            agent_log_tails: BTreeMap::from([("worker".into(), "PR: none".into())]),
            ..Default::default()
        },
    );
    assert_eq!(
        plan.next_state.agents["worker"].outcome,
        dispatch::RunOutcome::AwaitingPrReview
    );
    assert!(!dispatch::done_keys(&plan.next_state.agents, &BTreeSet::new()).contains("worker"));
}

#[test]
fn project_defaults_do_not_reopen_an_explicit_zero_worker_limit() {
    let mut state = DecisionState {
        max_parallel: 0,
        ..Default::default()
    };
    ProjectPolicy::default().apply_to_state(&mut state);
    assert_eq!(state.max_parallel, 0);
}

#[test]
fn zero_worker_slots_never_dispatch_queued_work() {
    let state = DecisionState {
        max_parallel: 0,
        todo: vec![TodoItem {
            key: "queued-task".into(),
            brief: "A task held by admission".into(),
            area: "core".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let plan = decide(
        &state,
        &WorldObservation {
            now: 100,
            ..Default::default()
        },
    );
    assert!(plan.dispatched_tasks.is_empty());
    assert_eq!(plan.next_state.max_parallel, 0);
}

#[test]
fn an_observed_train_never_mails_without_current_member_proofs() {
    let observation = WorldObservation {
        now: 100,
        ready_trains: vec![(
            "train".into(),
            vec![7],
            BTreeMap::from([(7, "head".into())]),
        )],
        ..Default::default()
    };
    let plan = decide(&DecisionState::default(), &observation);
    assert!(!plan
        .owner_notifications
        .iter()
        .any(|notification| notification.kind == "ready-train"));
}
