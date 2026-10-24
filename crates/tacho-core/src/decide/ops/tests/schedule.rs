//! Parity tests for `decide::ops::schedule` (TL1–TL7, RP1–RP15, RB1–RB5, MH1–MH2, DP1–DP5).

use std::collections::{BTreeMap, BTreeSet};

use crate::decide::ops::schedule::*;
use crate::decide::ops::tell::Fact;

#[test]
fn tl1_to_tl7_tick_step_order_isolation_and_pr_filter() {
    // TL1: all 20 steps run in order; one failing step does not abort the rest
    let mut visited = Vec::new();
    let errs = run_steps(|step| {
        visited.push(step);
        if step == Step::Merges {
            Err("gh api timeout".into())
        } else {
            Ok(())
        }
    });
    assert_eq!(visited.len(), 20);
    assert_eq!(errs, vec!["merges failed: gh api timeout"]);

    // TL2–TL7: PR filter keeps non-drafts, recent drafts (<= 20m), and tracked drafts
    let policy = SchedulePolicy::default();
    let now = 10_000;
    let raw = vec![
        PrSummary {
            number: 1,
            is_draft: false,
            updated_at: 1000,
            ..Default::default()
        },
        PrSummary {
            number: 2,
            is_draft: true,
            updated_at: now - 600,
            ..Default::default()
        },
        PrSummary {
            number: 3,
            is_draft: true,
            updated_at: now - 3600,
            ..Default::default()
        },
        PrSummary {
            number: 4,
            branch: "tracked-branch".into(),
            is_draft: true,
            updated_at: now - 3600,
            ..Default::default()
        },
    ];
    let kept = filter_open_prs(&raw, now, &[], &["tracked-branch".into()], &[], &policy);
    let nums: Vec<u64> = kept.iter().map(|p| p.number).collect();
    assert_eq!(nums, vec![1, 2, 4]);
}

#[test]
fn rp1_to_rp6_train_and_oversized_pr_detection() {
    let policy = SchedulePolicy::default();
    // RP1: single PR targeting main is not a train
    let single = vec![PrSummary {
        number: 10,
        branch: "feat-a".into(),
        base_ref: "main".into(),
        ..Default::default()
    }];
    assert!(find_trains(&single, &[]).is_empty());

    // RP2: two stacked PRs ordered root to tip
    let stacked = vec![
        PrSummary {
            number: 11,
            branch: "feat-b".into(),
            base_ref: "feat-a".into(),
            ..Default::default()
        },
        PrSummary {
            number: 10,
            branch: "feat-a".into(),
            base_ref: "main".into(),
            ..Default::default()
        },
    ];
    assert_eq!(find_trains(&stacked, &[]), vec![vec![10, 11]]);

    // RP3: PRs sharing an issue ordered by number
    let same_issue = vec![
        PrSummary {
            number: 15,
            branch: "b2".into(),
            base_ref: "main".into(),
            issue: Some(42),
            ..Default::default()
        },
        PrSummary {
            number: 12,
            branch: "b1".into(),
            base_ref: "main".into(),
            issue: Some(42),
            ..Default::default()
        },
    ];
    assert_eq!(find_trains(&same_issue, &[]), vec![vec![12, 15]]);

    // RP4: already tracked PRs skipped
    let existing = vec![ReplayTrain {
        key: "t1".into(),
        prs: vec![12, 15],
        state: ReplayState::Running,
        ..Default::default()
    }];
    assert!(find_trains(&same_issue, &existing).is_empty());

    // RP5 & RP6: oversized PR detection (> 25 files or > 1200 lines)
    let prs = vec![
        PrSummary {
            number: 20,
            additions: 800,
            deletions: 500,
            ..Default::default()
        },
        PrSummary {
            number: 21,
            additions: 100,
            deletions: 50,
            ..Default::default()
        },
    ];
    assert_eq!(
        find_oversized(&prs, &BTreeSet::new(), &[], &policy),
        vec![20]
    );
}

