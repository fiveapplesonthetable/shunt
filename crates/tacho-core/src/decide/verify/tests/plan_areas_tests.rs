//! Parity tests for `test_plan.py` (cases 127–162), `test_train_areas.py` (cases 179–187),
//! and `test_failures.py` (cases 188–194).

use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// --- Cases 127–140: group_of, touches_ci, stack_parents, ordered, cap ---

#[test]
fn test_group_of_routes_by_configured_rules() {
    let mut cfg = VerifyConfig::default();
    cfg.group_ignore = set(&["gate", "fuzz"]);
    cfg.groups = vec![
        GroupRule {
            name: "android".to_string(),
            when: GroupMatch::SupersetOf(set(&["android"])),
        },
        GroupRule {
            name: "console".to_string(),
            when: GroupMatch::SupersetOf(set(&["console"])),
        },
        GroupRule {
            name: "rust".to_string(),
            when: GroupMatch::SubsetOf(set(&["rust", "specs"])),
        },
        GroupRule {
            name: "ts".to_string(),
            when: GroupMatch::NonEmptySubsetOf(set(&["typescript", "schema"])),
        },
    ];
    cfg.fallback_group = "mixed".to_string();

    assert_eq!(group_of(&set(&["gate", "rust"]), &cfg), "rust");
    assert_eq!(group_of(&set(&["gate"]), &cfg), "rust");
    assert_eq!(
        group_of(&set(&["gate", "typescript", "console"]), &cfg),
        "console"
    );
    assert_eq!(
        group_of(&set(&["gate", "rust", "android"]), &cfg),
        "android"
    );
    assert_eq!(
        group_of(&set(&["gate", "typescript", "schema"]), &cfg),
        "ts"
    );
    assert_eq!(
        group_of(&set(&["gate", "rust", "typescript"]), &cfg),
        "mixed"
    );
}

#[test]
fn test_touches_ci_detects_workflow_and_dockerfile_changes() {
    let cfg = VerifyConfig::default();
    assert!(touches_ci(&[".github/workflows/ci.yml".to_string()], &cfg));
    assert!(touches_ci(&["docker/ci.Dockerfile".to_string()], &cfg));
    assert!(!touches_ci(&["crates/core/src/lib.rs".to_string()], &cfg));
}

#[test]
fn test_stack_parents_nearest_ancestor_and_tiebreak() {
    let prs = vec![
        (10, "sha10".to_string()),
        (20, "sha20".to_string()),
        (30, "sha30".to_string()),
    ];
    // 10 -> 20 -> 30 chain:
    let up = stack_parents(&prs, |anc, desc| {
        matches!(
            (anc, desc),
            ("sha10", "sha20") | ("sha10", "sha30") | ("sha20", "sha30")
        )
    });
    assert_eq!(up.get(&10), None);
    assert_eq!(up.get(&20), Some(&10));
    assert_eq!(up.get(&30), Some(&20));

    // Tiebreak when two PRs are at identical depth: lower PR number wins regardless of order
    let prs_tie = vec![
        (15, "sha15".to_string()),
        (12, "sha12".to_string()),
        (40, "sha40".to_string()),
    ];
    let up_tie = stack_parents(&prs_tie, |anc, desc| {
        matches!((anc, desc), ("sha15", "sha40") | ("sha12", "sha40"))
    });
    assert_eq!(up_tie.get(&40), Some(&12));
}

#[test]
fn test_ordered_and_cap_preserve_stack_integrity() {
    let mut up = BTreeMap::new();
    up.insert(20, 10);
    up.insert(30, 20);
    let ord = ordered(&[30, 10, 20, 5], &up);
    assert_eq!(ord, vec![5, 10, 20, 30]);

    // cap keeps a stack whole (children whose parent is in `out` ride along even past `limit`),
    // while unrelated PRs past `limit` wait:
    assert_eq!(cap(&[10, 20, 30, 40], &up, 2), vec![10, 20, 30]);
    // If parent 20 is NOT in `out` (because 5 and 6 filled limit 2), child 30 also waits:
    let mut up2 = BTreeMap::new();
    up2.insert(30, 20);
    assert_eq!(cap(&[5, 6, 20, 30], &up2, 2), vec![5, 6]);
}

// --- Cases 141–153: plan_batches, needs_check_at, covered, batch_id_for, claim_batch ---

