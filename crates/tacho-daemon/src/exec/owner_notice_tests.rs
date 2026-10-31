use super::*;
use crate::fleet::DaemonHands;
use std::{fs, path::Path};
use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::{AreaCheck, AreaRule, BatchRun, PassedRecord};

fn fixture(home: &Path, mode: &str) -> FleetStore {
    let root = home.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(home.join("mode"), mode).unwrap();
    fs::write(
        root.join("hook.py"),
        r#"import json,pathlib,sys
r=json.load(sys.stdin);home=pathlib.Path(sys.argv[1])
with (home/(r['kind']+'.jsonl')).open('a') as output: output.write(json.dumps(r)+'\n')
if r['kind']=='publish' and (home/'fail-publication').exists(): sys.exit(1)
if r['kind']=='notify': print(json.dumps({'delivery':(home/'mode').read_text()}))
"#,
    )
    .unwrap();
    let command = serde_json::json!({"program":"python3","args":[root.join("hook.py"),home]});
    let project=serde_json::from_value(serde_json::json!({"version":1,"area_rules":[{"area":"runtime","prefixes":["src/"]}],"hooks":{"publish":command,"notify":command}})).unwrap();
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            crate::project::apply(state, project, &root);
            state.max_workers = 0;
            state.last_tick_ts = Some(10);
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
            state.area_rules = vec![AreaRule {
                area: "runtime".into(),
                prefixes: vec!["src/".into()],
                ..Default::default()
            }];
            state.main_sha = Some(batch.main_sha.clone());
            state.open_prs_cache = batch
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
            state.passed = batch
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
            state.batches = vec![batch];
        })
        .unwrap();
    store
}

fn try_tick(home: &Path, store: &mut FleetStore, now: i64) -> io::Result<TickOutcome> {
    let live = tacho_core::LiveState::default();
    let mut hands = DaemonHands {
        home: home.to_path_buf(),
        live_state: live.clone(),
        appender: None,
        launcher: None,
    };
    let mut options = TickOptions::new(&live);
    options.procs = Some(Vec::new());
    options.now_override = Some(now);
    options.explicit_main_sha = Some("c".repeat(40));
    run_tick(home, store, &mut hands, options)
}

fn tick(home: &Path, store: &mut FleetStore, now: i64) -> TickOutcome {
    try_tick(home, store, now).unwrap()
}

fn requests(home: &Path, kind: &str) -> Vec<serde_json::Value> {
    fs::read_to_string(home.join(format!("{kind}.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn pending_delivery_retries_and_queue_publication_keeps_exact_proof_inputs() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let mut store = fixture(home, "pending");
    tick(home, &mut store, 11);
    assert!(store.load().unwrap().notified_failures.is_empty());
    assert_eq!(
        store.load().unwrap().trains.first().unwrap().id,
        "batch-b001"
    );
    fs::write(home.join("mode"), "sent").unwrap();
    tick(home, &mut store, 12);
    tick(home, &mut store, 13);
    assert_eq!(requests(home, "publish").len(), 1);
    let notices = requests(home, "notify");
    assert_eq!(notices.len(), 2);
    assert_eq!(
        notices.first().unwrap()["payload"]["ready"]["heads"]["7"],
        "a".repeat(40)
    );
    assert!(!store.load().unwrap().notified_failures.is_empty());
}

#[test]
fn a_crash_before_notification_io_does_not_record_delivery() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let mut store = fixture(home, "sent");
    let observed = store.load().unwrap();
    let decision_state = observe::decision_state_of(&observed, 11);
    let world = WorldObservation {
        now: 11,
        main_sha: "c".repeat(40),
        open_prs: observed.open_prs_cache.clone(),
        ..Default::default()
    };
    let plan = tacho_core::decide::decide(&decision_state, &world);
    persist_plan_state(&mut store, &observed, &decision_state, &world, &plan, 11).unwrap();
    assert!(store.load().unwrap().notified_failures.is_empty());
    tick(home, &mut store, 12);
    assert_eq!(requests(home, "notify").len(), 1);
}

#[test]
fn uncertain_delivery_is_visible_without_recursively_announcing_its_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let mut store = fixture(home, "uncertain");
    tick(home, &mut store, 11);
    let second = tick(home, &mut store, 12);
    assert_eq!(second.plan.owner_notifications.len(), 1);
    assert_eq!(
        second.plan.owner_notifications.first().unwrap().kind,
        "ready-train"
    );
    assert!(second
        .plan
        .attention_items
        .iter()
        .any(|item| item.kind == "notification-delivery"));
    assert_eq!(store.load().unwrap().external_attention.len(), 1);
    fs::write(home.join("mode"), "sent").unwrap();
    tick(home, &mut store, 13);
    assert!(store.load().unwrap().external_attention.is_empty());
}

#[test]
fn a_failed_queue_publication_retries_before_any_ready_email() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let mut store = fixture(home, "sent");
    fs::write(home.join("fail-publication"), "fail").unwrap();
    assert!(try_tick(home, &mut store, 11).is_err());
    assert!(requests(home, "notify").is_empty());
    assert!(store.load().unwrap().owner_queue_digest.is_none());
    fs::remove_file(home.join("fail-publication")).unwrap();
    tick(home, &mut store, 12);
    assert_eq!(requests(home, "publish").len(), 2);
    assert_eq!(requests(home, "notify").len(), 1);
}
