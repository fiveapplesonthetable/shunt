use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use tacho_daemon::FleetStore;

fn snapshot() -> Value {
    json!({"version":1, "source":"old-controller", "captured_at":100,
        "state":{"max_workers":0, "todo":[{"key":"queued","brief":"Preserve this task","after":["dependency"]}],
            "holds":{"group:ci":{"reason":"handoff","since":90}},
            "external_attention":{"decision":"Preserve this pending decision"}},
        "unmapped_active":[], "archive":{"opaque_history":["exact retained data"]}})
}

fn import(home: &Path, file: &Path, dry_run: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tacho"));
    command
        .args(["project", "import-state"])
        .arg(file)
        .env("TACHO_HOME", home)
        .env("TACHO_TEST_MODE", "1");
    if dry_run {
        command.arg("--dry-run");
    }
    command.output().unwrap()
}

#[test]
fn an_import_preserves_the_queue_pause_holds_and_original_archive() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let file = temp.path().join("snapshot.json");
    let original = snapshot();
    fs::write(&file, serde_json::to_vec(&original).unwrap()).unwrap();
    let result = import(&home, &file, false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let state = FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(state.max_workers, 0);
    assert_eq!(state.todo[0].key, "queued");
    assert_eq!(state.holds["group:ci"].reason, "handoff");
    let archive = Command::new("python3").args(["-c", "import sqlite3,sys; sys.stdout.buffer.write(sqlite3.connect(sys.argv[1]).execute('select snapshot from fleet_imports').fetchone()[0])"]).arg(home.join("events.sqlite")).output().unwrap();
    assert!(archive.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&archive.stdout).unwrap(),
        original
    );
}

#[test]
fn importing_the_same_snapshot_again_does_not_rewind_live_progress() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let file = temp.path().join("snapshot.json");
    fs::write(&file, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
    assert!(import(&home, &file, false).status.success());
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.todo.clear();
            state.max_workers = 2;
        })
        .unwrap();
    let before = store.load().unwrap();
    let result = import(&home, &file, false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(store.load().unwrap(), before);
    assert_eq!(store.recent_decisions(100, None, None).unwrap().len(), 1);
}

#[test]
fn a_new_snapshot_cannot_overwrite_an_initialized_controller() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let file = temp.path().join("snapshot.json");
    fs::write(&file, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store.mutate(None, |state| state.max_workers = 7).unwrap();
    let before = store.load().unwrap();
    assert!(!import(&home, &file, false).status.success());
    assert_eq!(store.load().unwrap(), before);
}

#[test]
fn unknown_state_fields_are_refused_instead_of_silently_discarded() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let file = temp.path().join("snapshot.json");
    let mut value = snapshot();
    value["state"]["future_control"] = json!({"held":true});
    fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
    let result = import(&home, &file, false);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("discard"));
    assert!(!home.join("events.sqlite").exists());
}

#[test]
fn an_import_dry_run_creates_no_controller_database() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let file = temp.path().join("snapshot.json");
    fs::write(&file, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
    let result = import(&home, &file, true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!home.join("events.sqlite").exists());
}

#[test]
fn an_operator_stop_records_a_terminal_outcome_and_a_hold() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.agents.insert(
                "worker".into(),
                tacho_core::decide::dispatch::AgentRun {
                    key: "worker".into(),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(["stop", "worker"])
        .env("TACHO_HOME", &home)
        .env("TACHO_TEST_MODE", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let state = store.load().unwrap();
    assert_eq!(
        state.agents["worker"].outcome,
        tacho_core::decide::dispatch::RunOutcome::Stopped
    );
    assert!(state.holds.contains_key("worker"));
}

#[test]
fn incomplete_proofs_and_unmapped_live_work_cannot_be_imported() {
    for unmapped in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let file = temp.path().join("snapshot.json");
        let mut value = snapshot();
        if unmapped {
            value["unmapped_active"] = json!(["unhandled gate"]);
        } else {
            value["state"]["passed"] = json!({"7":{"pr":7,"sha":"head","main_sha":"main","batch_id":"missing","passed_at":10,"tree":"tree","areas":["core"]}});
        }
        fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = import(&home, &file, false);
        assert!(!result.status.success());
        assert!(!home.join("events.sqlite").exists());
    }
}