#[test]
fn test_plan_batches_groups_stack_with_parent_and_isolates_ci() {
    let cfg = VerifyConfig::default();
    let mut up = BTreeMap::new();
    up.insert(11, 10);
    let waiting = vec![
        Waiting {
            pr: 10,
            sha: "s10".to_string(),
            areas: set(&["rust"]),
            ci: false,
        },
        Waiting {
            pr: 11,
            sha: "s11".to_string(),
            areas: set(&["console"]),
            ci: false,
        },
        Waiting {
            pr: 12,
            sha: "s12".to_string(),
            areas: set(&["rust"]),
            ci: true,
        },
        Waiting {
            pr: 13,
            sha: "s13".to_string(),
            areas: set(&["rust"]),
            ci: true,
        },
    ];
    let planned = plan_batches(&waiting, &up, &BTreeSet::new(), 4, &cfg);
    assert_eq!(planned.len(), 2);
    // Stack [10, 11] rides together in "core" (default fallback):
    assert_eq!(planned[0].members, vec![10, 11]);
    // CI group takes only 1 PR:
    let ci_batch = planned.iter().find(|b| b.group == "ci").unwrap();
    assert_eq!(ci_batch.members, vec![12]);
}

#[test]
fn test_needs_check_at_and_covered_require_every_non_skipped_area() {
    let touched = set(&["gate", "rust", "console"]);
    let skipped = set(&["browser"]);
    assert!(!covered(&touched, &set(&["gate", "rust"]), &skipped));
    assert!(covered(
        &touched,
        &set(&["gate", "rust", "console"]),
        &skipped
    ));

    // Passed at same sha with partial areas -> still needs check:
    let partial = set(&["gate", "rust"]);
    assert!(needs_check_at(
        "sha1",
        &touched,
        Some(("sha1", &partial)),
        None,
        &skipped
    ));
    // Passed at same sha with full areas -> does not need check:
    let full = set(&["gate", "rust", "console"]);
    assert!(!needs_check_at(
        "sha1",
        &touched,
        Some(("sha1", &full)),
        None,
        &skipped
    ));
    // Failed at same sha -> does not re-queue until sha moves:
    assert!(!needs_check_at(
        "sha1",
        &touched,
        None,
        Some("sha1"),
        &skipped
    ));
    assert!(needs_check_at(
        "sha2",
        &touched,
        None,
        Some("sha1"),
        &skipped
    ));
}

#[test]
fn test_new_batch_id_appends_counter_on_collision() {
    let taken = set(&["rust-100", "rust-100-1"]);
    assert_eq!(batch_id_for("rust", 100, &taken), "rust-100-2");
    assert_eq!(batch_id_for("rust", 101, &taken), "rust-101");
}

#[test]
fn test_claim_batch_attaches_to_live_same_prs_or_allocates_new() {
    let mut batches = BTreeMap::new();
    let d1 = claim_batch(
        &mut batches,
        "rust",
        &[10, 20],
        &["rust".to_string()],
        1000,
        4242,
        |_| true,
    );
    assert!(!d1.attached_existing);
    assert_eq!(d1.batch_id, "rust-1000");

    // Second caller with same PR set while PID 4242 is alive attaches to rust-1000:
    let d2 = claim_batch(
        &mut batches,
        "rust",
        &[20, 10],
        &["rust".to_string()],
        1005,
        4343,
        |pid| pid == 4242,
    );
    assert!(d2.attached_existing);
    assert_eq!(d2.batch_id, "rust-1000");

    // If PID 4242 is dead, allocates a new batch:
    let d3 = claim_batch(
        &mut batches,
        "rust",
        &[10, 20],
        &["rust".to_string()],
        1000,
        4444,
        |_| false,
    );
    assert!(!d3.attached_existing);
    assert_eq!(d3.batch_id, "rust-1000-1");

    // mark_batch_lost marks it done/lost:
    mark_batch_lost(&mut batches, &d3.batch_id, 1002);
    assert_eq!(batches[&d3.batch_id].status, "done");
    assert_eq!(batches[&d3.batch_id].outcome.as_deref(), Some("lost"));
}

// --- Cases 154–162: ready.rs (pr_still_ready, ready_queue_order, queue_grew, land_cmd, pr_standing) ---

