use super::*;

#[test]
fn terminal_runs_without_a_finish_timestamp_are_not_reaped_again() {
    for outcome in [
        dispatch::RunOutcome::Stopped,
        dispatch::RunOutcome::Abandoned,
        dispatch::RunOutcome::Completed,
    ] {
        let agent = AgentRun {
            key: "old-run".into(),
            started_at: 1,
            outcome,
            ..Default::default()
        };
        let state = DecisionState {
            agents: BTreeMap::from([("old-run".into(), agent.clone())]),
            ..Default::default()
        };
        let plan = decide(
            &state,
            &WorldObservation {
                now: 100,
                agent_log_tails: BTreeMap::from([("old-run".into(), "PR: #77".into())]),
                ..Default::default()
            },
        );
        assert!(plan.reaped_agents.is_empty());
        assert_eq!(plan.next_state.agents["old-run"], agent);
    }
}

#[test]
fn a_queue_row_cannot_restart_an_attempt_that_already_exists() {
    for outcome in [
        dispatch::RunOutcome::Stopped,
        dispatch::RunOutcome::Completed,
        dispatch::RunOutcome::Unspecified,
    ] {
        let state = DecisionState {
            max_parallel: 1,
            todo: vec![TodoItem {
                key: "existing".into(),
                brief: "Retained queue entry".into(),
                ..Default::default()
            }],
            agents: BTreeMap::from([(
                "existing".into(),
                AgentRun {
                    key: "existing".into(),
                    finished_at: Some(1),
                    outcome,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let plan = decide(
            &state,
            &WorldObservation {
                now: 100,
                ..Default::default()
            },
        );
        assert!(
            plan.dispatched_tasks.is_empty(),
            "duplicate dispatch for {outcome:?}"
        );
        assert_eq!(plan.next_state.todo.len(), 1);
    }
}

#[test]
fn a_new_retry_attempt_can_start_without_restarting_the_previous_attempt() {
    let state = DecisionState {
        max_parallel: 1,
        todo: vec![TodoItem {
            key: "existing-r1".into(),
            brief: "An explicitly queued retry".into(),
            ..Default::default()
        }],
        agents: BTreeMap::from([(
            "existing".into(),
            AgentRun {
                key: "existing".into(),
                finished_at: Some(1),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let plan = decide(
        &state,
        &WorldObservation {
            now: 100,
            ..Default::default()
        },
    );
    assert_eq!(plan.dispatched_tasks[0].key, "existing-r1");
}

#[test]
fn configured_pipeline_and_area_groups_preserve_batch_holds() {
    let mut value = serde_json::to_value(DecisionState::default()).unwrap();
    value["batch_grouping"] = serde_json::json!({"ci_group":"checks","ci_files":["^pipeline/"],
        "group_ignore":["secrets"],"groups":[{"name":"slow","when":{"superset_of":["runtime","ui"]}}],"fallback_group":"core"});
    value["holds"] = serde_json::json!({"group:checks":{"reason":"pipeline held","since":1},"group:slow":{"reason":"wide work held","since":1}});
    value["area_rules"] = serde_json::json!([
        {"area":"runtime","prefixes":[""],"checks":[]}, {"area":"ui","prefixes":[""],"checks":[]}
    ]);
    let state: DecisionState = serde_json::from_value(value).unwrap();
    let plan = decide(
        &state,
        &WorldObservation {
            now: 100,
            open_prs: vec![
                PullFacts {
                    number: 1,
                    state: "OPEN".into(),
                    head: "head-a".into(),
                    files: vec!["pipeline/check.toml".into()],
                    ..Default::default()
                },
                PullFacts {
                    number: 2,
                    state: "OPEN".into(),
                    head: "head-b".into(),
                    files: vec!["app/value.rs".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    );
    assert!(plan.batch_plans.is_empty());
}

#[test]
fn a_project_pipeline_batch_runs_alone_with_every_project_selected_area() {
    let state = DecisionState { batch_grouping: Some(serde_json::from_value(serde_json::json!({
        "ci_group":"checks","ci_files":["^pipeline/"],"group_ignore":[],"groups":[],"fallback_group":"work"
    })).unwrap()), area_rules: ["android-release", "custom-core"]
        .into_iter()
        .map(|area| AreaRule {
            area: area.into(),
            prefixes: vec!["".into()],
            ..Default::default()
        })
        .collect(), ..Default::default() };
    let pulls: Vec<_> = [1, 2]
        .into_iter()
        .map(|number| PullFacts {
            number,
            state: "OPEN".into(),
            head: format!("head-{number}"),
            files: vec!["pipeline/check.toml".into()],
            ..Default::default()
        })
        .collect();
    let plan = decide(
        &state,
        &WorldObservation {
            now: 100,
            open_prs: pulls,
            ..Default::default()
        },
    );
    assert_eq!(plan.batch_plans.len(), 1);
    let batch = &plan.batch_plans[0];
    assert_eq!(batch.group, "checks");
    assert_eq!(batch.prs, vec![1]);
    assert_eq!(batch.areas, vec!["android-release", "custom-core"]);
}

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
