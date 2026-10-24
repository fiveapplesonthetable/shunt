use std::fs;
use tempfile::tempdir;

use tacho_core::decide::dispatch::{AgentRun, Hold};
use tacho_core::decide::watch::CostRecord;
use tacho_daemon::FleetState;

use super::collect::parse_cpus;
use super::*;

#[test]
fn validate_sections_accepts_all_valid_and_rejects_unknown() {
    let ok = validate_sections(&[
        "headline,needs".to_string(),
        "MACHINE".to_string(),
        "cost,held,stale,progress,flakes,pipeline,agents,attention,needs_you".to_string(),
    ])
    .expect("valid sections");
    assert_eq!(ok.len(), 12);

    let err = validate_sections(&["headline,bogus_section".to_string()]).unwrap_err();
    assert!(
        err.contains("unknown section 'bogus_section'"),
        "err: {err}"
    );
}

#[test]
fn load_status_snapshot_refuses_corrupted_truncated_and_unsupported_version() {
    let tmp = tempdir().unwrap();
    let snap_path = tmp.path().join("snapshot.json");

    // 1. Truncated JSON
    fs::write(&snap_path, "{\"version\": 1, \"at\": 1000, \"headl").unwrap();
    let err = load_status_snapshot(tmp.path(), 1050).unwrap_err();
    assert!(
        err.contains("snapshot.json is malformed"),
        "expected malformed error, got: {err}"
    );

    // 2. Garbage bytes
    fs::write(&snap_path, b"\x00\x01not-json\xff").unwrap();
    let err = load_status_snapshot(tmp.path(), 1050).unwrap_err();
    assert!(err.contains("snapshot.json is malformed"));

    // 3. Unsupported version
    fs::write(&snap_path, r#"{"version": 99, "at": 1000}"#).unwrap();
    let err = load_status_snapshot(tmp.path(), 1050).unwrap_err();
    assert!(
        err.contains("unsupported version"),
        "expected unsupported version error, got: {err}"
    );
}

#[test]
fn load_status_snapshot_enforces_exact_300s_staleness_boundary() {
    let tmp = tempdir().unwrap();
    let snap_path = tmp.path().join("snapshot.json");
    let now = 10_000;

    // Exactly 300s old -> accepted
    fs::write(
        &snap_path,
        serde_json::to_string(&serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "at": now - MAX_SNAPSHOT_AGE_SECS,
        }))
        .unwrap(),
    )
    .unwrap();
    let ok = load_status_snapshot(tmp.path(), now).expect("300s old snapshot must be accepted");
    assert_eq!(ok.at, now - MAX_SNAPSHOT_AGE_SECS);

    // 301s old -> refused with exact actionable message
    fs::write(
        &snap_path,
        serde_json::to_string(&serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "at": now - (MAX_SNAPSHOT_AGE_SECS + 1),
        }))
        .unwrap(),
    )
    .unwrap();
    let err = load_status_snapshot(tmp.path(), now).unwrap_err();
    assert!(
        err.contains("the snapshot is 5 min old and the daemon writes one every tick; it has stopped. run: tacho tick"),
        "unexpected error: {err}"
    );
}

#[test]
fn status_snapshot_scrubs_secrets_in_json_and_text_and_caps_attention() {
    let mut state = FleetState::default();
    let now = 50_000;
    let fake_ghp = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let fake_sk = format!("sk-{}", "X1y2Z3a4B5c6D7e8F9g0H1i2J3k4L5m6N7o8");

    for i in 0..8 {
        let key = format!("task-{i}");
        state.agents.insert(
            key.clone(),
            AgentRun {
                key: key.clone(),
                area: "auth".to_string(),
                provider: "claude".to_string(),
                model: Some("claude-opus-4-6".to_string()),
                worktree: format!("/tmp/wt-{i}"),
                started_at: now - 120,
                finished_at: Some(now - 10),
                exit_code: Some(1),
                status_note: format!("failed with token {fake_ghp}"),
                ..Default::default()
            },
        );
    }
    state.holds.insert(
        "pr:42".to_string(),
        Hold {
            reason: format!("holding secret {fake_sk}"),
            since: now - 100_000,
        },
    );
    state.costs.push(CostRecord {
        key: "task-0".to_string(),
        provider: "claude".to_string(),
        model: "claude-opus-4-6".to_string(),
        usd: 1.75,
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    });

    let snap = build_snapshot_from_fleet(&state, now, now);
    let json_out = render_snapshot_json(&snap, &[]);
    let text_out = render_snapshot_text(&snap, &[]);

    assert!(
        !json_out.contains(&fake_ghp) && !json_out.contains(&fake_sk),
        "JSON output leaked secret: {json_out}"
    );
    assert!(
        !text_out.contains(&fake_ghp) && !text_out.contains(&fake_sk),
        "text output leaked secret: {text_out}"
    );
    assert!(
        text_out.contains("... and 3 more"),
        "expected attention cap in text_out: {text_out}"
    );
}

#[test]
fn parse_cpus_handles_cgroup_v2_quotas() {
    assert_eq!(parse_cpus("max 100000\n"), None);
    assert_eq!(parse_cpus("400000 100000\n"), Some(4.0));
    assert_eq!(parse_cpus("150000 100000"), Some(1.5));
    assert_eq!(parse_cpus("0 100000"), None);
    assert_eq!(parse_cpus("invalid"), None);
}
