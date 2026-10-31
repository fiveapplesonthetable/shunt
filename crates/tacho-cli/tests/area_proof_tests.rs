use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;
use tacho_core::decide::dispatch::PullFacts;
use tacho_daemon::FleetStore;

fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_string()
}

#[test]
fn separate_area_runs_combine_only_on_the_same_verified_source_tree() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Developer"]);
    git(&root, &["config", "user.email", "dev@example.com"]);
    fs::write(root.join("value"), "base\n").unwrap();
    let witness = temp.path().join("verified.jsonl");
    fs::write(root.join("hook.py"),format!("import json,sys\nr=json.load(sys.stdin)\nif r['kind']=='verify':\n with open({},'a') as f: f.write(json.dumps(r['areas'])+'\\n')\n",serde_json::to_string(&witness).unwrap())).unwrap();
    fs::write(root.join("shunt.json"),serde_json::to_vec(&json!({"version":1,"area_rules":[
        {"area":"quick","prefixes":[""],"checks":[]},{"area":"expensive","prefixes":[""],"checks":[]}],
        "hooks":{"runner_trust":{"program":"python3","args":["hook.py"]},"verify":{"program":"python3","args":["hook.py"]}}})).unwrap()).unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &["commit", "-m", "The project defines its verification"],
    );
    let base = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["update-ref", "refs/remotes/origin/main", &base]);
    git(&root, &["checkout", "-b", "proposal"]);
    fs::write(root.join("value"), "proposed\n").unwrap();
    git(&root, &["commit", "-am", "The value changes"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.max_workers = 0;
            state.open_prs_cache = vec![PullFacts {
                number: 7,
                state: "OPEN".into(),
                head: head.clone(),
                branch: "proposal".into(),
                files: vec!["value".into()],
                ..Default::default()
            }];
        })
        .unwrap();
    for args in [
        vec!["project", "apply"],
        vec!["batch", "--foreground", "7"],
        vec!["batch", "--foreground", "7"],
    ]
    .into_iter()
    .enumerate()
    {
        let (index, args) = args;
        let result = Command::new(env!("CARGO_BIN_EXE_tacho"))
            .args(args)
            .current_dir(&root)
            .env("TACHO_HOME", &home)
            .env("TACHO_TEST_MODE", "1")
            .env("TACHO_BATCH_ORIGIN", "manual")
            .env(
                "TACHO_BATCH_AREAS",
                if index == 1 { "quick" } else { "expensive" },
            )
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let state = store.load().unwrap();
    assert_eq!(
        state.passed[&7]
            .areas
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["quick".to_string(), "expensive".to_string()].into()
    );
    let rows: Vec<serde_json::Value> = fs::read_to_string(witness)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows, vec![json!(["quick"]), json!(["expensive"])]);
}
