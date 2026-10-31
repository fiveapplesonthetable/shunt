use serde_json::json;
use std::{fs, process::Command};

#[test]
fn project_selection_checks_each_path_without_creating_controller_state() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("project.json");
    let paths = directory.path().join("paths.json");
    let home = directory.path().join("controller");
    fs::write(
        &config,
        json!({"version":1,"area_rules":[
            {"area":"secrets","always":true},
            {"area":"console","patterns":["^web/"]},
            {"area":"inventory","fallback_patterns":[".*"]}
        ]})
        .to_string(),
    )
    .unwrap();
    fs::write(&paths, json!(["web/home.ts", "unplaced"]).to_string()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_tacho"))
        .args(["project", "select"])
        .arg(&config)
        .arg("--files-file")
        .arg(&paths)
        .arg("--each")
        .env("TACHO_HOME", &home)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let actual: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        actual,
        json!({"web/home.ts":["console","secrets"],"unplaced":["inventory","secrets"]})
    );
    assert!(!home.exists());
}
