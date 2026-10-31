use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::{AreaCheck, AreaRule, BatchRun, PassedRecord};
use tacho_core::decide::{decide, DecisionState, WorldObservation};

fn fixture() -> (DecisionState, WorldObservation) {
    let batch = BatchRun {
        id: "b001".into(),
        prs: vec![7, 8],
        shas: [(7, "a".repeat(40)), (8, "b".repeat(40))].into(),
        main_sha: "c".repeat(40),
        tree: Some("d".repeat(40)),
        areas: vec!["runtime".into()],
        checks: vec![AreaCheck {
            area: "runtime".into(),
            units: vec!["all".into()],
        }],
        status: "passed".into(),
        finished_at: Some(10),
        ..Default::default()
    };
    let passed = batch
        .shas
        .iter()
        .map(|(number, head)| {
            (
                *number,
                PassedRecord {
                    pr: *number,
                    sha: head.clone(),
                    main_sha: batch.main_sha.clone(),
                    tree: batch.tree.clone().unwrap(),
                    batch_id: batch.id.clone(),
                    areas: batch.areas.clone(),
                    ..Default::default()
                },
            )
        })
        .collect();
    let open_prs = batch
        .shas
        .iter()
        .map(|(number, head)| PullFacts {
            number: *number,
            head: head.clone(),
            state: "OPEN".into(),
            draft: true,
            files: vec!["src/lib.rs".into()],
            mergeable: "MERGEABLE".into(),
            ..Default::default()
        })
        .collect();
    (
        DecisionState {
            batches: vec![batch.clone()],
            passed,
            area_rules: vec![AreaRule {
                area: "runtime".into(),
                prefixes: vec!["src/".into()],
                ..Default::default()
            }],
            last_tick_ts: Some(10),
            ..Default::default()
        },
        WorldObservation {
            now: 11,
            main_sha: batch.main_sha,
            open_prs,
            ..Default::default()
        },
    )
}

#[test]
fn a_complete_passed_draft_batch_gets_its_owner_train_without_manual_setup() {
    let (state, world) = fixture();
    let plan = decide(&state, &world);
    assert_eq!(plan.ready_prs.len(), 2);
    assert_eq!(plan.ready_trains.len(), 1);
    let ready = plan.ready_trains.first().unwrap();
    assert_eq!(ready.train_id, "batch-b001");
    assert_eq!(ready.order, vec![7, 8]);
    assert!(ready.announce);
    assert!(plan
        .owner_notifications
        .iter()
        .any(|notice| notice.ready.is_some()));
}

#[test]
fn an_incomplete_member_proof_cannot_publish_a_complete_train() {
    let (mut state, world) = fixture();
    state.passed.get_mut(&8).unwrap().areas.clear();
    let plan = decide(&state, &world);
    assert!(plan.ready_trains.is_empty());
    assert!(!plan
        .owner_notifications
        .iter()
        .any(|notice| notice.ready.is_some()));
}
