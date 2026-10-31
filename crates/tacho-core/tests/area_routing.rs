use serde_json::json;
use tacho_core::decide::verify::{areas_for_files, checks_for_files, proof_plan, AreaRule};
use tacho_core::project::ProjectConfig;

fn rules() -> Vec<AreaRule> {
    serde_json::from_value(json!([
        {"area":"secrets","always":true,"patterns":["^docs/"],"checks":[["credentials","check-credentials"]]},
        {"area":"console","patterns":["^web/.*\\.(ts|css)$"],"fallback_patterns":["^scripts/"],"checks":[["browser","check-browser"]]},
        {"area":"runner","prefixes":["tools/runner/"],"fallback_patterns":["^scripts/"]},
        {"area":"inventory","fallback_patterns":[".*"]}
    ])).unwrap()
}

fn selected(paths: &[&str]) -> Vec<String> {
    areas_for_files(
        &paths
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>(),
        &rules(),
    )
}

#[test]
fn known_paths_select_regex_and_prefix_areas_without_the_unknown_fallback() {
    assert_eq!(selected(&["web/home.ts"]), vec!["console", "secrets"]);
    assert_eq!(
        selected(&["tools/runner/start.py"]),
        vec!["runner", "secrets"]
    );
    assert_eq!(selected(&["docs/guide.md"]), vec!["secrets"]);
    assert!(selected(&[]).is_empty());
}

#[test]
fn mandatory_checks_do_not_hide_unknown_input_paths() {
    assert_eq!(selected(&["new-build-input"]), vec!["inventory", "secrets"]);
    assert_eq!(
        selected(&["scripts/new-check.sh"]),
        vec!["console", "inventory", "runner", "secrets"]
    );
    assert_eq!(
        selected(&["web/home.ts", "new-build-input"]),
        vec!["console", "inventory", "secrets"]
    );
}

#[test]
fn proof_commands_and_structured_checks_use_the_same_routing() {
    let paths = vec!["web/home.ts".to_string()];
    let plan = proof_plan(&paths, &rules());
    assert!(plan.iter().any(|(_, command)| command == "check-browser"));
    assert!(plan
        .iter()
        .any(|(_, command)| command == "check-credentials"));
    let areas: Vec<_> = checks_for_files(&paths, &rules())
        .into_iter()
        .map(|check| check.area)
        .collect();
    assert_eq!(areas, selected(&["web/home.ts"]));
}

#[test]
fn malformed_path_or_fallback_patterns_are_refused_before_installation() {
    for field in ["patterns", "fallback_patterns", "required_patterns"] {
        let mut rule = json!({"area":"console","prefixes":["web/"]});
        rule[field] = json!(["["]);
        let project: ProjectConfig =
            serde_json::from_value(json!({"version":1,"area_rules":[rule]})).unwrap();
        assert!(project.validate().is_err(), "{field}");
    }
}

#[test]
fn required_path_checks_keep_unknown_input_fallbacks() {
    let configured: Vec<AreaRule> = serde_json::from_value(json!([
        {"area":"screens","required_patterns":["screenshots/"]},
        {"area":"inventory","fallback_patterns":[".*"]},
        {"area":"source","prefixes":["web/"]}
    ]))
    .unwrap();
    assert_eq!(
        areas_for_files(&["scripts/screenshots/new.py".into()], &configured),
        vec!["inventory", "screens"]
    );
    assert_eq!(
        areas_for_files(&["web/screenshots/home.png".into()], &configured),
        vec!["screens", "source"]
    );
}
