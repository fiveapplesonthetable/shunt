//! Parity tests for `decide::ops::selfupdate` (SU1–SU25).

use std::collections::BTreeSet;

use crate::decide::ops::selfupdate::*;
use crate::decide::ops::setup::{plan_link, LinkAction, LinkState};
use crate::decide::ops::OpsConfig;

#[test]
fn su1_rule_sentence_names_tool_and_code_env_var() {
    let s = rule_sentence("tacho", "TACHO_");
    assert!(s.contains("TACHO_CODE"));
    assert!(s.contains("tacho version"));
}

#[test]
fn su2_and_su3_target_picks_newest_green_tool_commit_within_candidates_and_skips_refused() {
    let policy = SelfUpdatePolicy::default();
    let behind = vec![
        Behind::new("red-newest", 900),
        Behind::green("green-second", 800),
        Behind::green("green-older", 700),
    ];
    assert_eq!(target(&behind, None, &policy), Some("green-second".into()));
    assert_eq!(target(&behind, Some("green-second"), &policy), None);
}

#[test]
fn su4_to_su9_stale_and_distance_consider_only_tool_touching_commits() {
    let policy = SelfUpdatePolicy::default();
    assert!(!stale(&[], 10_000, &policy));
    assert_eq!(distance(&[], 10_000, "tacho"), "up to date with main");

    // Untouching commits do not make the tool behind or stale
    let docs_only = vec![Behind::untouching("aaaa", 1000, true)];
    assert!(!stale(&docs_only, 10_000, &policy));
    assert_eq!(
        distance(&docs_only, 10_000, "tacho"),
        "up to date with main"
    );

    // Young tool commit -> not stale, but has distance
    let young = vec![Behind::new("bbbb", 10_000 - 600)];
    assert!(!stale(&young, 10_000, &policy));
    assert_eq!(
        distance(&young, 10_000, "tacho"),
        "behind main by 1 tacho commit, the oldest 10 min old"
    );

    // Old tool commit (> 30m) -> stale
    let old = vec![Behind::new("cccc", 10_000 - 2400)];
    assert!(stale(&old, 10_000, &policy));
}

#[test]
fn su10_to_su12_warning_and_attention_messages() {
    let policy = SelfUpdatePolicy::default();
    assert!(warning(&[], 10_000, "tacho").is_none());
    let old = vec![Behind::new("cccc", 10_000 - 2400)];
    let w = warning(&old, 10_000, "tacho").unwrap();
    assert!(w.contains("behind main by 1 tacho commit"));
    let att = attention(&old, 10_000, "tacho", &policy).unwrap();
    assert!(att.contains("(over 30 min)"));
}

#[test]
fn su13_to_su20_state_machine_and_advance_pass_with_rollback_and_unit_drift() {
    let cfg = OpsConfig {
        owner_issue: 10,
        ..Default::default()
    };

    // SU13 & SU16: non-detached checkout is not moved
    let not_detached = AdvanceInput {
        detached: false,
        head: "11111111".into(),
        behind: vec![Behind::green("22222222", 1000)],
        refused: None,
        move_ok: true,
        self_test: (true, String::new()),
        drifted_units: vec![],
        now: 2000,
    };
    let out1 = advance(&not_detached, &cfg);
    assert!(out1.effects.is_empty());
    assert_eq!(out1.head_after, "11111111");

    // SU18: failing self-test rolls back to previous SHA, refuses target, and tells owner issue
    let failing = AdvanceInput {
        detached: true,
        head: "11111111".into(),
        behind: vec![Behind::green("22222222", 1000)],
        refused: None,
        move_ok: true,
        self_test: (false, "test failure".into()),
        drifted_units: vec![],
        now: 2000,
    };
    let out2 = advance(&failing, &cfg);
    assert_eq!(out2.head_after, "11111111");
    assert_eq!(out2.refused_after, Some("22222222".into()));
    assert!(out2
        .effects
        .iter()
        .any(|e| matches!(e, UpdEffect::Refuse { sha, .. } if sha == "22222222")));
    assert!(out2
        .effects
        .iter()
        .any(|e| matches!(e, UpdEffect::Tell { issue: 10, .. })));

    // SU19 & SU20: passing self-test records CodeMoved, installs drifted units, and restarts
    let passing = AdvanceInput {
        detached: true,
        head: "11111111".into(),
        behind: vec![Behind::green("22222222", 1000)],
        refused: None,
        move_ok: true,
        self_test: (true, "ok".into()),
        drifted_units: vec!["tacho.service".into()],
        now: 2000,
    };
    let out3 = advance(&passing, &cfg);
    assert_eq!(out3.head_after, "22222222");
    assert!(out3.code_moved.is_some());
    assert!(out3
        .effects
        .iter()
        .any(|e| matches!(e, UpdEffect::InstallUnits { .. })));
    assert!(out3.effects.iter().any(|e| matches!(
        e,
        UpdEffect::Restart {
            service_changed: true,
            ..
        }
    )));
}

#[test]
fn su21_and_su22_version_text_reports_head_distance_last_move_and_units() {
    let cfg = OpsConfig::default();
    let moved = CodeMoved {
        from: "11111111".into(),
        to: "22222222".into(),
        at: 1_700_000_000,
    };
    let txt = version_text(
        "22222222",
        "/srv/fleet/code",
        "22222222",
        &[],
        Some(&moved),
        &["tacho.service".into()],
        1_700_000_060,
        &cfg,
    );
    assert!(txt.contains("running 22222222 from /srv/fleet/code"));
    assert!(txt.contains("up to date with main"));
    assert!(txt.contains("from 11111111 to 22222222"));
    assert!(txt.contains("systemctl --user restart tacho.service"));
}

#[test]
fn su23_cli_warns_for_commands_except_internal_helpers() {
    assert!(should_warn_cli("tick"));
    assert!(should_warn_cli("status"));
    assert!(!should_warn_cli("run-batch"));
    assert!(!should_warn_cli("summary-line"));
    assert!(!should_warn_cli("shrink-warm"));
}

#[test]
fn su24_check_areas_recognizes_tool_only_changes_without_selecting_console_or_android() {
    let policy = AreaCheckPolicy::default();
    let selected: BTreeSet<String> = ["console", "android", "fuzz"]
        .into_iter()
        .map(String::from)
        .collect();
    let skipped = BTreeSet::new();
    let files = vec!["tools/tacho/src/main.rs".to_string()];
    let required = check_areas(&selected, &files, &skipped, false, &policy);
    assert!(!required.contains("console"));
    assert!(!required.contains("android"));
    assert!(!required.contains("fuzz"));
}

#[test]
fn su25_link_repoints_wrong_symlink_and_preserves_regular_files() {
    let repoint = plan_link(
        "/repo/bin/tacho",
        "tacho",
        "/home/u/.local/bin",
        &LinkState::Symlink {
            target: "/old/bin/tacho".into(),
        },
    );
    assert!(matches!(repoint, LinkAction::Repoint { .. }));

    let keep = plan_link(
        "/repo/bin/tacho",
        "tacho",
        "/home/u/.local/bin",
        &LinkState::OtherFile,
    );
    assert!(matches!(keep, LinkAction::KeepNonLink { .. }));
}
