//! Comprehensive unit tests for the pure decision engine (`decide::dispatch`,
//! `decide::verify`, `decide::land`, and `decide::watch`).

use crate::decide::dispatch::*;
use crate::decide::land::*;
use crate::decide::verify::*;
use crate::decide::watch::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[test]
fn test_run_lineage_and_resumption() {
    assert_eq!(base_key("auth-fix"), "auth-fix");
    assert_eq!(base_key("auth-fix-r1"), "auth-fix");
    assert_eq!(base_key("auth-fix-r2"), "auth-fix");
    assert_eq!(attempt("auth-fix"), 0);
    assert_eq!(attempt("auth-fix-r1"), 1);
    assert_eq!(run_key("auth-fix-r1", 2), "auth-fix-r2");

    let mut agents = BTreeMap::new();
    agents.insert(
        "auth-fix".to_string(),
        AgentRun {
            key: "auth-fix".to_string(),
            area: "auth".to_string(),
            brief: "Fix login race".to_string(),
            worktree: "/tmp/wt/auth-fix".to_string(),
            started_at: 100,
            finished_at: Some(200),
            exit_code: Some(1),
            ..Default::default()
        },
    );

    let retired = BTreeSet::new();
    let holds = BTreeMap::new();
    let live = BTreeSet::new();
    let todo = Vec::new();

    assert_eq!(
        run_state(&agents, &todo, &retired, &live, "auth-fix"),
        RunState::Resuming
    );
    // Backoff not elapsed at t=250 (needs 200 + 120 = 320)
    assert!(!needs_resume(
        &agents, &todo, &retired, &holds, &live, "auth-fix", 250
    ));
    assert!(needs_resume(
        &agents, &todo, &retired, &holds, &live, "auth-fix", 320
    ));

    let resumed = resume_task(
        agents.get("auth-fix").unwrap(),
        "abc1234 wip commit",
        "M src/lib.rs",
        320,
    );
    assert_eq!(resumed.key, "auth-fix-r1");
    assert!(resumed.brief.contains("Stopped: previous run (auth-fix)"));
    assert!(resumed.brief.contains("abc1234 wip commit"));

    // After r1 also fails, state becomes Stuck
    agents.insert(
        "auth-fix-r1".to_string(),
        AgentRun {
            key: "auth-fix-r1".to_string(),
            area: "auth".to_string(),
            brief: resumed.brief.clone(),
            worktree: "/tmp/wt/auth-fix".to_string(),
            started_at: 325,
            finished_at: Some(400),
            exit_code: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(
        run_state(&agents, &todo, &retired, &live, "auth-fix"),
        RunState::Stuck
    );
    assert_eq!(stuck_runs(&agents, &todo, &retired, &holds, &live).len(), 1);
}

#[test]
fn test_stated_and_guessed_pr_attribution() {
    assert_eq!(stated_pr("all done\nPR: #42\n"), Some(Some(42)));
    assert_eq!(stated_pr("nothing needed\nPR: none\n"), Some(None));
    assert_eq!(stated_pr("mentioned #42 inline"), None);

    let open = vec![PullFacts {
        number: 42,
        branch: "other-branch".to_string(),
        head: "deadbeef".to_string(),
        ..Default::default()
    }];
    // Mentioning #42 without matching branch or `PR: #42` line is only `guessed`
    let (pr, src) = find_pr("my-task", 100, &open, "I saw #42 in logs");
    assert_eq!(pr, Some(42));
    assert_eq!(src.as_deref(), Some("guessed"));

    let mut run = AgentRun {
        key: "my-task".to_string(),
        pr,
        pr_source: src,
        finished_at: Some(200),
        exit_code: Some(0),
        ..Default::default()
    };
    // Guessed PR is NOT trusted by recorded_pr and flags a pr_problem
    assert_eq!(recorded_pr(&run), None);
    assert!(pr_problem(&run, None).is_some());

    // Confirming via attach makes it trusted
    run.pr_source = Some("attached".to_string());
    assert_eq!(recorded_pr(&run), Some(42));
    assert!(pr_problem(&run, None).is_none());
}

#[test]
fn test_area_gate_and_next_work_stacking() {
    let mut agents = BTreeMap::new();
    agents.insert(
        "step-1".to_string(),
        AgentRun {
            key: "step-1".to_string(),
            area: "storage".to_string(),
            started_at: 100,
            finished_at: Some(200),
            exit_code: Some(0),
            pr: Some(10),
            pr_source: Some("stated".to_string()),
            ..Default::default()
        },
    );

    let todo = vec![
        TodoItem {
            key: "step-2".to_string(),
            brief: "Second storage change".to_string(),
            area: "storage".to_string(),
            priority: 0,
            created_at: 150,
            ..Default::default()
        },
        TodoItem {
            key: "step-3".to_string(),
            brief: "Third storage change".to_string(),
            area: "storage".to_string(),
            priority: 0,
            created_at: 160,
            ..Default::default()
        },
    ];

    let retired = BTreeSet::new();
    let holds = BTreeMap::new();
    let live = BTreeSet::new();
    let mut passed = BTreeSet::new();
    let failed = BTreeSet::new();
    let merged = BTreeSet::new();
    let closed = BTreeSet::new();

    // While PR #10 has not passed batch verification, storage area is gated
    let (chosen, _) = next_work(
        &todo, &agents, &retired, &holds, &live, &passed, &failed, &merged, &closed, 4,
    );
    assert!(chosen.is_empty());

    // Once PR #10 passes, at most ONE task in the `storage` area starts, stacked on PR #10
    passed.insert(10);
    let (chosen, rem) = next_work(
        &todo, &agents, &retired, &holds, &live, &passed, &failed, &merged, &closed, 4,
    );
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].key, "step-2");
    assert_eq!(chosen[0].base.as_deref(), Some("10"));
    assert_eq!(rem.len(), 1);
    assert_eq!(rem[0].key, "step-3");
}