#[test]
fn test_pr_still_ready_rechecks_live_head_draft_hold_files_and_proof() {
    let mut cfg = VerifyConfig::default();
    cfg.areas = demo_area_cfg();
    let skipped = BTreeSet::new();

    let obs = ReadyPrObs {
        pr: 10,
        open: true,
        draft: false,
        head: "sha10".to_string(),
        areas: set(&["rust"]),
        files: Some(vec!["crates/core/src/lib.rs".to_string()]),
        parent: None,
        held: false,
        blocked: false,
    };
    let mut pass = ReadyPass {
        sha: "sha10".to_string(),
        areas: set(&["gate", "rust"]),
        batch_id: "rust-100".to_string(),
        main_sha: "m1".to_string(),
        tree: Some("t1".to_string()),
        proved_by: [
            ("gate".to_string(), "r1".to_string()),
            ("rust".to_string(), "r1".to_string()),
        ]
        .into_iter()
        .collect(),
    };
    assert!(pr_still_ready(&obs, Some(&pass), &skipped, &cfg).0);

    // Moved head -> false:
    let mut moved = obs.clone();
    moved.head = "sha10-new".to_string();
    assert!(!pr_still_ready(&moved, Some(&pass), &skipped, &cfg).0);

    // Empty file list -> false (refuses to land when diff is empty):
    let mut empty_files = obs.clone();
    empty_files.files = Some(Vec::new());
    assert!(!pr_still_ready(&empty_files, Some(&pass), &skipped, &cfg).0);

    // Missing area proof in proved_by -> false:
    pass.proved_by.remove("rust");
    assert!(!pr_still_ready(&obs, Some(&pass), &skipped, &cfg).0);
}

#[test]
fn test_ready_queue_order_and_pr_standing() {
    let cfg = VerifyConfig::default();
    let skipped = BTreeSet::new();
    let mut passes = BTreeMap::new();
    passes.insert(
        10,
        ReadyPass {
            sha: "s10".to_string(),
            areas: set(&["rust"]),
            batch_id: "b1".to_string(),
            ..Default::default()
        },
    );
    passes.insert(
        20,
        ReadyPass {
            sha: "s20".to_string(),
            areas: set(&["rust"]),
            batch_id: "b1".to_string(),
            ..Default::default()
        },
    );
    let c10 = ReadyPrObs {
        pr: 10,
        open: true,
        head: "s10".to_string(),
        areas: set(&["rust"]),
        ..Default::default()
    };
    let c20 = ReadyPrObs {
        pr: 20,
        open: true,
        head: "s20".to_string(),
        areas: set(&["rust"]),
        parent: Some(10),
        ..Default::default()
    };
    let open: BTreeSet<u64> = [10, 20].into_iter().collect();
    assert_eq!(
        ready_queue_order(&[c20.clone(), c10.clone()], &passes, &open, &skipped, &cfg),
        vec![10, 20]
    );
    // If parent 10 is open but NOT passed, child 20 cannot land before parent:
    let mut passes_only_20 = BTreeMap::new();
    passes_only_20.insert(20, passes[&20].clone());
    assert!(
        ready_queue_order(&[c20.clone(), c10], &passes_only_20, &open, &skipped, &cfg).is_empty()
    );

    // ready_queue_grew & land_cmd:
    assert!(ready_queue_grew(&[10], &[10, 20], &BTreeSet::new()));
    assert!(!ready_queue_grew(&[10], &[10], &BTreeSet::new()));
    assert_eq!(
        land_cmd("tacho", 10, Some("s10")),
        vec!["tacho", "land", "10", "--expect-head", "s10"]
    );

    // pr_standing when waiting on parent:
    let st = pr_standing(
        &c20,
        None,
        None,
        None,
        passes.get(&20),
        None,
        None,
        &open,
        false,
        &skipped,
        &cfg,
    );
    assert_eq!(st, PrStanding::WaitingParent { parent: 10 });
}

// --- Cases 179–187: test_train_areas.py (select_areas, check_areas, recompute_checked_areas) ---

