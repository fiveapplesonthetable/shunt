//! Parity tests for `test_batch.py` (cases 1–20).

use super::*;
use std::collections::BTreeMap;

pub(super) fn heads_for(prs: &[u64]) -> BTreeMap<u64, String> {
    prs.iter().map(|p| (*p, format!("sha{p}"))).collect()
}

pub(super) fn make_state(prs: &[u64], areas: &[&str]) -> BatchState {
    BatchState::new(
        "rust-1000",
        "rust",
        prs.to_vec(),
        heads_for(prs),
        areas.iter().map(|s| s.to_string()).collect(),
        "main-sha-1",
        1000,
    )
}

#[test]
fn test_next_probe_finds_each_position_0() {
    let cand = vec![10, 20, 30, 40];
    let mut results = BTreeMap::new();
    results.insert(cand.len(), false);
    loop {
        match next_prefix(cand.len(), &results) {
            Probe::Prefix(p) => {
                results.insert(p, p > 0 && p < 1);
            }
            Probe::MainBroken | Probe::Culprit(_) => break,
        }
    }
    let probes: Vec<BisectProbe> = (1..=cand.len())
        .map(|prefix_len| BisectProbe {
            prefix_len,
            passed: prefix_len < 1,
        })
        .collect();
    assert!(matches!(
        bisect::step(&cand, &probes),
        BisectDecision::Resolved { culprit: 10, .. }
    ));
}

#[test]
fn test_next_probe_finds_each_position_1_to_3() {
    let cand = vec![10, 20, 30, 40];
    for (idx, want) in [(2, 20), (3, 30), (4, 40)] {
        let mut probes = Vec::new();
        while let BisectDecision::Probe { prefix_len, .. } = bisect::step(&cand, &probes) {
            probes.push(BisectProbe {
                prefix_len,
                passed: prefix_len < idx,
            });
        }
        assert!(matches!(
            bisect::step(&cand, &probes),
            BisectDecision::Resolved { culprit, .. } if culprit == want
        ));
    }
}

#[test]
fn test_next_probe_logarithmic_count() {
    let cand: Vec<u64> = (1..=16).collect();
    let mut probes = Vec::new();
    while let BisectDecision::Probe { prefix_len, .. } = bisect::step(&cand, &probes) {
        probes.push(BisectProbe {
            prefix_len,
            passed: prefix_len < 11,
        });
    }
    assert!(probes.len() <= 4);
    assert!(matches!(
        bisect::step(&cand, &probes),
        BisectDecision::Resolved { culprit: 11, .. }
    ));
}

#[test]
fn test_all_green_records_every_pr_as_passed_and_clears_old_failures() {
    let st = make_state(&[1, 2, 3], &["gate", "rust"]);
    let ctx = VerifyCtx::default();
    let (_, verdict, actions) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        green_check(&m, &["gate", "rust"], Some("tree-123"))
    });
    assert_eq!(verdict.outcome, Outcome::Green);
    assert_eq!(
        verdict.passed.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(verdict.passed[&1].tree.as_deref(), Some("tree-123"));
    assert_eq!(
        verdict.passed[&1].proved_by.get("rust").map(String::as_str),
        Some("rust-1000")
    );
    assert!(actions.iter().any(|a| matches!(
        a,
        VerifyAction::ClearStaleFailures { prs, .. } if prs == &vec![1, 2, 3]
    )));
}

#[test]
fn test_green_rc_without_area_proof_does_not_record_passed() {
    let st = make_state(&[1, 2], &["gate", "rust"]);
    let ctx = VerifyCtx::default();
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        green_check(&m, &["gate"], Some("tree-half"))
    });
    assert_eq!(verdict.outcome, Outcome::IncompleteProof);
    assert!(verdict.passed.is_empty());
}

