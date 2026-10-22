//! Golden tests for install/uninstall hooks across Claude Code, Gemini CLI, and Jetski.

use std::fs;
use tachograph::cli::Provider;
use tachograph::install::{
    load_or_init_json, merge_provider_hooks, remove_provider_hooks, save_json,
};

#[test]
fn test_claude_empty_install_and_uninstall() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("settings.json");

    // 1. Initial install on empty / non-existent file
    let mut val = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Claude, &mut val);
    save_json(&config_path, &val).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("\"PreToolUse\""));
    assert!(content.contains("\"tacho hook claude\""));

    // 2. Idempotent install: installing again does not duplicate entries
    let mut val2 = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Claude, &mut val2);
    let arr = val2["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "Must not duplicate hook entries on re-install"
    );

    // 3. Uninstall
    let modified = remove_provider_hooks(Provider::Claude, &mut val2);
    assert!(modified);
    assert!(!val2.as_object().unwrap().contains_key("hooks"));
}

#[test]
fn test_preserve_unrelated_keys_and_formatting() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("settings.json");

    let initial = r#"{
  "theme": "dark",
  "apiKey": "sk-12345",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "custom_linter"
          }
        ]
      }
    ]
  },
  "telemetry": false
}"#;
    fs::write(&config_path, initial).unwrap();

    // 1. Install
    let mut val = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Claude, &mut val);
    save_json(&config_path, &val).unwrap();

    // Verify existing keys preserved
    assert_eq!(val["theme"], "dark");
    assert_eq!(val["apiKey"], "sk-12345");
    assert_eq!(val["telemetry"], false);

    // Verify existing custom hook preserved beside tacho
    let pre_tool_hooks = val["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_tool_hooks.len(), 2);
    assert_eq!(pre_tool_hooks[0]["hooks"][0]["command"], "custom_linter");
    assert_eq!(
        pre_tool_hooks[1]["hooks"][0]["command"],
        "tacho hook claude"
    );

    // 2. Uninstall: only removes tacho, leaves custom_linter untouched
    let modified = remove_provider_hooks(Provider::Claude, &mut val);
    assert!(modified);
    assert_eq!(val["theme"], "dark");
    assert_eq!(val["apiKey"], "sk-12345");
    assert_eq!(val["telemetry"], false);

    let pre_tool_after = val["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_tool_after.len(), 1);
    assert_eq!(pre_tool_after[0]["hooks"][0]["command"], "custom_linter");
}

#[test]
fn test_refuse_malformed_json() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("malformed.json");

    fs::write(&config_path, "{ \"incomplete\": true, ").unwrap();

    let res = load_or_init_json(&config_path);
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert!(err.contains("Invalid JSON"));
    assert!(err.contains("Refusing to modify malformed configuration file"));
}

#[test]
fn test_gemini_cli_install_and_uninstall() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("hooks.json");

    let mut val = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Gemini, &mut val);
    save_json(&config_path, &val).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("\"BeforeTool\""));
    assert!(content.contains("\"tacho hook gemini\""));

    // Idempotent re-install
    let mut val2 = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Gemini, &mut val2);
    let arr = val2["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(arr.len(), 1);

    // Uninstall
    let modified = remove_provider_hooks(Provider::Gemini, &mut val2);
    assert!(modified);
    assert!(!val2.as_object().unwrap().contains_key("hooks"));
}

#[test]
fn test_jetski_install_and_uninstall() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("hooks.json");

    let initial = r#"{
  "plugins": ["git", "memory"],
  "model": "gemini-2.5-pro"
}"#;
    fs::write(&config_path, initial).unwrap();

    // Install
    let mut val = load_or_init_json(&config_path).unwrap();
    merge_provider_hooks(Provider::Jetski, &mut val);
    save_json(&config_path, &val).unwrap();

    assert_eq!(val["plugins"][0], "git");
    assert_eq!(val["model"], "gemini-2.5-pro");
    assert!(val["tacho"]["PreToolUse"].is_array());
    assert_eq!(
        val["tacho"]["PreToolUse"][0]["hooks"][0]["command"],
        "tacho hook jetski"
    );

    // Uninstall
    let modified = remove_provider_hooks(Provider::Jetski, &mut val);
    assert!(modified);
    assert!(!val.as_object().unwrap().contains_key("tacho"));
    assert_eq!(val["plugins"][0], "git");
    assert_eq!(val["model"], "gemini-2.5-pro");
}

#[test]
fn test_concurrent_installs_processes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_dir = temp_dir.path().join(".claude");
    fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("settings.json");
    fs::write(&config_path, "{\n  \"user_setting\": 123\n}\n").unwrap();

    let tacho_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_tacho"));

    let mut procs = Vec::new();
    for _ in 0..10 {
        let mut cmd = std::process::Command::new(&tacho_bin);
        cmd.args(["install", "claude"]).current_dir(temp_dir.path());
        procs.push(cmd.spawn().expect("failed to spawn tacho install"));
    }

    for mut p in procs {
        let status = p.wait().expect("failed to wait on tacho install");
        assert!(status.success());
    }

    // Verify valid JSON
    let content = fs::read_to_string(&config_path).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).expect("Must be valid JSON");
    assert_eq!(val["user_setting"], 123);
    let arr = val["hooks"]["PreToolUse"]
        .as_array()
        .expect("Must have PreToolUse array");
    assert_eq!(
        arr.len(),
        1,
        "Must have exactly one tacho entry, not duplicated"
    );
}