#[test]
fn test_compare_expansion() {
    let item = TodoItem {
        key: "bench".to_string(),
        brief: "Optimize parser".to_string(),
        area: "core".to_string(),
        compare: Some("opus,codex,gemini".to_string()),
        ..Default::default()
    };
    let expanded = compare_items(&item, Some("high")).unwrap();
    assert_eq!(expanded.len(), 3);
    assert_eq!(expanded[0].key, "bench-opus");
    assert_eq!(expanded[0].provider.as_deref(), Some("claude"));
    assert_eq!(expanded[0].area, ""); // Cleared so compare variants run in parallel
    assert_eq!(expanded[1].key, "bench-codex");
    assert_eq!(expanded[1].provider.as_deref(), Some("codex"));
    assert_eq!(expanded[2].key, "bench-gemini");
    assert_eq!(expanded[2].provider.as_deref(), Some("gemini"));
}

#[test]
fn test_stack_ordering_and_whole_stack_cap() {
    let open = vec![
        PullFacts {
            number: 1,
            branch: "feat-1".to_string(),
            base: "main".to_string(),
            ..Default::default()
        },
        PullFacts {
            number: 2,
            branch: "feat-2".to_string(),
            base: "feat-1".to_string(),
            ..Default::default()
        },
        PullFacts {
            number: 3,
            branch: "feat-3".to_string(),
            base: "feat-2".to_string(),
            ..Default::default()
        },
        PullFacts {
            number: 4,
            branch: "solo-4".to_string(),
            base: "main".to_string(),
            ..Default::default()
        },
    ];
    let pmap = parents(&open);
    assert_eq!(root(3, &pmap), 1);
    assert_eq!(ordered(&[3, 1, 2], &pmap), vec![1, 2, 3]);

    // Capping [4, 1, 2, 3] with limit 2:
    // Child 2 rides with parent 1, and child 3 rides with parent 2 so the whole stack [1, 2, 3] rides together!
    assert_eq!(cap(&[3, 2, 1], &pmap, 2), vec![1, 2, 3]);
    // With limit 3 on [3, 2, 1, 4], the stack [1, 2, 3] fills the cap and solo 4 waits
    assert_eq!(cap(&[3, 2, 1, 4], &pmap, 3), vec![1, 2, 3]);
}

