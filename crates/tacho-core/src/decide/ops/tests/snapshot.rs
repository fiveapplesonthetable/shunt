//! Parity tests for `decide::ops::snapshot` and `decide::ops::render` (SN1–SN29, RT1–RT2).

use std::collections::BTreeMap;

use crate::decide::ops::render::*;
use crate::decide::ops::selfupdate::Behind;
use crate::decide::ops::snapshot::*;
use crate::decide::ops::OpsConfig;

#[test]
fn sn1_and_sn2_needs_you_lists_ready_prs_and_needs_human_issues() {
    let mut obs = SnapshotObs::default();
    obs.ready_prs.push((101, "Add rate feed".into(), 600));
    obs.needs_human_issues.push((42, "Design question".into()));
    let ny = needs_you(&obs);
    assert_eq!(
        ny,
        vec![
            "PR #101 ready to merge: Add rate feed",
            "issue #42 needs human: Design question"
        ]
    );
}

#[test]
fn sn3_to_sn9_needs_attention_covers_main_deploy_batches_agents_selfupdate_tick_and_held() {
    let rules = AttentionRules::default();
    let mut obs = SnapshotObs {
        now: 100_000,
        last_tick_age: Some(900),
        main_block: MainStatusBlock {
            status: "failure".into(),
            last_completed: Some(MainCiRun {
                id: 1,
                conclusion: "failure".into(),
                title: "cargo test".into(),
                ..Default::default()
            }),
            deploy: Some(MainCiRun {
                id: 2,
                status: "completed".into(),
                conclusion: "failure".into(),
                title: "deploy prod".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    obs.running_batches
        .push(("b1".into(), "dead".into(), 100, None));
    obs.running_batches
        .push(("b2".into(), "rust".into(), 2000, Some(2400)));
    obs.running_agents
        .push(("issue-9".into(), "claude".into(), 3600));
    obs.selfupdate_behind = vec![Behind::new("11111111", 100_000 - 4 * 3600)];
    obs.held_prs.insert(55, "merge conflict".into());
    obs.held_replays
        .insert("train-1".into(), "cherry-pick failed".into());
    obs.blocked_issues.insert(77, "missing spec".into());
    obs.retried.insert("issue-88".into(), 2);
    obs.ready_prs.push((101, "Ready PR".into(), 2400));

    let att = needs_attention(&obs, &rules, "tacho");
    assert!(att.iter().any(|s| s.contains("main is broken")));
    assert!(att.iter().any(|s| s.contains("deploy #2 failed")));
    assert!(att.iter().any(|s| s.contains("batch b1 died")));
    assert!(att.iter().any(|s| s.contains("batch b2 stale")));
    assert!(att.iter().any(|s| s.contains("agent issue-9")));
    assert!(att.iter().any(|s| s.contains("behind main")));
    assert!(att.iter().any(|s| s.contains("tick loop quiet")));
    assert!(att.iter().any(|s| s.contains("PR #55 held")));
    assert!(att.iter().any(|s| s.contains("replay train-1 held")));
    assert!(att.iter().any(|s| s.contains("issue #77 blocked")));
    assert!(att.iter().any(|s| s.contains("issue-88 exhausted retries")));
    assert!(att.iter().any(|s| s.contains("PR #101 ready for 40m")));
}

#[test]
fn sn10_to_sn15_and_rt1_rt2_attention_prioritization_dedup_cap_and_filters() {
    let rules = AttentionRules::default();
    let mut obs = SnapshotObs {
        now: 100_000,
        open_prs: vec![10],
        open_issues: vec![20],
        ..Default::default()
    };
    // Closed PR #99 and closed issue #98 should be filtered out (SN13, RT2)
    obs.held_prs.insert(99, "closed pr".into());
    obs.held_prs.insert(10, "open pr held".into());
    obs.blocked_issues.insert(98, "closed issue".into());
    obs.blocked_issues.insert(20, "open issue blocked".into());
    // RT1: retried key with an active agent run is suppressed
    obs.retried.insert("issue-20".into(), 3);
    obs.running_agents
        .push(("issue-20".into(), "claude".into(), 120));

    let att = needs_attention(&obs, &rules, "tacho");
    assert!(!att.iter().any(|s| s.contains("#99")));
    assert!(!att.iter().any(|s| s.contains("#98")));
    assert!(!att.iter().any(|s| s.contains("issue-20 exhausted retries")));
    assert!(att.iter().any(|s| s.contains("PR #10 held")));
    assert!(att.iter().any(|s| s.contains("issue #20 blocked")));

    // SN12: snapshot caps attention at 5 while headline.attention has full count
    for pr in 100..110 {
        obs.open_prs.push(pr);
        obs.held_prs.insert(pr, "held".into());
    }
    let shot = snapshot(&obs, &rules, &OpsConfig::default());
    assert_eq!(shot.attention.len(), 5);
    assert!(shot.headline.attention > 5);
}

#[test]
fn sn16_to_sn20_main_block_combinations() {
    // SN16: current running + last completed passing
    let b1 = build_main_block(
        Some(MainCiRun {
            id: 2,
            status: "in_progress".into(),
            ..Default::default()
        }),
        Some(MainCiRun {
            id: 1,
            status: "completed".into(),
            conclusion: "success".into(),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(b1.status, "passing");
    assert!(b1.current.is_some());

    // SN17: only running -> "running"
    let b2 = build_main_block(
        Some(MainCiRun {
            id: 2,
            status: "in_progress".into(),
            ..Default::default()
        }),
        None,
        None,
    );
    assert_eq!(b2.status, "running");

    // SN18 & SN19: failure and unknown
    let b3 = build_main_block(
        None,
        Some(MainCiRun {
            id: 1,
            status: "completed".into(),
            conclusion: "failure".into(),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(b3.status, "failure");
    let b4 = build_main_block(None, None, None);
    assert_eq!(b4.status, "unknown");
}

#[test]
fn sn21_and_sn22_snapshot_json_roundtrip_and_section_slicing() {
    let mut obs = SnapshotObs {
        now: 50_000,
        redact_literals: vec!["super-secret-value".into()],
        ..Default::default()
    };
    obs.held_prs
        .insert(1, "token super-secret-value leaked".into());
    let shot = snapshot(&obs, &AttentionRules::default(), &OpsConfig::default());
    let json_str = serde_json::to_string(&shot).unwrap();
    assert!(!json_str.contains("super-secret-value"));
    assert!(json_str.contains("[REDACTED]"));

    let decoded: Snapshot = serde_json::from_str(&json_str).unwrap();
    assert_eq!(decoded.version, VERSION);

    let needs_val = section(&shot, &["needs"]).unwrap();
    assert!(needs_val.get("needs_you").is_some());
    assert!(needs_val.get("attention").is_some());
    assert!(section(&shot, &["nonexistent"]).is_err());
}

#[test]
fn sn23_to_sn29_owner_classification_and_cpu_rates() {
    let rules = OwnerRules::default();
    let procs = vec![
        ProcObs {
            pid: 10,
            ppid: 1,
            comm: "Runner.Worker".into(),
            cgroup: "actions.runner.1".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 50.0,
            ..Default::default()
        },
        ProcObs {
            pid: 20,
            ppid: 1,
            comm: "cargo".into(),
            cwd: "/srv/work/batches/b1".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 100.0,
            ..Default::default()
        },
        ProcObs {
            pid: 30,
            ppid: 1,
            comm: "claude".into(),
            cwd: "/srv/work/worktrees/w1".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 30.0,
            ..Default::default()
        },
        ProcObs {
            pid: 31,
            ppid: 30,
            comm: "rustc".into(),
            cwd: "/srv/work/worktrees/w1".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 40.0,
            ..Default::default()
        },
        ProcObs {
            pid: 40,
            ppid: 1,
            comm: "rustc".into(),
            cwd: "/home/dev/manual".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 20.0,
            ..Default::default()
        },
        ProcObs {
            pid: 50,
            ppid: 1,
            comm: "tacho".into(),
            cmdline: "tacho run".into(),
            rss_bytes: 1_073_741_824,
            cpu_seconds: 10.0,
            ..Default::default()
        },
    ];
    let mut prev_map = BTreeMap::new();
    prev_map.insert("batches".into(), 80.0);
    let prev = (990_i64, prev_map);
    let (owners, _) = classify_owners(&procs, 10.0, Some(&prev), 1000, 16, &rules);
    assert_eq!(owners["ci_runners"].count, 1);
    assert_eq!(owners["batches"].count, 1);
    assert_eq!(owners["batches"].cpu_cores, Some(2.0));
    assert_eq!(owners["agents"].count, 2); // claude + child rustc
    assert_eq!(owners["burners"].count, 1); // standalone rustc outside agent tree
    assert_eq!(owners["orchestrator"].count, 1);
    assert!(owners["unknown"].rss_gb >= 3.9); // residual 10GB - 6GB = 4GB
}

#[test]
fn render_status_view_html_issue_serve_top_and_next_step() {
    let cfg = OpsConfig::default();
    let obs = SnapshotObs {
        now: 10_000,
        ..Default::default()
    };
    let shot = snapshot(&obs, &AttentionRules::default(), &cfg);

    assert_eq!(status_view(None, &[], false, true, 10_000, &cfg).0, 1);
    assert_eq!(
        status_view(Some(&shot), &["bad"], false, true, 10_000, &cfg).0,
        2
    );
    assert_eq!(
        status_view(Some(&shot), &[], false, true, 10_050, &cfg).0,
        0
    );
    assert_eq!(
        status_view(Some(&shot), &[], false, true, 10_400, &cfg).0,
        1
    );

    let page = html(&shot, &[], "tacho");
    assert!(page.contains("http-equiv=\"refresh\""));
    let md = issue_text(&shot, "http://100.64.1.2:8787", &[]);
    assert!(issue_due(&md, None, None, 10_000, 900));
    let core = issue_core(&md);
    assert!(!issue_due(&md, Some(&core), Some(9950), 10_000, 900));

    assert_eq!(
        tailnet_address("100.101.102.103/32\n"),
        Some("100.101.102.103".into())
    );
    assert_eq!(serve_request("POST", "/", Some(&page)).status, 405);
    assert_eq!(serve_request("GET", "/", Some(&page)).status, 200);
    assert_eq!(serve_request("HEAD", "/", Some(&page)).body, "");
    assert_eq!(serve_request("GET", "/unknown", Some(&page)).status, 404);

    let lines = frame(&shot, &["ev1".into()], 10, 40, 10_010, "tacho");
    assert!(lines.len() <= 10);
    assert!(lines.iter().all(|l| l.chars().count() <= 40));

    let mut ns = NextStepInput::default();
    assert!(next_step(&ns, &cfg).contains("All clear"));
    ns.ready_prs.push((12, "Ship it".into()));
    assert_eq!(next_step(&ns, &cfg), "Review & merge ready PR #12: Ship it");
}
