use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::json;
use tacho_core::decide::dispatch::PullFacts;
use tacho_daemon::FleetStore;

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn cli(repo: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(args)
        .current_dir(repo)
        .env("TACHO_HOME", home)
        .env("TACHO_TEST_MODE", "1")
        .env_remove("TACHO_GH_REPO")
        .env_remove("TACHO_NOTIFY")
        .output()
        .unwrap()
}

fn fixture(repo: &Path, mutate: bool) {
    fs::create_dir_all(repo.join("unrelated")).unwrap();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.name", "Developer"]);
    git(repo, &["config", "user.email", "dev@example.com"]);
    fs::write(repo.join("unrelated/value"), "before\n").unwrap();
    fs::write(repo.join("adapter.py"), format!(r#"import json, pathlib, sys
request = json.load(sys.stdin)
assert request['version'] == 1
assert request['kind'] == 'proof_plan'
assert request['files'] == ['unrelated/value']
assert request['base'] and request['head'] and request['tree']
if {mutate}:
    pathlib.Path('unrelated/value').write_text('a planner cannot edit source\n')
print(json.dumps([{{'name':'the project supplies the check', 'cmd':"test \"$(cat unrelated/value)\" = after", 'area':'opaque', 'units':[]}}]))
"#, mutate=if mutate { "True" } else { "False" })).unwrap();
    fs::write(repo.join("shunt.json"), serde_json::to_vec(&json!({
        "version": 1,
        "area_rules": [{"area":"opaque", "prefixes":["unrelated/"], "checks":[], "unit_prefixes":{}, "isolate_batch":false, "needs_screenshot":false}],
        "hooks": {"proof_plan":{"program":"python3", "args":["adapter.py"]}}
    })).unwrap()).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "The project defines its checks"]);
    fs::write(repo.join("unrelated/value"), "after\n").unwrap();
}

#[test]
fn a_project_hook_plans_real_checks_without_repository_assumptions() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&repo, false);
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store.mutate(None, |state| state.max_workers = 0).unwrap();
    let applied = cli(&repo, &home, &["project", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(store.load().unwrap().max_workers, 0);
    let proof = cli(&repo, &home, &["prove", "--base", "HEAD"]);
    assert!(
        proof.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&proof.stdout),
        String::from_utf8_lossy(&proof.stderr)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.join(".tachoproof/summary.json")).unwrap()).unwrap();
    assert_eq!(summary["ok"], true);
    assert_eq!(
        summary["checks"][0]["name"],
        "the project supplies the check"
    );
}

#[test]
fn a_project_planner_cannot_edit_the_inputs_it_claims_to_check() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&repo, true);
    let applied = cli(&repo, &home, &["project", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let proof = cli(&repo, &home, &["prove", "--base", "HEAD", "--dry-run"]);
    assert!(!proof.status.success());
    assert!(String::from_utf8_lossy(&proof.stderr).contains("planner changed"));
}

fn run_project_batch(repo: &Path, home: &Path, refuse: bool) -> Output {
    fixture(repo, false);
    let witness = home.join("hook-order.jsonl");
    fs::create_dir_all(home).unwrap();
    fs::write(
        repo.join("batch_adapter.py"),
        format!(
            r#"import json, os, pathlib, sys
request = json.load(sys.stdin)
kind = request['kind']
with pathlib.Path({witness}).open('a') as output:
    output.write(kind + '\n')
if kind == 'runner_trust':
    assert pathlib.Path('unrelated/value').read_text().strip() == 'after'
    if {refuse}:
        sys.exit('runner refused')
else:
    assert pathlib.Path.cwd() != pathlib.Path(request['repository_root'])
    assert pathlib.Path('unrelated/value').read_text().strip() == 'proposed'
    assert request['version'] == 1 and request['tree']
    assert request['payload']['operation'] == 'batch'
    if kind == 'prepare':
        print(json.dumps({{'environment': {{'PROJECT_VALUE': 'literal $HOME; $(false)'}}}}))
    else:
        assert os.environ['PROJECT_VALUE'] == 'literal $HOME; $(false)'
"#,
            witness = serde_json::to_string(&witness.to_string_lossy()).unwrap(),
            refuse = if refuse { "True" } else { "False" }
        ),
    )
    .unwrap();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.join("shunt.json")).unwrap()).unwrap();
    for kind in ["runner_trust", "prepare", "verify"] {
        config["hooks"][kind] = json!({"program":"python3", "args":["batch_adapter.py"]});
    }
    fs::write(
        repo.join("shunt.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(
        repo,
        &["commit", "-m", "The project defines its runtime hooks"],
    );
    git(repo, &["checkout", "-b", "proposal"]);
    fs::write(repo.join("unrelated/value"), "proposed\n").unwrap();
    if refuse {
        let script = fs::read_to_string(repo.join("batch_adapter.py"))
            .unwrap()
            .replace("if True:", "if False:");
        fs::write(repo.join("batch_adapter.py"), script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(repo.join(".hooks")).unwrap();
        let hook = repo.join(".hooks/post-merge");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf 'untrusted git hook\\n' >> '{}'\n",
                witness.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        git(repo, &["config", "core.hooksPath", ".hooks"]);
    }
    git(repo, &["add", "."]);
    git(
        repo,
        &["commit", "-m", "The proposal changes its own files"],
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
    git(repo, &["checkout", "main"]);
    let applied = cli(repo, home, &["project", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .mutate(None, |state| {
            state.open_prs_cache = vec![PullFacts {
                number: 1,
                branch: "proposal".into(),
                base: "main".into(),
                head,
                state: "OPEN".into(),
                mergeable: "MERGEABLE".into(),
                files: vec!["unrelated/value".into()],
                ..Default::default()
            }];
        })
        .unwrap();
    cli(repo, home, &["batch", "1", "--foreground"])
}

#[test]
fn a_batch_runs_trust_and_prepare_before_project_verification() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let result = run_project_batch(&repo, &home, false);
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(home.join("hook-order.jsonl")).unwrap(),
        "runner_trust\nprepare\nverify\n"
    );
    let state = FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .load()
        .unwrap();
    assert!(state.passed.contains_key(&1));
}

#[test]
fn a_proposal_cannot_replace_the_runner_admission_hook() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let result = run_project_batch(&repo, &home, true);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("runner refused"));
    assert_eq!(
        fs::read_to_string(home.join("hook-order.jsonl")).unwrap(),
        "runner_trust\n"
    );
    let state = FleetStore::open(&home.join("events.sqlite"))
        .unwrap()
        .load()
        .unwrap();
    assert!(!state.passed.contains_key(&1));
}

#[test]
fn an_invalid_project_version_preserves_the_live_configuration_and_pause() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fixture(&repo, false);
    let applied = cli(&repo, &home, &["project", "apply"]);
    assert!(applied.status.success());
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store.mutate(None, |state| state.max_workers = 0).unwrap();
    let before = store.load().unwrap();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.join("shunt.json")).unwrap()).unwrap();
    config["version"] = json!(999);
    fs::write(
        repo.join("shunt.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let refused = cli(&repo, &home, &["project", "apply"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unsupported project contract version")
    );
    assert_eq!(store.load().unwrap(), before);
}