#[test]
fn test_bisect_checks_main_before_blaming_first_member() {
    let members = vec![
        (10, "sha10".to_string()),
        (11, "sha11".to_string()),
        (12, "sha12".to_string()),
        (13, "sha13".to_string()),
    ];
    let mut b = Bisect {
        members: members.clone(),
        results: BTreeMap::from([(4, false)]),
    };
    // Midpoint probe is 2
    assert_eq!(next_probe(&b), ProbeStep::Probe(2));
    // Suppose prefix 2 also fails -> probes 1
    b.results.insert(2, false);
    assert_eq!(next_probe(&b), ProbeStep::Probe(1));
    // Prefix 1 fails -> MUST probe 0 (main alone) before blaming PR #10!
    b.results.insert(1, false);
    assert_eq!(next_probe(&b), ProbeStep::Probe(0));
    // If main passes, PR #10 is the culprit
    b.results.insert(0, true);
    assert_eq!(
        next_probe(&b),
        ProbeStep::Culprit((10, "sha10".to_string()))
    );

    // If main fails, report MainBroken instead of blaming PR #10
    b.results.insert(0, false);
    assert_eq!(next_probe(&b), ProbeStep::MainBroken);
}

#[test]
fn test_classify_code_beats_infra_noise() {
    let pure_infra = "curl: (7) Failed to connect: ECONNREFUSED\n503 Service Unavailable";
    assert_eq!(classify(pure_infra), "infrastructure");

    let mixed =
        "warning: ECONNREFUSED on optional telemetry\ntest decide::tests::my_test ... FAILED\n";
    assert_eq!(classify(mixed), "code");

    let proptest_log = "503 Service Unavailable\nminimal failing input: x = 0\n";
    assert_eq!(classify(proptest_log), "code");
    assert_eq!(
        counterexample(proptest_log).as_deref(),
        Some("minimal failing input: x = 0")
    );
}

#[test]
fn test_flake_ledger_auto_queues_task_at_threshold() {
    let ledger = BTreeMap::new();
    let tests = vec!["recorder::tests::flaky_wal".to_string()];
    let l1 = record_flakes(
        &ledger,
        &tests,
        "core",
        "b001",
        "test recorder::tests::flaky_wal ... FAILED",
        100,
    );
    let todo = Vec::new();
    let agents = BTreeMap::new();
    let retired = BTreeSet::new();

    // 1 flake is below threshold (2)
    let (_, tasks1) = flake_tasks(&l1, &todo, &agents, &retired, 100);
    assert!(tasks1.is_empty());

    // 2nd flake crosses threshold and queues a fix task
    let l2 = record_flakes(
        &l1,
        &tests,
        "core",
        "b002",
        "test recorder::tests::flaky_wal ... FAILED",
        200,
    );
    let (l2_updated, tasks2) = flake_tasks(&l2, &todo, &agents, &retired, 200);
    assert_eq!(tasks2.len(), 1);
    assert_eq!(tasks2[0].area, "core");
    assert!(tasks2[0].brief.contains("tacho starve"));

    // Calling again does not duplicate the task once queued
    let (_, tasks3) = flake_tasks(&l2_updated, &tasks2, &agents, &retired, 210);
    assert!(tasks3.is_empty());
}