#[test]
fn test_select_areas_and_check_areas_pruning_and_unplaced() {
    let cfg = demo_area_cfg();
    let skipped = BTreeSet::new();

    // Rust change on Train tier:
    let sel_rust = select_areas(&["crates/core/src/lib.rs".to_string()], Tier::Train, &cfg);
    assert_eq!(sel_rust.areas, set(&["gate", "rust"]));
    assert!(sel_rust.unplaced.is_empty());

    // Console change on Train vs Change tier:
    let files_ui = vec!["apps/console/src/App.tsx".to_string()];
    let sel_ui_train = select_areas(&files_ui, Tier::Train, &cfg);
    assert_eq!(
        sel_ui_train.areas,
        set(&["gate", "typescript", "console", "browser"])
    );
    let chk_ui_change = check_areas(
        &select_areas(&files_ui, Tier::Change, &cfg).areas,
        &files_ui,
        &skipped,
        false,
        &cfg,
    );
    assert!(chk_ui_change.contains("pixel"));

    // Docs-only change selects empty -> recompute_checked_areas runs [gate] only (R16):
    let doc_files = vec!["docs/guide.md".to_string()];
    let sel_doc = select_areas(&doc_files, Tier::Train, &cfg);
    assert!(sel_doc.areas.is_empty());
    let recomputed = recompute_checked_areas(
        &["rust".to_string()],
        &[doc_files],
        &skipped,
        &cfg,
        |f, t| select_areas(f, t, &cfg).areas,
    );
    assert_eq!(recomputed, vec!["gate".to_string()]);

    // specs/README.md matches specs rule, but check_areas prunes "specs" when no .tla/.cfg/.lean file is touched:
    let spec_doc = vec!["specs/README.md".to_string()];
    let sel_spec = select_areas(&spec_doc, Tier::Train, &cfg);
    assert!(sel_spec.areas.contains("specs"));
    let chk_spec = check_areas(&sel_spec.areas, &spec_doc, &skipped, true, &cfg);
    assert_eq!(chk_spec, set(&["gate"]));

    // Unplaced file on Train tier runs gate + rust and records path in unplaced:
    let mystery = vec!["mystery/new_top_level.xyz".to_string()];
    let sel_mys = select_areas(&mystery, Tier::Train, &cfg);
    assert_eq!(sel_mys.areas, set(&["gate", "rust"]));
    assert_eq!(sel_mys.unplaced, mystery);

    // Unplaced file on Change tier fails closed and selects all areas:
    let sel_mys_change = select_areas(&mystery, Tier::Change, &cfg);
    assert_eq!(sel_mys_change.areas, cfg.all_areas);
}

// --- Cases 188–194: test_failures.py (flake_key, lines_about, record_flakes, plan_flake_tasks, prune_stale_records) ---

#[test]
fn test_flake_key_disambiguates_long_names_with_sha1() {
    let k1 = flake_key("crates::very_long_module_name_that_exceeds_forty_characters::test_alpha");
    let k2 = flake_key("crates::very_long_module_name_that_exceeds_forty_characters::test_beta");
    assert!(k1.starts_with("flake-"));
    assert_ne!(k1, k2);
}

#[test]
fn test_lines_about_matches_leaf_test_segment_and_caps_at_max() {
    let log = (1..=20)
        .map(|i| format!("line {i}: failure in my_leaf_test"))
        .collect::<Vec<_>>()
        .join("\n");
    let excerpt = lines_about(&log, "module::submodule::my_leaf_test", 12);
    assert_eq!(excerpt.lines().count(), 12);
    assert!(excerpt.contains("my_leaf_test"));
}

#[test]
fn test_record_flakes_and_plan_flake_tasks_threshold_and_dedupe() {
    let cfg = VerifyConfig::default();
    let log = "test mod::flaky_one ... FAILED\n";
    let mut ledger = BTreeMap::new();
    // 1st batch:
    ledger = record_flakes(
        &ledger,
        &["mod::flaky_one".to_string()],
        "rust",
        "b1",
        log,
        100,
    );
    // Duplicate batch_id b1 does not increment count:
    ledger = record_flakes(
        &ledger,
        &["mod::flaky_one".to_string()],
        "rust",
        "b1",
        log,
        101,
    );
    assert_eq!(ledger["mod::flaky_one"].count, 1);

    let (_, tasks_at_1) =
        plan_flake_tasks(&ledger, &[], &BTreeMap::new(), &BTreeSet::new(), 101, &cfg);
    assert!(tasks_at_1.is_empty());

    // 2nd distinct batch reaches threshold 2:
    ledger = record_flakes(
        &ledger,
        &["mod::flaky_one".to_string()],
        "rust",
        "b2",
        log,
        102,
    );
    let (ledger2, tasks_at_2) =
        plan_flake_tasks(&ledger, &[], &BTreeMap::new(), &BTreeSet::new(), 102, &cfg);
    assert_eq!(tasks_at_2.len(), 1);
    assert_eq!(tasks_at_2[0].area, "rust");
    assert_eq!(tasks_at_2[0].model.as_deref(), Some("sonnet"));
    assert!(tasks_at_2[0].brief.contains("never by a longer timeout"));
    assert!(tasks_at_2[0].brief.contains("10 times under 4 busy loops"));
    assert!(tasks_at_2[0].brief.contains("draft PR"));

    // Subsequent call when task is in todo or retired does not duplicate:
    let (_, dup) = plan_flake_tasks(
        &ledger2,
        &tasks_at_2,
        &BTreeMap::new(),
        &BTreeSet::new(),
        103,
        &cfg,
    );
    assert!(dup.is_empty());
}

