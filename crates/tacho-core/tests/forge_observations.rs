use serde_json::json;
use tacho_core::decide::plugin::{parse_gh_pr_list, CliForgePlugin, ForgePlugin};

#[test]
fn branch_heads_come_from_the_configured_forge_repository() {
    let forge = CliForgePlugin::new("owner/canonical");
    let sha = "a".repeat(40);
    let head = forge
        .branch_head("main", &mut |argv| {
            assert!(argv
                .iter()
                .any(|arg| arg == "repos/owner/canonical/git/ref/heads/main"));
            Ok(json!({"object":{"sha":sha}}).to_string())
        })
        .unwrap();
    assert_eq!(head, Some(sha));
    assert!(forge
        .branch_head("main", &mut |_| Ok(
            json!({"object":{"sha":"abbreviated"}}).to_string()
        ))
        .is_err());
}

#[test]
fn a_disappeared_pull_requires_an_explicit_terminal_state() {
    use tacho_core::decide::plugin::PullDisposition;
    let forge = CliForgePlugin::new("owner/canonical");
    for (state, expected) in [
        ("OPEN", PullDisposition::Open),
        ("MERGED", PullDisposition::Merged),
        ("CLOSED", PullDisposition::Closed),
    ] {
        assert_eq!(
            forge
                .pull_disposition(7, &mut |argv| {
                    assert!(argv
                        .windows(2)
                        .any(|pair| pair == ["--repo", "owner/canonical"]));
                    Ok(json!({"state":state}).to_string())
                })
                .unwrap(),
            expected
        );
    }
    assert!(forge.pull_disposition(7, &mut |_| Ok("{}".into())).is_err());
}

#[test]
fn an_open_pull_observation_preserves_the_files_that_select_its_checks() {
    let forge = CliForgePlugin::new("owner/project");
    let argv = forge.plan_open_prs_cmd();
    let fields = argv.windows(2).find_map(|pair| {
        (pair.first().map(String::as_str) == Some("--json"))
            .then(|| pair.get(1))
            .flatten()
    });
    assert!(fields.is_some_and(|fields| fields.split(',').any(|field| field == "files")));
    assert!(fields.is_some_and(|fields| fields.split(',').any(|field| field == "changedFiles")));
    let pulls = parse_gh_pr_list(r#"[{"number":7,"headRefOid":"head","files":[{"path":"src/lib.rs"},{"path":"README.md"}]}]"#)
        .expect("valid forge output");
    assert_eq!(
        pulls.first().map(|pull| &pull.files),
        Some(&vec!["src/lib.rs".to_string(), "README.md".to_string()])
    );
}

#[test]
fn a_partial_forge_file_list_cannot_select_an_incomplete_proof() {
    let result = parse_gh_pr_list(
        r#"[{"number":7,"headRefOid":"head","changedFiles":2,"files":[{"path":"README.md"}]}]"#,
    );
    assert!(result.is_err());
}

fn observe_large_pull(
    head_after: &str,
    count_after: usize,
    duplicate: bool,
) -> Result<Vec<tacho_core::decide::dispatch::PullFacts>, String> {
    let forge = CliForgePlugin::new("owner/project");
    let mut calls = 0;
    let observed = forge.open_prs(&mut |argv| {
        calls += 1;
        match calls {
            1 => Ok(json!([{"number":7,"headRefOid":"input-head","changedFiles":101,
                "files":(0..100).map(|index| json!({"path":format!("src/file-{index}")})).collect::<Vec<_>>()}]).to_string()),
            2 => {
                assert!(argv.iter().any(|arg| arg == "--paginate"));
                assert!(!argv.iter().any(|arg| arg == "--slurp"));
                assert!(argv.iter().any(|arg| arg == "repos/owner/project/pulls/7/files?per_page=100"));
                let first: Vec<_> = (0..100).map(|index| json!({"filename":format!("src/file-{index}")})).collect();
                Ok(format!("{}\n{}",json!(first),json!([{"filename":if duplicate {"src/file-0"} else {"schema/last-file.sql"}}])))
            }
            3 => Ok(json!({"headRefOid":head_after,"changedFiles":count_after}).to_string()),
            _ => panic!("unexpected forge query"),
        }
    });
    if observed.is_ok() {
        assert_eq!(calls, 3);
    }
    observed
}

#[test]
fn a_large_pull_includes_files_beyond_the_graphql_page_limit() {
    let pulls = observe_large_pull("input-head", 101, false).unwrap();
    assert_eq!(pulls[0].files.len(), 101);
    assert!(pulls[0].files.contains(&"schema/last-file.sql".to_string()));
}

#[test]
fn a_pull_that_moves_during_file_pagination_cannot_select_checks() {
    assert!(observe_large_pull("new-head", 101, false).is_err());
    assert!(observe_large_pull("input-head", 102, false).is_err());
}

#[test]
fn duplicate_paginated_files_cannot_disguise_missing_paths() {
    assert!(observe_large_pull("input-head", 101, true).is_err());
}
