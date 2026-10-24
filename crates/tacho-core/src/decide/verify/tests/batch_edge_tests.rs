//! Parity tests for `test_batch.py` (cases 21–51) and `test_statuses.py` (cases 214–223).

use super::batch_tests::{heads_for, make_state};
use super::*;

#[test]
fn test_infrastructure_failure_blames_nobody_and_untied_pair_recorded() {
    let st = make_state(&[1, 2], &["rust"]);
    let ctx = VerifyCtx::default();
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        red_check(
            &m,
            &["rust"],
            &["rust"],
            " No space left on device ",
            Some("t1"),
        )
    });
    assert_eq!(verdict.outcome, Outcome::Infrastructure);
    assert!(verdict.blamed.is_empty());

    let st3 = make_state(&[10, 20, 30], &["rust"]);
    let mut ctx0 = VerifyCtx::default();
    ctx0.cfg.reruns = 0;
    let (_, v3, _) = run_batch_to_verdict(st3, &ctx0, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if m == vec![10] {
            green_check(&m, &["rust"], Some("t1"))
        } else if m == vec![10, 20] {
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "Cannot connect to the Docker daemon",
                Some("t2"),
            )
        } else {
            red_check(&m, &["rust"], &["rust"], "test x ... FAILED", Some("t3"))
        }
    });
    assert_eq!(v3.outcome, Outcome::Infrastructure);
    assert_eq!(v3.untied.len(), 1);
    assert_eq!(v3.untied[0].prs, vec![10, 20]);
}

#[test]
fn test_skips_partial_and_rebase_decisions_and_nothing_merged() {
    let st = make_state(&[1, 2], &["rust"]);
    let ctx = VerifyCtx::default();
    let (_, v_conflict, _) = run_batch_to_verdict(st.clone(), &ctx, 1000, |_, _, _, _| {
        let mut r = green_check(&[1], &["rust"], Some("t1"));
        r.skipped = vec![2];
        r.skips.insert(
            2,
            SkipReason::Conflict {
                files: vec!["crates/a.rs".to_string()],
            },
        );
        r
    });
    assert_eq!(v_conflict.outcome, Outcome::Partial);
    assert_eq!(
        v_conflict.rebases.get(&2),
        Some(&vec!["crates/a.rs".to_string()])
    );

    let (_, v_fetch, _) = run_batch_to_verdict(st.clone(), &ctx, 1000, |_, _, _, _| {
        let mut r = green_check(&[1], &["rust"], Some("t1"));
        r.skipped = vec![2];
        r.skips.insert(2, SkipReason::FetchFailed);
        r
    });
    assert!(v_fetch.rebases.is_empty());

    let (_, v_moved, _) = run_batch_to_verdict(st.clone(), &ctx, 1000, |_, _, _, _| {
        let mut r = green_check(&[1], &["rust"], Some("t1"));
        r.skipped = vec![2];
        r.skips.insert(2, SkipReason::HeadMoved);
        r
    });
    assert!(v_moved.rebases.is_empty());

    let (_, v_none, _) = run_batch_to_verdict(st, &ctx, 1000, |_, _, _, _| CheckResult {
        rc: RC_NOTHING_MERGED,
        merged: Vec::new(),
        skipped: vec![1, 2],
        ..Default::default()
    });
    assert_eq!(v_none.outcome, Outcome::NothingMerged);
}

#[test]
fn test_round_limit_counterexample_and_wall_clock() {
    let st = make_state(&[1, 2, 3, 4], &["rust"]);
    let mut ctx = VerifyCtx::default();
    ctx.cfg.rounds = 2;
    let (_, v_gave_up, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if m.is_empty() {
            green_check(&[], &["rust"], Some("t-main"))
        } else {
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "test bad ... FAILED",
                Some("t-bad"),
            )
        }
    });
    assert_eq!(v_gave_up.outcome, Outcome::GaveUp);
    assert_eq!(v_gave_up.blamed.len(), 2);

    let st2 = make_state(&[1, 2], &["rust"]);
    let (_, v_ce, _) =
        run_batch_to_verdict(st2, &VerifyCtx::default(), 1000, |members, _, _, _| {
            let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "Test failed: minimal failing input: x = 0",
                Some("t1"),
            )
        });
    assert_eq!(v_ce.outcome, Outcome::Counterexample);

    let st3 = make_state(&[1, 2, 3], &["rust"]);
    let mut ctx_wc = VerifyCtx::default();
    ctx_wc.cfg.reruns = 0;
    ctx_wc.cfg.wall_clock_secs = 1800;
    let (_, v_wc, _) = run_batch_to_verdict(st3, &ctx_wc, 1000, |members, _, _, now| {
        *now += 1900;
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        red_check(&m, &["rust"], &["rust"], "test x ... FAILED", Some("t1"))
    });
    assert_eq!(v_wc.outcome, Outcome::WallClock);
}

