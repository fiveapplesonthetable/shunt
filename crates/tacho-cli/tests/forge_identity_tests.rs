use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

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
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn unavailable_canonical_refs_cannot_be_replaced_with_local_or_cached_heads() {
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
    let main = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-b", "proposal"]);
    fs::write(root.join("value"), "proposal\n").unwrap();
    git(&root, &["commit", "-am", "The value changes"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "main"]);
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    store.mutate(None, |state| {
        let project = serde_json::from_value::<tacho_core::project::ProjectConfig>(json!({
            "version":1,"repository":"owner/canonical","area_rules":[{"area":"opaque","prefixes":[""]}]
        })).unwrap();
        state.area_rules = project.area_rules.clone();
        state.project = Some(project);
        state.open_prs_cache = vec![PullFacts {
            number:7, state:"OPEN".into(), head:head.clone(), branch:"proposal".into(),
            files:vec!["value".into()], ..Default::default()
        }];
    }).unwrap();
    let gh = bin.join("gh");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    for script in [
        format!("#!/bin/sh\nif [ \"$1\" = api ]; then exit 1; else printf '%s\\n' '{{\"headRefOid\":\"{head}\"}}'; fi\n"),
        format!("#!/bin/sh\nif [ \"$1\" = api ]; then printf '%s\\n' '{{\"object\":{{\"sha\":\"{main}\"}}}}'; else printf '%s\\n' '{{}}'; fi\n"),
    ] {
        fs::write(&gh, script).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_tacho"))
            .args(["batch", "--foreground", "--cmd", "true", "7"])
            .env("PATH", paths).env("TACHO_HOME", &home).env("TACHO_TEST_MODE", "1")
            .env_remove("TACHO_BATCH_CLAIMED").env_remove("TACHO_BATCH_AREAS")
            .current_dir(&root).output().unwrap();
        assert!(!result.status.success(), "missing forge identities passed: {}", String::from_utf8_lossy(&result.stdout));
        assert!(store.load().unwrap().passed.is_empty());
    }
}