#[test]
fn test_flaky_retry_merges_area_proof_on_same_tree() {
    let st = make_state(&[1, 2], &["gate", "rust", "typescript"]);
    let mut ctx = VerifyCtx::default();
    ctx.cfg.pass_on_flaky_rerun = true;
    let mut call = 0;
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        call += 1;
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if call == 1 {
            red_check(
                &m,
                &["gate", "rust", "typescript"],
                &["typescript"],
                "FAIL src/app.test.ts > renders",
                Some("tree-same"),
            )
        } else {
            green_check(&m, &["typescript"], Some("tree-same"))
        }
    });
    assert_eq!(verdict.outcome, Outcome::Green);
    assert_eq!(verdict.passed.len(), 2);
    assert_eq!(verdict.passed[&1].proved_by["gate"], "rust-1000");
    assert_eq!(
        verdict.passed[&1].proved_by["typescript"],
        "rust-1000-retry"
    );
}

#[test]
fn test_flaky_retry_rejects_proof_from_different_tree() {
    let st = make_state(&[1, 2], &["gate", "rust", "typescript"]);
    let ctx = VerifyCtx::default();
    let mut call = 0;
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        call += 1;
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if call == 1 {
            red_check(
                &m,
                &["gate", "rust", "typescript"],
                &["typescript"],
                "FAIL src/app.test.ts > renders",
                Some("tree-A"),
            )
        } else {
            green_check(&m, &["typescript"], Some("tree-B"))
        }
    });
    assert_eq!(verdict.outcome, Outcome::IncompleteProof);
    assert!(verdict.passed.is_empty());
}

#[test]
fn test_bisect_isolates_middle_culprit_and_passes_rest() {
    let st = make_state(&[1, 2, 3, 4], &["gate", "rust"]);
    let ctx = VerifyCtx::default();
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, areas, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        let a: Vec<&str> = areas.iter().map(String::as_str).collect();
        if m.contains(&3) {
            red_check(&m, &a, &["rust"], "test foo::bad ... FAILED", Some("t-bad"))
        } else {
            green_check(&m, &a, Some("t-good"))
        }
    });
    assert_eq!(verdict.outcome, Outcome::Green);
    assert_eq!(verdict.blamed.len(), 1);
    assert_eq!(verdict.blamed[0].pr, 3);
    assert_eq!(
        verdict.passed.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
}

#[test]
fn test_bisect_narrows_to_failed_area() {
    let st = make_state(&[1, 2, 3], &["gate", "rust", "typescript"]);
    let ctx = VerifyCtx::default();
    let mut probe_areas = Vec::new();
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, areas, label, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        let a: Vec<&str> = areas.iter().map(String::as_str).collect();
        if label.contains("bisect") {
            probe_areas.push(areas.to_vec());
        }
        if m.contains(&2) {
            red_check(
                &m,
                &a,
                &["typescript"],
                "error TS2322: bad type",
                Some("t1"),
            )
        } else {
            green_check(&m, &a, Some("t2"))
        }
    });
    assert_eq!(verdict.blamed[0].pr, 2);
    assert!(!probe_areas.is_empty());
    for pa in probe_areas {
        assert_eq!(pa, vec!["typescript".to_string()]);
    }
}

#[test]
fn test_bisect_two_culprits_in_two_rounds() {
    let st = make_state(&[1, 2, 3, 4], &["rust"]);
    let ctx = VerifyCtx::default();
    let (_, verdict, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if m.contains(&2) || m.contains(&4) {
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "test bad ... FAILED",
                Some("t-red"),
            )
        } else {
            green_check(&m, &["rust"], Some("t-green"))
        }
    });
    assert_eq!(verdict.outcome, Outcome::Green);
    let culprits: Vec<u64> = verdict.blamed.iter().map(|b| b.pr).collect();
    assert_eq!(culprits, vec![2, 4]);
    assert_eq!(
        verdict.passed.keys().copied().collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn test_first_pr_culprit_checks_main_before_blaming_and_broken_main_blames_nobody() {
    let st = make_state(&[1, 2], &["rust"]);
    let ctx = VerifyCtx::default();
    let mut main_probed = false;
    let (_, verdict, _) = run_batch_to_verdict(st.clone(), &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if m.is_empty() {
            main_probed = true;
            return green_check(&[], &["rust"], Some("t-main"));
        }
        red_check(
            &m,
            &["rust"],
            &["rust"],
            "test p1_bad ... FAILED",
            Some("t-1"),
        )
    });
    assert!(main_probed);
    assert_eq!(verdict.blamed[0].pr, 1);

    let (_, v_broken, _) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        red_check(
            &m,
            &["rust"],
            &["rust"],
            "test main_bad ... FAILED",
            Some("t-m"),
        )
    });
    assert_eq!(v_broken.outcome, Outcome::MainBroken);
    assert!(v_broken.blamed.is_empty());
}