#[test]
fn test_summary_line_and_classify_with() {
    let areas = vec![
        "gate".to_string(),
        "rust".to_string(),
        "typescript".to_string(),
    ];
    assert_eq!(
        summary_line_for("some output\nfailed: rust typescript\n", &areas),
        "failed: rust typescript"
    );
    assert_eq!(
        summary_line_for("passed: gate rust\nfailed: something in test\n", &areas),
        "passed: gate rust"
    );
    let r = CheckResult {
        rc: 1,
        line: String::new(),
        ..Default::default()
    };
    assert_eq!(failed_areas_of(&r, &areas), areas);

    let pats = LogPatterns::default();
    assert_eq!(
        classify_with("ERROR: No space left on device", &pats),
        Classification::Infrastructure
    );
    assert_eq!(
        classify_with("error[E0308]: mismatched types", &pats),
        Classification::Code
    );
    assert_eq!(
        classify_with("No space left on device\ntest foo::bar ... FAILED\n", &pats),
        Classification::Code
    );
}

#[test]
fn test_reuses_green_batch_with_same_main_and_heads() {
    let heads = heads_for(&[1, 2]);
    let cand = ReusableCandidate {
        batch_id: "rust-900".to_string(),
        status: "done".to_string(),
        outcome: Some(Outcome::Green),
        main_sha: Some("main-sha-1".to_string()),
        heads: heads.clone(),
        passed: vec![1, 2],
        areas: vec!["gate".to_string(), "rust".to_string()],
    };
    assert_eq!(
        reusable_batch_id(
            &[cand.clone()],
            "main-sha-1",
            &heads,
            &[1, 2],
            &["gate".to_string(), "rust".to_string()]
        ),
        Some("rust-900".to_string())
    );
    assert_eq!(
        reusable_batch_id(
            &[cand.clone()],
            "main-sha-2",
            &heads,
            &[1, 2],
            &["rust".to_string()]
        ),
        None
    );
    let mut moved = heads.clone();
    moved.insert(2, "sha2-new".to_string());
    assert_eq!(
        reusable_batch_id(
            &[cand],
            "main-sha-1",
            &moved,
            &[1, 2],
            &["rust".to_string()]
        ),
        None
    );
}

#[test]
fn test_redraw_areas_and_stale_shots_invalidation() {
    let cfg_areas = demo_area_cfg();
    assert_eq!(
        redraw_areas(&["apps/console/src/App.tsx".to_string()], &cfg_areas),
        ["pixel".to_string()].into_iter().collect()
    );
    assert!(redraw_areas(&["crates/core/src/lib.rs".to_string()], &cfg_areas).is_empty());

    let mut cfg = VerifyConfig::default();
    cfg.areas = cfg_areas;
    let heads = heads_for(&[7]);
    assert!(matches!(
        stale_shots_step(
            &[7],
            &heads,
            &["pixel".to_string()],
            &["apps/console/src/App.tsx".to_string()],
            &cfg
        ),
        StaleShotsAction::RefreshAndInvalidate { pr: 7, .. }
    ));

    let st = make_state(&[7], &["gate", "pixel"]);
    let ctx = VerifyCtx {
        cfg,
        ..Default::default()
    };
    let (st2, _) = step(&st, &VerifyEvent::Start { now: 1000 }, &ctx);
    let (_, acts) = step(
        &st2,
        &VerifyEvent::ShotsRefreshed {
            pr: 7,
            new_sha: "sha7-refreshed".to_string(),
            now: 1010,
        },
        &ctx,
    );
    let Some(VerifyAction::RecordVerdict { verdict, .. }) = acts
        .into_iter()
        .find(|a| matches!(a, VerifyAction::RecordVerdict { .. }))
    else {
        panic!("expected verdict");
    };
    assert_eq!(verdict.outcome, Outcome::ShotsUpdated);
    assert!(verdict.passed.is_empty());
}

#[test]
fn test_commit_status_lifecycle_pending_green_red_undecided_crashed() {
    let cfg = VerifyConfig::default();
    let p = pending_status(10, "sha10", "rust-100", &[10, 20], &cfg).unwrap();
    assert_eq!(p.state, StatusState::Pending);
    let c = crashed_status(10, "sha10", "rust-100", 137, &cfg).unwrap();
    assert_eq!(c.state, StatusState::Error);

    let green_s = member_verdict_status(
        10,
        "sha10",
        "rust-100",
        Outcome::Partial,
        &[10],
        &[],
        &[20],
        None,
        &cfg,
    )
    .unwrap();
    assert_eq!(green_s.state, StatusState::Success);

    let skip_s = member_verdict_status(
        20,
        "sha20",
        "rust-100",
        Outcome::Partial,
        &[10],
        &[],
        &[20],
        None,
        &cfg,
    )
    .unwrap();
    assert_eq!(skip_s.state, StatusState::Error);

    let long_summary = "x".repeat(200);
    let blamed_s = member_verdict_status(
        10,
        "sha10",
        "rust-100",
        Outcome::Red,
        &[],
        &[10],
        &[],
        Some(&long_summary),
        &cfg,
    )
    .unwrap();
    assert_eq!(blamed_s.state, StatusState::Failure);
    assert_eq!(blamed_s.description.len(), 140);

    for out in [
        Outcome::MainBroken,
        Outcome::Infrastructure,
        Outcome::NothingMerged,
        Outcome::Counterexample,
        Outcome::RedOnce,
        Outcome::GaveUp,
    ] {
        let u =
            member_verdict_status(10, "sha10", "rust-100", out, &[], &[], &[], None, &cfg).unwrap();
        assert_eq!(u.state, StatusState::Error);
    }
    assert!(member_verdict_status(
        10,
        "",
        "rust-100",
        Outcome::Green,
        &[10],
        &[],
        &[],
        None,
        &cfg
    )
    .is_none());
}