#[test]
fn test_pr_preflight_refusals_and_proof_validation() {
    let proof = ProofSummary {
        head: "abc123456789".to_string(),
        tree: "tree11111111".to_string(),
        base: "base00000000".to_string(),
        ok: true,
        timestamp: 1000,
        changed_files: vec!["crates/tacho-core/src/lib.rs".to_string()],
        checks: vec![ProofCheck {
            name: "test".to_string(),
            cmd: "cargo test".to_string(),
            ok: true,
            duration_ms: 42,
            output_tail: "ok".to_string(),
        }],
        screenshots: Vec::new(),
    };

    // Valid when tree matches and no forbidden trailers exist
    let ok_refusals = pr_refusals(
        "Add pure decision engine",
        "Implements dispatch, verify, land, and watch modules.",
        &["abc1234 Add pure decision engine".to_string()],
        &[],
        &["crates/tacho-core/src/lib.rs".to_string()],
        &[],
        "abc123456789",
        "tree11111111",
        Some(&proof),
        &[],
    );
    assert!(
        ok_refusals.is_empty(),
        "expected no refusals, got {ok_refusals:?}"
    );

    // Refuses stale tree, dirty files, merge commits, and Co-Authored-By
    let bad_refusals = pr_refusals(
        "Add feature",
        "Summary\n\nCo-Authored-By: Someone <a@b.com>",
        &["abc1234 Add feature".to_string()],
        &["def5678 Merge branch 'main'".to_string()],
        &["crates/tacho-core/src/lib.rs".to_string()],
        &["M Cargo.toml".to_string()],
        "abc123456789",
        "different_tree_hash",
        Some(&proof),
        &[],
    );
    assert!(bad_refusals.iter().any(|r| r.contains("merge commit")));
    assert!(bad_refusals
        .iter()
        .any(|r| r.contains("uncommitted changes")));
    assert!(bad_refusals.iter().any(|r| r.contains("proof is stale")));
    assert!(bad_refusals.iter().any(|r| r.contains("Co-Authored-By")));
}

#[test]
fn test_land_still_ready_and_train_tree_verification() {
    let pr = PullFacts {
        number: 11,
        state: "OPEN".to_string(),
        mergeable: "MERGEABLE".to_string(),
        head: "sha11".to_string(),
        ..Default::default()
    };
    let rec = PassedRecord {
        pr: 11,
        sha: "sha11".to_string(),
        main_sha: "main0".to_string(),
        batch_id: "b001".to_string(),
        passed_at: 100,
        tree: "tree_b001".to_string(),
        areas: vec!["core".to_string()],
    };
    let batch = BatchRun {
        id: "b001".to_string(),
        prs: vec![10, 11],
        shas: BTreeMap::from([(10, "sha10".to_string()), (11, "sha11".to_string())]),
        main_sha: "main0".to_string(),
        status: "passed".to_string(),
        tree: Some("tree_b001".to_string()),
        ..Default::default()
    };
    let holds = BTreeMap::new();

    // Still ready if main moved only because earlier batch member #10 merged
    assert!(still_ready(
        &pr,
        &rec,
        Some(&batch),
        &holds,
        "main_after_10",
        &[10]
    ));
    // Not ready if an unrelated PR #99 merged onto main
    assert!(!still_ready(
        &pr,
        &rec,
        Some(&batch),
        &holds,
        "main_after_99",
        &[10, 99]
    ));

    let train = make_train(&[], &batch, "main0", "tree_b001", 200).unwrap();
    let after_10 = after_outside_merge(&train, 10, "main_after_10");
    assert_eq!(after_10.status, "open");
    assert_eq!(after_10.landed_prs, vec![10]);
    assert_eq!(mismatch(&after_10, "tree_b001"), None);
    assert!(mismatch(&after_10, "wrong_tree").is_some());
}

#[test]
fn test_drop_breakers_bisects_compile_culprit_and_descendants() {
    let parent_map = BTreeMap::from([(2, 1)]);
    // PR #2 fails to compile; PR #1 and #3 compile cleanly
    let (surviving, dropped) = drop_breakers(&[1, 2, 3], &parent_map, |slice| {
        if slice.contains(&2) {
            Err("error[E0425]: cannot find value `broken` in PR 2".to_string())
        } else {
            Ok(())
        }
    });
    assert_eq!(surviving, vec![1, 3]);
    assert!(dropped.contains_key(&2));
}