#[test]
fn test_prune_stale_records_drops_closed_or_moved_heads() {
    let mut failed = BTreeMap::new();
    failed.insert(
        10,
        FailedRecord {
            pr: 10,
            sha: "sha10".to_string(),
            batch_id: "b1".to_string(),
            summary: "failed: rust".to_string(),
            failed_at: 100,
            kind: "code".to_string(),
            failed_tests: Vec::new(),
            counterexample: None,
            log: String::new(),
        },
    );
    failed.insert(
        20,
        FailedRecord {
            pr: 20,
            sha: "sha20-old".to_string(),
            batch_id: "b1".to_string(),
            summary: "failed: rust".to_string(),
            failed_at: 100,
            kind: "code".to_string(),
            failed_tests: Vec::new(),
            counterexample: None,
            log: String::new(),
        },
    );
    let untied = vec![UntiedRecord {
        prs: vec![10, 20],
        shas: [(10, "sha10".to_string()), (20, "sha20-old".to_string())]
            .into_iter()
            .collect(),
        batch_id: "b1".to_string(),
        summary: "failed: rust".to_string(),
        at: 100,
    }];
    let open_heads: BTreeMap<u64, String> =
        [(10, "sha10".to_string()), (20, "sha20-new".to_string())]
            .into_iter()
            .collect();
    let (next_failed, next_untied, changed) = prune_stale_records(&failed, &untied, &open_heads);
    assert!(changed);
    assert_eq!(next_failed.keys().copied().collect::<Vec<_>>(), vec![10]);
    assert!(next_untied.is_empty());
}

// =============================================================================
// Costly Area Selection & Related Unit Tests (`test_selection.py`, `test_related.py`)
// =============================================================================

fn eval_costly(paths: &[&str], pull_request: bool, train: bool) -> BTreeSet<String> {
    let mut found = vec!["secrets"];
    for &p in paths {
        if p.starts_with("frontend/admin/") {
            found.push("console");
        }
        if p.starts_with("frontend/android/") || p == ".github/workflows/ci.yaml" {
            found.push("android");
        }
        if p.starts_with("crates/") {
            found.push("rust");
        }
        if p == "deploy/Dockerfile" || p == ".dockerignore" {
            found.push("image");
        }
    }
    select_costly_areas(&found, paths, pull_request, train)
        .into_iter()
        .collect()
}

#[test]
fn test_a_console_change_builds_no_image_and_no_release() {
    let got = eval_costly(&["frontend/admin/src/a.ts"], true, false);
    assert!(!got.contains("image"));
    assert!(!got.contains("android-release"));
    assert!(got.contains("console-browser"));
}

#[test]
fn test_a_service_change_builds_no_image() {
    let got = eval_costly(&["crates/api-service/src/a.rs"], true, false);
    assert!(!got.contains("image"));
}

#[test]
fn test_a_dockerfile_change_builds_the_image() {
    let got = eval_costly(&["deploy/Dockerfile"], true, false);
    assert!(got.contains("image"));
}

#[test]
fn test_a_crate_manifest_change_builds_the_image() {
    let got = eval_costly(&["crates/core-lib/Cargo.toml"], true, false);
    assert!(got.contains("image"));
}

#[test]
fn test_an_android_source_change_assembles_no_release() {
    let got = eval_costly(
        &["frontend/android/app/src/main/java/app/mobile/Home.kt"],
        true,
        false,
    );
    assert!(got.contains("android"));
    assert!(!got.contains("android-release"));
}

#[test]
fn test_an_android_build_script_change_assembles_the_release() {
    let got = eval_costly(&["frontend/android/app/build.gradle.kts"], true, false);
    assert!(got.contains("android-release"));
}

#[test]
fn test_a_ci_workflow_change_assembles_the_release() {
    let got = eval_costly(&[".github/workflows/ci.yaml"], true, false);
    assert!(got.contains("android-release"));
}

#[test]
fn test_console_screenshots_alone_run_no_browser_suite() {
    let got = eval_costly(&["frontend/admin/screenshots/home.png"], true, false);
    assert!(got.contains("console"));
    assert!(!got.contains("console-browser"));
}