#[test]
fn rp7_to_rp15_replay_draft_visibility_prune_protection_and_issue_closure() {
    let policy = SchedulePolicy::default();
    let now = 10_000;
    let replays = vec![
        ReplayTrain {
            key: "synth".into(),
            prs: vec![10, 11],
            branches: vec!["b10".into(), "b11".into()],
            replay_branch: "fleet/replay-synth".into(),
            head_pr: Some(99),
            mode: ReplayMode::Synthetic,
            state: ReplayState::Running,
            issue: Some(50),
            ..Default::default()
        },
        ReplayTrain {
            key: "native".into(),
            prs: vec![20, 21],
            mode: ReplayMode::Native,
            state: ReplayState::Running,
            ..Default::default()
        },
    ];
    let raw = vec![
        PrSummary {
            number: 99,
            is_draft: true,
            updated_at: 1000,
            ..Default::default()
        },
        PrSummary {
            number: 20,
            is_draft: true,
            updated_at: 1000,
            ..Default::default()
        },
    ];
    // RP7 & RP8: old draft head_pr and native train drafts are kept
    let kept = filter_open_prs(&raw, now, &[], &[], &replays, &policy);
    assert_eq!(kept.len(), 2);

    // RP8: prune_stale_ids protects active replay PRs and branches, and does nothing when !prs_seen_all
    assert_eq!(
        prune_stale_ids(false, &[], &[10, 88], &[], &["88".into()], &replays),
        (vec![], vec![])
    );
    let (pruned_prs, pruned_conflicts) = prune_stale_ids(
        true,
        &[],
        &[10, 88],
        &[],
        &["10".into(), "88".into(), "b10".into(), "gone-branch".into()],
        &replays,
    );
    assert_eq!(pruned_prs, vec![88]);
    assert_eq!(pruned_conflicts, vec!["88", "gone-branch"]);

    // RP9: issue stays open when a member PR merges while train is still active, closes when head_pr merges
    assert!(!should_close_issue_on_merge(10, 50, &[], &replays));
    assert!(should_close_issue_on_merge(99, 50, &[], &replays));
}

#[test]
fn rb1_to_rb5_conflict_rebase_decisions() {
    let policy = SchedulePolicy::default();
    let replays = vec![ReplayTrain {
        key: "t1".into(),
        prs: vec![10],
        state: ReplayState::Running,
        ..Default::default()
    }];
    let prs = vec![
        // RB1: PR in active replay train is skipped
        PrSummary {
            number: 10,
            branch: "b10".into(),
            mergeable: "CONFLICTING".into(),
            ..Default::default()
        },
        // RB2–RB4: conflicting PR outside replay train spawns rebase up to 2 rounds, then holds
        PrSummary {
            number: 11,
            branch: "b11".into(),
            mergeable: "CONFLICTING".into(),
            ..Default::default()
        },
    ];
    let mut counts = BTreeMap::new();
    let a1 = decide_conflicts(
        &prs,
        &replays,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &mut counts,
        &policy,
    );
    assert_eq!(
        a1,
        vec![ConflictAction::SpawnRebase {
            pr: 11,
            branch: "b11".into(),
            round: 1
        }]
    );
    let a2 = decide_conflicts(
        &prs,
        &replays,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &mut counts,
        &policy,
    );
    assert_eq!(
        a2,
        vec![ConflictAction::SpawnRebase {
            pr: 11,
            branch: "b11".into(),
            round: 2
        }]
    );
    let a3 = decide_conflicts(
        &prs,
        &replays,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &mut counts,
        &policy,
    );
    assert_eq!(
        a3,
        vec![ConflictAction::HoldExhausted {
            pr: 11,
            reason: "conflict resolution rounds exhausted".into()
        }]
    );

    // RB5: when PR becomes MERGEABLE, its conflict count is cleared
    let clean = vec![PrSummary {
        number: 11,
        branch: "b11".into(),
        mergeable: "MERGEABLE".into(),
        ..Default::default()
    }];
    let _ = decide_conflicts(
        &clean,
        &replays,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &mut counts,
        &policy,
    );
    assert!(!counts.contains_key("11"));
}