#[test]
fn test_watch_verdicts_supervision_starve_and_tidy() {
    let s0 = ProgressSample {
        ts: 0,
        log_bytes: 1000,
        head_sha: "a".to_string(),
        dirty_files: 0,
        cpu_ticks: 500,
    };
    let s1 = ProgressSample {
        ts: 2000,
        log_bytes: 1000,
        head_sha: "a".to_string(),
        dirty_files: 0,
        cpu_ticks: 500,
    };
    // 2000s (>= 1800s) with 0 log growth and 0 CPU delta -> HUNG -> Resume
    let (v_hung, idle) = verdict(&[s0.clone(), s1], 2000);
    assert_eq!(v_hung, Verdict::Hung);
    assert!(matches!(
        supervise_step("w1", v_hung, idle, false),
        SuperviseAction::Resume { .. }
    ));

    // 3000s (>= 2700s) with CPU delta > 0 but 0 work -> STALLED -> Nudge then Escalate
    let s2 = ProgressSample {
        ts: 3000,
        log_bytes: 1000,
        head_sha: "a".to_string(),
        dirty_files: 0,
        cpu_ticks: 900,
    };
    let (v_stalled, idle2) = verdict(&[s0, s2], 3000);
    assert_eq!(v_stalled, Verdict::Stalled);
    assert!(matches!(
        supervise_step("w1", v_stalled, idle2, false),
        SuperviseAction::Nudge { .. }
    ));
    assert!(matches!(
        supervise_step("w1", v_stalled, idle2, true),
        SuperviseAction::Escalate { .. }
    ));

    // Starve planning refuses when a batch is running and caps to 1/8th of CPUs
    assert!(plan_starve(16, None, None, true, 60).is_err());
    let plan = plan_starve(16, None, None, false, 60).unwrap();
    assert_eq!(plan.cpus, vec![14, 15]);
    assert_eq!(plan.burners, 8);

    // Tidy respects live worktrees and sets rescue=true on dirty/unpushed trees
    let snap = TidySnapshot {
        now: 10_000,
        worktrees: vec![
            WorktreeCandidate {
                path: "/wt/live".to_string(),
                branch: "live".to_string(),
                age_secs: 9_000,
                dirty: false,
                unpushed_commits: 0,
                pr_merged_or_closed: false,
            },
            WorktreeCandidate {
                path: "/wt/dirty-old".to_string(),
                branch: "dirty-old".to_string(),
                age_secs: 9_000,
                dirty: true,
                unpushed_commits: 1,
                pr_merged_or_closed: false,
            },
        ],
        live_agent_worktrees: BTreeSet::from(["/wt/live".to_string()]),
        ..Default::default()
    };
    let decisions = decide_tidy(&snap, 3_600);
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions[0],
        TidyDecision::RemoveTree {
            path: "/wt/dirty-old".to_string(),
            rescue: true,
            because: "worktree idle for 9000s >= 3600s".to_string(),
        }
    );
    assert!(owns(
        Path::new("/wt/live/src/lib.rs"),
        Path::new("/wt/live")
    ));
    assert!(!owns(
        Path::new("/wt/live2/src/lib.rs"),
        Path::new("/wt/live")
    ));
}