#[test]
fn test_a_console_source_beside_screenshots_runs_the_browser_suite() {
    let got = eval_costly(
        &[
            "frontend/admin/screenshots/home.png",
            "frontend/admin/src/a.ts",
        ],
        true,
        false,
    );
    assert!(got.contains("console-browser"));
}

#[test]
fn test_a_train_builds_the_image_and_the_browser_suite_and_leaves_the_release_to_main() {
    let got = eval_costly(
        &[
            "frontend/admin/src/a.ts",
            "frontend/android/app/src/main/java/a.kt",
        ],
        true,
        true,
    );
    assert!(got.contains("image"));
    assert!(got.contains("console-browser"));
    assert!(!got.contains("android-release"));
}

#[test]
fn test_main_builds_every_part_of_the_areas_it_runs() {
    let got = eval_costly(
        &[
            "frontend/admin/src/a.ts",
            "frontend/android/app/src/main/java/a.kt",
            "deploy/Dockerfile",
        ],
        false,
        false,
    );
    assert!(got.contains("image"));
    assert!(got.contains("android-release"));
    assert!(got.contains("console-browser"));
}

#[test]
fn test_a_change_with_no_android_has_no_release_to_assemble() {
    let got = eval_costly(&["frontend/admin/src/a.ts"], false, false);
    assert!(!got.contains("android-release"));
}

fn fixture_fleet_tests() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "tools/tacho/tests/test_plan.py".to_string(),
            "from tacho import plan\n".to_string(),
        ),
        (
            "tools/tacho/tests/test_runs.py".to_string(),
            "from tacho import runs, plan\n".to_string(),
        ),
        (
            "tools/tacho/tests/test_mail.py".to_string(),
            "import unittest\nfrom tacho import mail\n# the plan is not imported here\n"
                .to_string(),
        ),
    ])
}

#[test]
fn test_a_module_selects_the_tests_that_import_it_and_no_other() {
    let tests = fixture_fleet_tests();
    assert_eq!(
        related_package_tests(
            &["tools/tacho/tacho/plan.py"],
            &tests,
            "tools/tacho",
            "tacho"
        ),
        vec!["tests.test_plan", "tests.test_runs"]
    );
}

#[test]
fn test_a_changed_test_selects_itself() {
    let tests = fixture_fleet_tests();
    assert_eq!(
        related_package_tests(
            &["tools/tacho/tests/test_mail.py"],
            &tests,
            "tools/tacho",
            "tacho"
        ),
        vec!["tests.test_mail"]
    );
}

#[test]
fn test_a_script_selects_the_tests_that_name_it_and_never_everything() {
    let mut tests = fixture_fleet_tests();
    tests.insert(
        "tools/tacho/tests/test_cli.py".to_string(),
        "cmd = ['bin/tacho', '--help']\n".to_string(),
    );
    assert_eq!(
        related_package_tests(&["tools/tacho/bin/tacho"], &tests, "tools/tacho", "tacho"),
        vec!["tests.test_cli"]
    );
    assert!(related_package_tests(
        &["tools/tacho/bin/unmentioned.sh"],
        &tests,
        "tools/tacho",
        "tacho"
    )
    .is_empty());
}

#[test]
fn test_a_module_selects_the_tests_of_the_modules_that_import_it() {
    let tests = fixture_fleet_tests();
    let modules = BTreeMap::from([
        (
            "tools/tacho/tacho/gh.py".to_string(),
            "def call(): pass\n".to_string(),
        ),
        (
            "tools/tacho/tacho/plan.py".to_string(),
            "from tacho import gh\n".to_string(),
        ),
        (
            "tools/tacho/tacho/mail.py".to_string(),
            "def send(): pass\n".to_string(),
        ),
    ]);
    assert_eq!(
        related_package_tests_with_modules(
            &["tools/tacho/tacho/gh.py"],
            &tests,
            &modules,
            "tools/tacho",
            "tacho"
        ),
        vec!["tests.test_plan", "tests.test_runs"]
    );
}

#[test]
fn test_a_path_outside_the_fleet_selects_nothing() {
    let tests = fixture_fleet_tests();
    assert!(
        related_package_tests(&["crates/app/src/lib.rs"], &tests, "tools/tacho", "tacho")
            .is_empty()
    );
}

