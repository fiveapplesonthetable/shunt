use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::json;
use tacho_core::decide::dispatch::PullFacts;
use tacho_daemon::FleetStore;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn cli(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(args)
        .current_dir(root)
        .env("TACHO_HOME", home)
        .env("TACHO_TEST_MODE", "1")
        .env_remove("TACHO_GH_REPO")
        .env_remove("TACHO_NOTIFY")
        .output()
        .unwrap()
}

fn fixture(root: &Path, home: &Path, alter_source: bool) {
    fs::create_dir_all(root.join("reports")).unwrap();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Developer"]);
    git(root, &["config", "user.email", "dev@example.com"]);
    fs::write(root.join("value"), "before\n").unwrap();
    fs::write(root.join("reports/plot.txt"), "before\n").unwrap();
    let command = format!(
        "printf 'after\\n' > reports/plot.txt; printf 'new\\n' > reports/new.txt{}",
        if alter_source {
            "; printf 'unexpected\\n' > new-source.rs"
        } else {
            ""
        }
    );
    let planner = format!(
        "import json,sys; json.load(sys.stdin); print({})",
        serde_json::to_string(
            &json!([{"name":"draw outputs", "area":"value", "cmd":command, "units":[]}])
                .to_string()
        )
        .unwrap()
    );
    let verifier = format!("import json,pathlib,subprocess,sys; json.load(sys.stdin); subprocess.run({},shell=True,check=True)", serde_json::to_string(&command).unwrap());
    let config = json!({"version":1, "area_rules":[{"area":"value", "prefixes":["value"], "checks":[]}],
    "generated_outputs":["^reports/.*\\.txt$"],
    "hooks":{
        "proof_plan":{"program":"python3", "args":["-c",planner]},
        "runner_trust":{"program":"python3", "args":["-c","import json,sys; json.load(sys.stdin)"]},
        "verify":{"program":"python3", "args":["-c",verifier]}
    }});
    fs::write(
        root.join("shunt.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "-m", "The project declares its generated reports"],
    );
    let output = cli(root, home, &["project", "apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(root.join("value"), "after\n").unwrap();
}

#[test]
fn generated_outputs_are_archived_without_replacing_worker_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&root, &home, false);
    let output = cli(&root, &home, &["prove", "--base", "HEAD"]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("reports/plot.txt")).unwrap(),
        "before\n"
    );
    assert!(!root.join("reports/new.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join(".tachoproof/artifacts/after/reports/plot.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        fs::read_to_string(root.join(".tachoproof/artifacts/after/reports/new.txt")).unwrap(),
        "new\n"
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join(".tachoproof/artifacts/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["files"].as_array().unwrap().len(), 2);
    assert_eq!(git(&root, &["diff", "--name-only"]), "value");
}

#[test]
fn output_collection_does_not_hide_untracked_source_edits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&root, &home, true);
    let output = cli(&root, &home, &["prove", "--base", "HEAD"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("new-source.rs"));
}

#[test]
fn listed_reference_files_remain_inputs_inside_generated_output_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&root, &home, false);
    fs::write(
        root.join("report-inputs.txt"),
        "# Required reference\nfixed.txt\n",
    )
    .unwrap();
    fs::write(root.join("reports/fixed.txt"), "required reference\n").unwrap();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("shunt.json")).unwrap()).unwrap();
    config["input_lists"] = json!([{"file":"report-inputs.txt", "root":"reports"}]);
    let checks = json!([{"name":"a reference is an input", "area":"value", "cmd":"printf 'changed reference\\n' > reports/fixed.txt", "units":[]}]);
    let script = format!(
        "import json,sys; json.load(sys.stdin); print({})",
        serde_json::to_string(&checks.to_string()).unwrap()
    );
    config["hooks"]["proof_plan"]["args"] = json!(["-c", script]);
    fs::write(
        root.join("shunt.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let applied = cli(&root, &home, &["project", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let output = cli(&root, &home, &["prove", "--base", "HEAD"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reports/fixed.txt"));
}

fn batch_outputs(alter_source: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&root, &home, alter_source);
    git(&root, &["checkout", "-b", "proposal"]);
    git(&root, &["commit", "-am", "The proposal changes its input"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "main"]);
    FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .mutate(None, |state| {
            state.open_prs_cache = vec![PullFacts {
                number: 1,
                branch: "proposal".into(),
                base: "main".into(),
                head: head.clone(),
                state: "OPEN".into(),
                mergeable: "MERGEABLE".into(),
                files: vec!["value".into()],
                ..Default::default()
            }];
        })
        .unwrap();
    let output = cli(&root, &home, &["batch", "1", "--foreground"]);
    assert_eq!(
        output.status.success(),
        !alter_source,
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state = FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(git(&root, &["rev-parse", "proposal"]), head);
    if !alter_source {
        assert_eq!(state.passed[&1].sha, head);
        let path = home
            .join("runs")
            .join(&state.batches[0].id)
            .join("artifacts/r0/manifest.json");
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            manifest["input_tree"],
            state.batches[0].tree.clone().unwrap()
        );
        assert_eq!(manifest["files"].as_array().unwrap().len(), 2);
    } else {
        assert!(!state.passed.contains_key(&1));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("tracked files")
                || String::from_utf8_lossy(&output.stderr).contains("source")
        );
    }
}

#[test]
fn a_batch_keeps_its_tested_heads_when_it_collects_generated_outputs() {
    batch_outputs(false);
}

#[test]
fn a_batch_cannot_certify_new_untracked_source_files() {
    batch_outputs(true);
}