#[test]
fn test_parity_edge_cases_cap_flake_key_retire_env_and_guardrails() {
    // 1. cap: child rides with parent past limit, and child whose parent was left out waits
    let up = BTreeMap::from([(7, 6)]);
    assert_eq!(
        cap(&[1, 2, 3, 4, 5, 6, 7], &up, 6),
        vec![1, 2, 3, 4, 5, 6, 7]
    );
    let up2 = BTreeMap::from([(8, 7)]);
    assert_eq!(
        cap(&[1, 2, 3, 4, 5, 6, 7, 8], &up2, 6),
        vec![1, 2, 3, 4, 5, 6]
    );

    // 2. flake_key: long names sharing a 40+ char prefix get distinct hash suffixes
    let k_alpha =
        flake_key("pool::tests::rate_locking_is_kept_under_load_when_the_redis_pool_is_full_alpha");
    let k_beta =
        flake_key("pool::tests::rate_locking_is_kept_under_load_when_the_redis_pool_is_full_beta");
    assert_ne!(k_alpha, k_beta);

    // 3. record_flakes: idempotent when called twice for the same batch_id
    let ledger = BTreeMap::new();
    let l1 = record_flakes(
        &ledger,
        &["pool::tests::lock".to_string()],
        "core",
        "b001",
        "test pool::tests::lock ... FAILED",
        100,
    );
    let l2 = record_flakes(
        &l1,
        &["pool::tests::lock".to_string()],
        "core",
        "b001",
        "test pool::tests::lock ... FAILED",
        200,
    );
    assert_eq!(l2["pool::tests::lock"].count, 1);
    assert_eq!(l2["pool::tests::lock"].last_seen, 100);

    // 4. failed_tests: parses Rust, Gradle/JUnit, and Playwright
    let multi_log = "\
test billing::tests::charges_once ... ok
test billing::tests::refunds_twice ... FAILED
NumbersFlowTest > a_number_is_registered FAILED
  ✘   40 [chromium] › e2e/console.spec.ts:1013:3 › the audit log (7.2s)
";
    assert_eq!(
        failed_tests(multi_log),
        vec![
            "NumbersFlowTest > a_number_is_registered".to_string(),
            "billing::tests::refunds_twice".to_string(),
            "e2e/console.spec.ts:1013:3 › the audit log".to_string(),
        ]
    );

    // 5. retire: strips retired key from downstream todo.after lists
    let todo = vec![
        TodoItem {
            key: "step-1".to_string(),
            brief: "First".to_string(),
            area: "core".to_string(),
            ..Default::default()
        },
        TodoItem {
            key: "step-2".to_string(),
            brief: "Second".to_string(),
            area: "core".to_string(),
            after: vec!["step-1".to_string()],
            ..Default::default()
        },
    ];
    let (new_todo, new_retired, _) = retire(
        "step-1",
        &todo,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    assert!(new_retired.contains("step-1"));
    assert_eq!(new_todo.len(), 1);
    assert!(new_todo[0].after.is_empty());

    // 6. env_file / parse_env: roundtrips embedded single quotes
    let env_txt = env_file(
        "my-slug",
        "deadbeef",
        Some(42),
        20000,
        20019,
        Some("/tmp/it's/target"),
    );
    let parsed = parse_env(&env_txt);
    assert_eq!(parsed.get("SLUG").map(String::as_str), Some("my-slug"));
    assert_eq!(
        parsed.get("CARGO_TARGET_DIR").map(String::as_str),
        Some("/tmp/it's/target")
    );

    // 7. proof_plan: scopes leaf module edits to `--lib -- <module>::` and test suites to `--test <suite>`
    let plan = proof_plan(
        &[
            "crates/tacho-core/src/decide/verify.rs".to_string(),
            "crates/tacho-core/tests/oracle.rs".to_string(),
        ],
        &[],
    );
    assert!(plan
        .iter()
        .any(|(_, cmd)| cmd == "cargo fmt --all -- --check"));
    assert!(plan
        .iter()
        .any(|(_, cmd)| cmd == "cargo test -p tacho-core --lib -- decide::verify::"));
    assert!(plan
        .iter()
        .any(|(_, cmd)| cmd == "cargo test -p tacho-core --test oracle"));

    // 8. burner_command & guard_bash_tool & unknown model pricing
    assert_eq!(
        burner_command("timeout", "timeout -k 5 30 /usr/bin/yes").as_deref(),
        Some("yes")
    );
    assert!(guard_bash_tool(true, "cargo test", false).is_err());
    assert!(guard_bash_tool(false, "yes > /dev/null", false).is_err());
    assert!(guard_bash_tool(false, "yes > /dev/null", true).is_ok());
    assert!(guard_bash_tool(false, "git stash", false).is_err());
    assert!(guard_bash_tool(false, "git commit --no-verify -m 'wip'", false).is_err());
    assert_eq!(price("unknown-custom-model", 100_000, 50_000, 0, 0), 0.0);
    assert!(queue_grew(&[10, 11], &[10, 11, 12]));
    assert!(!queue_grew(&[10, 11], &[10]));
}
