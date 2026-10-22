//! Test 7: Restart keeps tracks.
//!
//! Verifies: append, stop, start, append; one track per key, same UUIDs.

use std::collections::{HashMap, HashSet};
use tacho_core::{Input, SessionKey, ToolInput};
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_restart_keeps_tracks() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    let session = SessionKey::new("gemini", "session-restart-777");

    // Phase 1: Initial daemon start and append
    let mut initial_tracks: HashMap<String, u64> = HashMap::new();
    {
        let daemon = Daemon::start(paths.clone(), config.clone()).unwrap();

        daemon
            .appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(1_000_000),
                    provider: "gemini".to_string(),
                    session_id: "session-restart-777".to_string(),
                    agent: Some("agent-restart-alpha".to_string()),
                    task: None,
                    pid: Some(4001),
                    cwd: Some("/repo".to_string()),
                    model: Some("gemini-1.5-pro".to_string()),
                    parent_session: None,
                    git_tree: None,
                },
                1_000_000,
            )
            .unwrap();

        daemon
            .appender
            .send_input(
                Input::ToolBegin {
                    source_ts_ns: Some(2_000_000),
                    session: session.clone(),
                    tool_use_id: "tool-first".to_string(),
                    tool_name: "Bash".to_string(),
                    input: ToolInput::Shell {
                        argv: "git status".to_string(),
                        cwd: Some("/repo".to_string()),
                        git_tree: None,
                    },
                },
                2_000_000,
            )
            .unwrap();

        daemon.appender.flush();

        let reg = daemon.appender.registry();
        let mut seen_keys = HashSet::new();

        for entry in reg.iter() {
            assert!(
                seen_keys.insert(entry.key.clone()),
                "Duplicate track key found in registry: {}",
                entry.key
            );
            initial_tracks.insert(entry.key.clone(), entry.uuid);
        }

        assert!(initial_tracks.contains_key("agent:agent-restart-alpha"));
        assert!(initial_tracks.contains_key("agent:agent-restart-alpha/run:1"));
    } // Daemon dropped / stopped here

    // Phase 2: Restart daemon on same storage directory
    {
        let daemon = Daemon::start(paths, config).unwrap();

        // Check immediately after restart before new events
        let reg_after_start = daemon.appender.registry();
        for (key, uuid) in &initial_tracks {
            let restored = reg_after_start
                .get_by_key(key)
                .expect("Track key missing after restart");
            assert_eq!(
                restored.uuid, *uuid,
                "UUID changed across restart for key {}",
                key
            );
        }

        // Append more events in phase 2
        daemon
            .appender
            .send_input(
                Input::ToolEnd {
                    source_ts_ns: Some(3_000_000),
                    session: session.clone(),
                    tool_use_id: "tool-first".to_string(),
                    error: false,
                    result_bytes: Some(100),
                    exit_code: Some(0),
                },
                3_000_000,
            )
            .unwrap();

        daemon
            .appender
            .send_input(
                Input::RunEnd {
                    source_ts_ns: Some(4_000_000),
                    session: session.clone(),
                    exit_code: Some(0),
                    reason: Some("success".to_string()),
                },
                4_000_000,
            )
            .unwrap();

        daemon.appender.flush();

        let final_reg = daemon.appender.registry();
        let mut seen_final_keys = HashSet::new();

        for entry in final_reg.iter() {
            assert!(
                seen_final_keys.insert(entry.key.clone()),
                "Duplicate track key found after phase 2 append: {}",
                entry.key
            );
            if let Some(&original_uuid) = initial_tracks.get(&entry.key) {
                assert_eq!(
                    entry.uuid, original_uuid,
                    "Track UUID changed for key {}",
                    entry.key
                );
            }
        }
    }
}