#[test]
fn mh1_mh2_and_dp1_to_dp5_main_health_and_deploy_step() {
    let policy = SchedulePolicy::default();
    // MH1: main CI failure spawns fix when none running, or emits MainBroken when fix already active
    let obs_fail = MainHealthObs {
        id: 100,
        status: "completed".into(),
        conclusion: "failure".into(),
        title: "cargo test".into(),
    };
    let m1 = main_health_step(Some(&obs_fail), &[], false);
    assert_eq!(m1.spawn_fix_for, Some((100, "cargo test".into())));
    let m2 = main_health_step(Some(&obs_fail), &[], true);
    assert_eq!(
        m2.tell_facts,
        vec![Fact::MainJobFailed {
            run: 100,
            job: "cargo test".into(),
            sha: String::new(),
            first_line: "cargo test".into(),
            url: String::new(),
        }]
    );

    // MH2: main CI success clears pending fixes
    let obs_ok = MainHealthObs {
        id: 101,
        status: "completed".into(),
        conclusion: "success".into(),
        title: "cargo test".into(),
    };
    assert!(main_health_step(Some(&obs_ok), &[100], true).clear_pending_main_fixes);

    // DP1–DP5: deploy step checks deploy_gate job ("Confirm successful main push")
    let mut jobs_pre_gate = BTreeMap::new();
    jobs_pre_gate.insert("Confirm successful main push".into(), "failure".into());
    let mut jobs_post_gate = BTreeMap::new();
    jobs_post_gate.insert("Confirm successful main push".into(), "success".into());

    let runs = vec![
        // In-progress run: not marked seen
        DeployRunObs {
            id: 1,
            status: "in_progress".into(),
            conclusion: "".into(),
            title: "d1".into(),
            job_conclusions: BTreeMap::new(),
        },
        // Pre-deploy CI failure (gate did not pass): marked seen, no fix or alert
        DeployRunObs {
            id: 2,
            status: "completed".into(),
            conclusion: "failure".into(),
            title: "d2".into(),
            job_conclusions: jobs_pre_gate,
        },
        // Real deploy failure (gate passed): spawns fix and emits event
        DeployRunObs {
            id: 3,
            status: "completed".into(),
            conclusion: "failure".into(),
            title: "d3".into(),
            job_conclusions: jobs_post_gate.clone(),
        },
        // Second real deploy failure while first fix active: emits DeployDone(ok=false) fact
        DeployRunObs {
            id: 4,
            status: "completed".into(),
            conclusion: "failure".into(),
            title: "d4".into(),
            job_conclusions: jobs_post_gate,
        },
    ];
    let out = deploy_step(&runs, &BTreeSet::new(), false, &policy);
    assert_eq!(out.newly_seen, vec![2, 3, 4]);
    assert_eq!(out.spawn_fix_for, Some((3, "d3".into())));
    assert_eq!(
        out.tell_facts,
        vec![Fact::DeployDone {
            run: 4,
            sha: String::new(),
            ok: false,
            what: "d4".into()
        }]
    );
}

#[test]
fn events_parse_since_and_select_events() {
    assert_eq!(parse_since("15m", 10_000).unwrap(), 10_000 - 900);
    assert_eq!(parse_since("2h", 10_000).unwrap(), 10_000 - 7200);
    assert_eq!(parse_since("1d", 100_000).unwrap(), 100_000 - 86400);
    assert_eq!(parse_since("5000", 10_000).unwrap(), 5000);
    assert!(parse_since("bad", 10_000).is_err());

    let lines = vec![
        r#"{"ts": 100, "event": "a"}"#.to_string(),
        r#"{"ts": 200, "event": "b"}"#.to_string(),
        r#"{"ts": 300, "event": "c"}"#.to_string(),
    ];
    assert_eq!(select_events(&lines, Some(200), 50).len(), 2);
    assert_eq!(select_events(&lines, None, 2).len(), 2);
}