#[test]
fn test_flaky_failure_retries_and_gate_or_stopped_areas_are_included() {
    let st = make_state(&[1, 2], &["rust"]);
    let ctx = VerifyCtx::default();
    let mut calls = 0;
    let (_, verdict, actions) = run_batch_to_verdict(st, &ctx, 1000, |members, _, _, _| {
        calls += 1;
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if calls == 1 {
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "test sync::flaky_test ... FAILED",
                Some("t1"),
            )
        } else {
            green_check(&m, &["rust"], Some("t1"))
        }
    });
    assert_eq!(verdict.outcome, Outcome::FlakyGreen);
    assert!(actions
        .iter()
        .any(|a| matches!(a, VerifyAction::RecordFlakes { .. })));

    let mut r = red_check(
        &[1, 2],
        &["gate", "rust", "typescript"],
        &["rust"],
        "test x ... FAILED",
        Some("t1"),
    );
    r.stats.insert(
        "typescript".to_string(),
        AreaResult {
            rc: RC_STOPPED,
            ..Default::default()
        },
    );
    let areas = vec![
        "gate".to_string(),
        "rust".to_string(),
        "typescript".to_string(),
    ];
    assert_eq!(
        retry_areas(&r, &areas, Some("gate")),
        vec!["rust".to_string(), "typescript".to_string()]
    );

    let r_gate = red_check(
        &[1, 2],
        &["gate", "rust", "typescript"],
        &["gate"],
        "clippy failed",
        Some("t1"),
    );
    assert_eq!(retry_areas(&r_gate, &areas, Some("gate")), areas);
}

#[test]
fn test_single_member_skips_rerun_and_bisect_probe_retries_red() {
    let st1 = make_state(&[1], &["rust"]);
    let ctx = VerifyCtx::default();
    let mut calls = 0;
    let (_, v1, _) = run_batch_to_verdict(st1, &ctx, 1000, |members, _, _, _| {
        calls += 1;
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        red_check(
            &m,
            &["rust"],
            &["rust"],
            "test broken ... FAILED",
            Some("t1"),
        )
    });
    assert_eq!(calls, 1);
    assert_eq!(v1.outcome, Outcome::Red);

    let st3 = make_state(&[1, 2, 3], &["rust"]);
    let mut ctx2 = VerifyCtx::default();
    ctx2.cfg.reruns = 2;
    let (_, v3, _) = run_batch_to_verdict(st3, &ctx2, 1000, |members, _, label, _| {
        let m: Vec<u64> = members.iter().map(|m| m.pr).collect();
        if m == vec![1] && !label.ends_with("-retry") {
            return red_check(
                &m,
                &["rust"],
                &["rust"],
                "test flake_in_probe ... FAILED",
                Some("t1"),
            );
        }
        if m == vec![1] && label.ends_with("-retry") {
            return green_check(&m, &["rust"], Some("t1"));
        }
        if m.contains(&3) {
            red_check(
                &m,
                &["rust"],
                &["rust"],
                "test real_bug ... FAILED",
                Some("t3"),
            )
        } else {
            green_check(&m, &["rust"], Some("t-ok"))
        }
    });
    assert_eq!(v3.blamed[0].pr, 3);
}
