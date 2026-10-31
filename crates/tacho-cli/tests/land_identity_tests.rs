use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::land::TrainRecord;
use tacho_daemon::FleetStore;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn disabled_merging_cannot_record_an_unmerged_train_as_landed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let home = temporary.path().join("home");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Developer"]);
    git(&root, &["config", "user.email", "dev@example.com"]);
    fs::write(root.join("value"), "base\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Main has a value"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let train = TrainRecord {
        id: "t001".into(),
        prs: vec![7],
        shas: [(7, head.clone())].into(),
        base: head.clone(),
        tree,
        status: "open".into(),
        ..Default::default()
    };
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |state| {
            state.trains = vec![train.clone()];
            state.open_prs_cache = vec![PullFacts {
                number: 7,
                head: head.clone(),
                state: "OPEN".into(),
                ..Default::default()
            }];
        })
        .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(["land", "t001"])
        .env("TACHO_HOME", &home)
        .env("TACHO_LAND_MERGE", "0")
        .env("TACHO_TEST_MODE", "1")
        .env_remove("TACHO_GH_REPO")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let state = store.load().unwrap();
    assert_eq!(state.trains, vec![train]);
    assert!(state.merged_prs.is_empty());
}

fn merge_case(mode: &str) {
    use serde_json::json;
    use tacho_core::decide::verify::{AreaCheck, AreaRule, BatchRun, PassedRecord};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let home = temporary.path().join("home");
    let bin = temporary.path().join("bin");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&bin).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Developer"]);
    git(&root, &["config", "user.email", "dev@example.com"]);
    fs::write(root.join("value"), "base\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Main has a value"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-b", "proposal"]);
    fs::write(root.join("value"), "proposal\n").unwrap();
    git(&root, &["commit", "-am", "The value changes"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    git(&root, &["checkout", "main"]);
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store.mutate(None, |state| {
        let project = serde_json::from_value::<tacho_core::project::ProjectConfig>(json!({"version":1,"repository":"owner/canonical","area_rules":[{"area":"opaque","prefixes":[""]}]})).unwrap();
        state.area_rules = vec![AreaRule { area:"opaque".into(),prefixes:vec!["".into()],..Default::default() }];
        state.project = Some(project);
        state.open_prs_cache = vec![PullFacts { number:7,head:head.clone(),state:"OPEN".into(),files:vec!["value".into()],..Default::default() }];
        state.batches = vec![BatchRun { id:"b001".into(),prs:vec![7],shas:[(7,head.clone())].into(),main_sha:base.clone(),tree:Some(tree.clone()),status:"passed".into(),areas:vec!["opaque".into()],checks:vec![AreaCheck{area:"opaque".into(),units:vec!["all".into()]}],finished_at:Some(1),..Default::default() }];
        state.passed.insert(7,PassedRecord {pr:7,sha:head.clone(),main_sha:base.clone(),batch_id:"b001".into(),tree:tree.clone(),areas:vec!["opaque".into()],..Default::default()});
        state.trains = vec![TrainRecord {id:"t001".into(),prs:vec![7],shas:[(7,head.clone())].into(),base:base.clone(),tree:tree.clone(),status:"open".into(),batch_id:"b001".into(),..Default::default()}];
    }).unwrap();
    let model_path = bin.join("forge.json");
    let model =
        json!({"mode":mode,"base":base,"head":head,"tree":tree,"merged":false,"attempts":[]});
    fs::write(&model_path, serde_json::to_vec(&model).unwrap()).unwrap();
    let gh = bin.join("gh");
    fs::write(&gh,r#"#!/usr/bin/python3
import json,sys
from pathlib import Path
p=Path(__file__).with_name('forge.json');v=json.loads(p.read_text());a=sys.argv[1:]
if a[0]=='api':
 if a[1].endswith('/git/ref/heads/main'):
  print(json.dumps({'object':{'sha':'e'*40 if v['mode']=='main' else 'f'*40 if v['merged'] else v['base']}}))
 elif '/git/commits/' in a[1]:
  print(json.dumps({} if v['mode']=='tree' else {'tree':{'sha':v['tree']}}))
 else: raise SystemExit(24)
elif a[:2]==['pr','view']:
 print(json.dumps({'headRefOid':'e'*40 if v['mode']=='head' else v['head'],'baseRefName':'release' if v['mode']=='base' else 'main','state':'MERGED' if v['merged'] else 'OPEN','isDraft':False,'mergeCommit':{'oid':'f'*40} if v['merged'] else None}))
elif a[:2]==['pr','merge']:
 v['attempts'].append(a);p.write_text(json.dumps(v))
 if '--match-head-commit' not in a or a[a.index('--match-head-commit')+1]!=v['head']: raise SystemExit(25)
 v['merged']=True;p.write_text(json.dumps(v))
else: raise SystemExit(26)
"#).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let land = || {
        Command::new(env!("CARGO_BIN_EXE_tacho"))
            .args(["land", "t001"])
            .env("PATH", &paths)
            .env("TACHO_HOME", &home)
            .env("TACHO_LAND_MERGE", "1")
            .env("TACHO_TEST_MODE", "1")
            .env_remove("TACHO_GH_REPO")
            .current_dir(&root)
            .output()
            .unwrap()
    };
    let result = land();
    let mut observed: serde_json::Value =
        serde_json::from_slice(&fs::read(&model_path).unwrap()).unwrap();
    let state = store.load().unwrap();
    if mode == "success" {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(state.trains.first().unwrap().status, "landed");
        assert!(state.merged_prs.contains(&7));
        assert_eq!(observed["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]),
            base,
            "the local checkout is not evidence of the remote merge"
        );
    } else if mode == "tree" {
        assert!(!result.status.success());
        assert_eq!(state.trains.first().unwrap().status, "open");
        assert_eq!(state.trains.first().unwrap().landed_prs, vec![7]);
        observed["mode"] = json!("success");
        fs::write(&model_path, serde_json::to_vec(&observed).unwrap()).unwrap();
        let resumed = land();
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(&model_path).unwrap()).unwrap();
        assert_eq!(
            after["attempts"].as_array().unwrap().len(),
            1,
            "a confirmed prefix is never merged twice"
        );
        assert_eq!(
            store.load().unwrap().trains.first().unwrap().status,
            "landed"
        );
    } else {
        assert!(!result.status.success(), "{mode} moved without refusal");
        assert!(
            observed["attempts"].as_array().unwrap().is_empty(),
            "{mode} reached the merge API"
        );
        assert_eq!(state.trains.first().unwrap().status, "open");
        assert!(state.merged_prs.is_empty());
    }
}

#[test]
fn moved_main_head_or_base_is_refused_before_a_merge_request() {
    for mode in ["main", "head", "base"] {
        merge_case(mode);
    }
}

#[test]
fn the_confirmed_forge_tree_certifies_an_exact_head_merge() {
    merge_case("success");
}

#[test]
fn a_missing_remote_tree_keeps_the_train_open_and_resumes_its_confirmed_prefix() {
    merge_case("tree");
}