fn fixture_console_modules() -> (BTreeMap<String, String>, BTreeSet<String>) {
    let modules = BTreeMap::from([
        (
            "frontend/admin/src/lib/money.ts".to_string(),
            "export const x = 1\n".to_string(),
        ),
        (
            "frontend/admin/tests/money.test.mjs".to_string(),
            "import { x } from '../src/lib/money.ts'\n".to_string(),
        ),
        (
            "frontend/admin/tests/names.test.mjs".to_string(),
            "import { y } from '../src/lib/names.ts'\n".to_string(),
        ),
        (
            "frontend/admin/src/lib/names.ts".to_string(),
            "export const y = 1\n".to_string(),
        ),
    ]);
    let tests = BTreeSet::from([
        "frontend/admin/tests/money.test.mjs".to_string(),
        "frontend/admin/tests/names.test.mjs".to_string(),
    ]);
    (modules, tests)
}

#[test]
fn test_a_library_module_selects_the_test_that_imports_it() {
    let (modules, tests) = fixture_console_modules();
    assert_eq!(
        related_console_tests(
            &["frontend/admin/src/lib/money.ts"],
            &modules,
            &tests,
            "frontend/admin/"
        ),
        vec!["tests/money.test.mjs"]
    );
}

#[test]
fn test_a_changed_config_selects_everything() {
    let (modules, tests) = fixture_console_modules();
    assert_eq!(
        related_console_tests(
            &["frontend/admin/package.json"],
            &modules,
            &tests,
            "frontend/admin/"
        ),
        vec!["*"]
    );
}

#[test]
fn test_batch_planner_integrates_costly_area_selection_end_to_end() {
    let rules = vec![
        AreaRule {
            patterns: Vec::new(),
            fallback_patterns: Vec::new(),
            required_patterns: Vec::new(),
            input_patterns: Vec::new(),
            always: false,
            area: "console".to_string(),
            prefixes: vec!["frontend/admin/".to_string()],
            unit_prefixes: BTreeMap::new(),
            checks: Vec::new(),
            isolate_batch: false,
            needs_screenshot: false,
        },
        AreaRule {
            patterns: Vec::new(),
            fallback_patterns: Vec::new(),
            required_patterns: Vec::new(),
            input_patterns: Vec::new(),
            always: false,
            area: "android".to_string(),
            prefixes: vec!["frontend/android/".to_string()],
            unit_prefixes: BTreeMap::new(),
            checks: Vec::new(),
            isolate_batch: false,
            needs_screenshot: false,
        },
        AreaRule {
            patterns: Vec::new(),
            fallback_patterns: Vec::new(),
            required_patterns: Vec::new(),
            input_patterns: Vec::new(),
            always: false,
            area: "image".to_string(),
            prefixes: vec!["deploy/Dockerfile".to_string(), "crates/".to_string()],
            unit_prefixes: BTreeMap::new(),
            checks: Vec::new(),
            isolate_batch: false,
            needs_screenshot: false,
        },
    ];

    // 1. Console source PR in `batches(...)` gets `console-browser` added, while a Rust `.rs` source
    // file (without Cargo.toml or Dockerfile) has `image` pruned:
    let open_prs = vec![
        PullFacts {
            number: 101,
            branch: "agent/ui".to_string(),
            head: "a".repeat(40),
            base: "main".to_string(),
            state: "OPEN".to_string(),
            draft: false,
            mergeable: "MERGEABLE".to_string(),
            files: vec!["frontend/admin/src/screens/Home.tsx".to_string()],
            ..Default::default()
        },
        PullFacts {
            number: 102,
            branch: "agent/svc".to_string(),
            head: "b".repeat(40),
            base: "main".to_string(),
            state: "OPEN".to_string(),
            draft: false,
            mergeable: "MERGEABLE".to_string(),
            files: vec!["crates/api-service/src/main.rs".to_string()],
            ..Default::default()
        },
        PullFacts {
            number: 103,
            branch: "agent/manifest".to_string(),
            head: "c".repeat(40),
            base: "main".to_string(),
            state: "OPEN".to_string(),
            draft: false,
            mergeable: "MERGEABLE".to_string(),
            files: vec!["crates/api-service/Cargo.toml".to_string()],
            ..Default::default()
        },
    ];

    let planned = batches(
        &open_prs,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        &rules,
        6,
    );
    let console_batch = planned
        .iter()
        .find(|b| b.prs.contains(&101))
        .expect("console batch");
    assert!(console_batch.areas.contains(&"console".to_string()));
    assert!(console_batch.areas.contains(&"console-browser".to_string()));
    assert!(!console_batch.areas.contains(&"image".to_string()));

    // PR 102 alone (only `.rs` source) prunes `image`:
    let planned_svc_only = batches(
        &open_prs[1..2],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        &rules,
        6,
    );
    assert_eq!(planned_svc_only.len(), 1);
    assert!(!planned_svc_only[0].areas.contains(&"image".to_string()));

    // PR 103 (touches `Cargo.toml`) keeps `image`:
    let planned_manifest = batches(
        &open_prs[2..3],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        &rules,
        6,
    );
    assert_eq!(planned_manifest.len(), 1);
    assert!(planned_manifest[0].areas.contains(&"image".to_string()));
}

#[test]
fn test_affected_rust_targets_from_manifests_computes_reverse_closure_and_test_binaries() {
    let manifests = BTreeMap::from([
        (
            "core-types".to_string(),
            "[package]\nname = \"core-types\"\n".to_string(),
        ),
        (
            "domain-lib".to_string(),
            "[package]\nname = \"domain-lib\"\n[dependencies]\ncore-types = { path = \"../core-types\" }\n".to_string(),
        ),
        (
            "api-service".to_string(),
            "[package]\nname = \"api-service\"\n[dependencies]\ndomain-lib = { path = \"../domain-lib\" }\n".to_string(),
        ),
        (
            "isolated-tool".to_string(),
            "[package]\nname = \"isolated-tool\"\n".to_string(),
        ),
    ]);

    // 1. Leaf crate source change selects only that crate:
    assert_eq!(
        affected_rust_targets_from_manifests(&["crates/api-service/src/lib.rs"], &manifests),
        vec!["api-service"]
    );

    // 2. Foundation crate source change selects its transitive dependents (`core-types`, `domain-lib`, `api-service`), not `isolated-tool`:
    let closure =
        affected_rust_targets_from_manifests(&["crates/core-types/src/lib.rs"], &manifests);
    assert_eq!(closure, vec!["api-service", "core-types", "domain-lib"]);

    // 3. A single integration test binary change selects `crate --test <binary>`:
    assert_eq!(
        affected_rust_targets_from_manifests(&["crates/api-service/tests/http.rs"], &manifests),
        vec!["api-service --test http"]
    );

    // 4. Shared test helper (`tests/common/mod.rs`) runs the whole crate and its dependents:
    assert_eq!(
        affected_rust_targets_from_manifests(
            &["crates/api-service/tests/common/mod.rs"],
            &manifests
        ),
        vec!["api-service"]
    );

    // 5. Migrations or workspace Cargo.toml select `["*"]`:
    assert_eq!(
        affected_rust_targets_from_manifests(
            &["crates/core-types/migrations/0001_init.sql"],
            &manifests
        ),
        vec!["*"]
    );
    assert_eq!(
        affected_rust_targets_from_manifests(&["Cargo.toml"], &manifests),
        vec!["*"]
    );
}

#[test]
fn test_related_console_specs_and_related_deploy_checks() {
    let specs = BTreeMap::from([
        (
            "frontend/admin/e2e/disputes.spec.ts".to_string(),
            "test('disputes screen', async ({ page }) => { await page.goto('/disputes') })\n"
                .to_string(),
        ),
        (
            "frontend/admin/e2e/treasury.spec.ts".to_string(),
            "test('treasury screen', async ({ page }) => { await page.goto('/treasury') })\n"
                .to_string(),
        ),
    ]);
    assert_eq!(
        related_console_specs(
            &["frontend/admin/src/screens/DisputesScreen.tsx"],
            &specs,
            "frontend/admin"
        ),
        vec!["e2e/disputes.spec.ts"]
    );
    assert_eq!(
        related_console_specs(
            &["frontend/admin/e2e/treasury.spec.ts"],
            &specs,
            "frontend/admin"
        ),
        vec!["e2e/treasury.spec.ts"]
    );
    assert_eq!(
        related_console_specs(&["frontend/admin/src/App.tsx"], &specs, "frontend/admin"),
        vec!["*"]
    );

    assert_eq!(
        related_deploy_checks(&["deploy/prometheus/alerts.yml"], DEFAULT_DEPLOY_CHECKS),
        vec!["observability"]
    );
    assert_eq!(
        related_deploy_checks(&["deploy/helm/Chart.yaml"], DEFAULT_DEPLOY_CHECKS),
        vec!["helm", "network-policies"]
    );
    assert_eq!(
        related_deploy_checks(&["scripts/areas/deploy.sh"], DEFAULT_DEPLOY_CHECKS),
        vec!["*"]
    );
}
