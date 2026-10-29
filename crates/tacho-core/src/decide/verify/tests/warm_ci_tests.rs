//! Parity tests for `test_warm.py` (cases 52–126), `test_kit.py` (cases 163–178),
//! `test_waits_end_on_facts.py` (cases 195–201), `test_jobs.py` (cases 202–213),
//! `test_ci_warm_cache.py` (cases 224–228), and `test_resource_budget.py` (cases 229–233).

use super::*;
use std::collections::{BTreeMap, BTreeSet};

// --- Cases 52–82: cpu_sets, area_budget, execution_waves, StopTracker, CpuMeterState ---

#[test]
fn test_cpu_sets_and_area_budget_partition_host_cores() {
    let sets = cpu_sets(8, 3);
    assert_eq!(sets, vec![vec![0, 1], vec![2, 3], vec![4, 5, 6, 7]]);

    // When n > total, every area still gets a non-empty CPU list:
    let oversub = cpu_sets(2, 4);
    assert_eq!(oversub.len(), 4);
    assert!(oversub.iter().all(|s| !s.is_empty()));

    // Gate gets all CPUs; parallel areas split CPUs:
    let (gate_cpus, gate_jobs) = area_budget(0, 3, true, 8, 8);
    assert_eq!(gate_cpus.len(), 8);
    assert_eq!(gate_jobs, 8);
    let (par_cpus, par_jobs) = area_budget(0, 2, false, 8, 8);
    assert_eq!(par_cpus, vec![0, 1, 2, 3]);
    assert_eq!(par_jobs, 4);
}

#[test]
fn test_execution_waves_runs_gate_first_then_parallel_siblings() {
    let areas = vec![
        "gate".to_string(),
        "rust".to_string(),
        "typescript".to_string(),
    ];
    let waves = execution_waves(&areas, Some("gate"));
    assert_eq!(
        waves,
        vec![
            vec!["gate".to_string()],
            vec!["rust".to_string(), "typescript".to_string()]
        ]
    );
}

#[test]
fn test_stop_tracker_kills_siblings_once_and_marks_rc_98() {
    let mut stop = StopTracker::new();
    assert!(!stop.start("rust"));
    assert!(!stop.start("typescript"));
    assert!(!stop.start("console"));
    stop.finish("console"); // console finished before failure -> never killed

    let killed = stop.fail("rust");
    assert_eq!(killed, vec!["typescript".to_string()]);
    // Second failure kills nothing:
    assert!(stop.fail("typescript").is_empty());
    // Late starter after failure is killed immediately:
    assert!(stop.start("android"));
    assert_eq!(stop.effective_rc("typescript", 143), RC_STOPPED);
    assert_eq!(stop.effective_rc("rust", 1), 1);
    assert_eq!(stop.effective_rc("console", 0), 0);
}

#[test]
fn test_cpu_meter_parses_usage_usec_and_docker_inspect_uses_type_container() {
    let mut meter = CpuMeterState::new();
    meter.record_sample("scope-1", "usage_usec 2400000\nuser_usec 1800000\n");
    meter.record_sample("scope-2", "usage_usec 1100000\n");
    assert_eq!(meter.total_seconds(), 3.5);

    let argv = CpuMeterState::docker_inspect_argv("warm-postgres");
    let type_idx = argv.iter().position(|x| x == "--type").unwrap();
    let name_idx = argv.iter().position(|x| x == "warm-postgres").unwrap();
    assert_eq!(argv[type_idx + 1], "container");
    assert!(type_idx < name_idx);
}

// --- Cases 83–105: sccache_counts, tests_passed, stats_line, summary, shrink_plan, settlement ---

#[test]
fn test_sccache_counts_and_tests_passed_and_stats_line() {
    let pats = StatsPatterns::default();
    let sc_out = "Compile requests 20\nCache hits 15\nCache misses 5\n";
    assert_eq!(sccache_counts(sc_out, &pats), (15, 5));

    let log = "\
running 12 tests
test result: ok. 12 passed; 0 failed; 0 ignored
Tests  3 passed (3)
5 passed in 0.42s
";
    assert_eq!(tests_passed(log, &pats), 20);

    let ar = AreaResult {
        rc: 0,
        wall_s: 10,
        cpu_s: 22.4,
        cache_hits: 15,
        cache_misses: 5,
        tests_passed: 20,
    };
    let line = stats_line("rust", &ar);
    assert_eq!(
        line,
        "rust: ok wall=10s cpu=22.4s sccache=15/20 (75%) tests=20"
    );
}

#[test]
fn test_shrink_plan_evicts_oldest_inactive_target_dirs() {
    let entries = vec![
        AreaDirEntry {
            name: "rust".to_string(),
            bytes: 300,
            mtime: 10,
        },
        AreaDirEntry {
            name: "android".to_string(),
            bytes: 400,
            mtime: 20,
        },
        AreaDirEntry {
            name: "console".to_string(),
            bytes: 200,
            mtime: 30,
        },
    ];
    // "rust" is active so it must NOT be deleted even though it is oldest:
    let active: BTreeSet<String> = ["rust".to_string()].into_iter().collect();
    let evicted = shrink_plan(&entries, &active, 500);
    assert_eq!(evicted, vec!["android".to_string()]);
}

#[test]
fn test_settle_conflicts_regenerates_generated_files_and_aborts_handwritten() {
    let cfg = demo_area_cfg();
    let settled_gen = settle_conflicts(&["packages/sdk/src/generated.ts".to_string()], &cfg);
    assert!(matches!(
        settled_gen,
        Settlement::Regenerate { areas, .. } if areas.contains("schema")
    ));

    let real = settle_conflicts(
        &[
            "packages/sdk/src/generated.ts".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ],
        &cfg,
    );
    assert!(matches!(real, Settlement::AbortAndSkip { .. }));
}

// --- Cases 106–126: native_job_argv, host_job_plan, rust_cache_env, is_network_failure ---

#[test]
fn test_native_job_argv_runs_as_root_and_chowns_only_writable_and_area_target() {
    let yaml = r#"
jobs:
  rust:
    container: ghcr.io/demo/ci:latest
    steps:
      - name: Test
        run: cargo test
"#;
    let job = job_for_yaml(yaml, "rust", "rust", &BTreeMap::new()).unwrap();
    let cfg = CiRunnerConfig::default();
    let mounts = [
        ("/cache/cargo", "/cargo", false),
        ("/cache/target", "/target", false),
        ("/cache/ro-data", "/ro-data", true),
    ];
    let argv = native_job_argv(
        &job,
        "rust",
        "warm-rust-1",
        "warm-net",
        "/tmp/wt",
        &mounts,
        &[0, 1],
        2,
        1000,
        1000,
        &cfg,
    );
    assert!(argv.windows(2).any(|w| w == ["--user", "0:0"]));
    assert!(argv.windows(2).any(|w| w == ["--cpuset-cpus", "0,1"]));
    assert!(argv.contains(&"/cache/ro-data:/ro-data:ro".to_string()));
    let script = argv.last().unwrap();
    // Chowns /w, /cargo, and /target/rust, never /ro-data or whole /target:
    assert!(script.contains("chown -R 1000:1000 /w /cargo /target/rust"));
    assert!(!script.contains("/ro-data"));
    assert!(script.contains("trap 'kill -TERM -$$"));
    // Runner temp files live outside /w and chown surfaces real errors while ignoring missing paths:
    assert!(script.contains("runner_files=$(mktemp -d -t github-runner.XXXXXX)"));
    assert!(script.contains("export GITHUB_OUTPUT=\"$runner_files/output\""));
    assert!(script.contains("export GITHUB_STEP_SUMMARY=\"$runner_files/summary\""));
    assert!(script.contains("grep -v ': No such file or directory$'"));
}

#[test]
fn test_compiled_job_script_executes_and_verifies_chown_handback_behavior() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_chown = bin_dir.join("chown");
    std::fs::write(
        &fake_chown,
        r#"#!/usr/bin/env bash
case "${FAKE_CHOWN_MODE:-ok}" in
  ok)
    exit 0
    ;;
  missing)
    echo "chown: cannot access '/target/rust': No such file or directory" >&2
    exit 1
    ;;
  denied)
    echo "chown: changing ownership of '/w/out': Operation not permitted" >&2
    exit 1
    ;;
  silent_fail)
    exit 2
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_chown, std::fs::Permissions::from_mode(0o755)).unwrap();

    let yaml = r#"
jobs:
  rust:
    container: ghcr.io/demo/ci:latest
    steps:
      - name: Check
        run: test -d "$ runner_files" || true; echo "step-ran"
"#;
    let job = job_for_yaml(yaml, "rust", "rust", &BTreeMap::new()).unwrap();
    let script = compile_job_script(
        &job,
        "rust",
        &["/cargo".to_string(), "/target".to_string()],
        Some((1000, 1000)),
    );

    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // 1. Missing path (e.g. /target/rust not created) must NOT fail a green job:
    let out_missing = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", &path_env)
        .env("FAKE_CHOWN_MODE", "missing")
        .output()
        .unwrap();
    assert!(
        out_missing.status.success(),
        "missing chown target should not fail green job: stderr={}",
        String::from_utf8_lossy(&out_missing.stderr)
    );

    // 2. Real chown failure (Operation not permitted) MUST fail an otherwise green job:
    let out_denied = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", &path_env)
        .env("FAKE_CHOWN_MODE", "denied")
        .output()
        .unwrap();
    assert!(
        !out_denied.status.success(),
        "chown permission refusal must fail otherwise green job"
    );
    let stderr_denied = String::from_utf8_lossy(&out_denied.stderr);
    assert!(
        stderr_denied.contains("Operation not permitted"),
        "expected refusal stderr, got: {stderr_denied}"
    );

    // 3. Silent non-zero chown exit MUST also fail an otherwise green job:
    let out_silent = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", &path_env)
        .env("FAKE_CHOWN_MODE", "silent_fail")
        .output()
        .unwrap();
    assert_eq!(out_silent.status.code(), Some(2));
}

#[test]
fn test_host_job_plan_wraps_in_systemd_user_scope_and_taskset() {
    let yaml = r#"
jobs:
  android:
    steps:
      - name: Assemble
        run: ./gradlew assembleDebug
"#;
    let job = job_for_yaml(yaml, "android", "android", &BTreeMap::new()).unwrap();
    let cfg = CiRunnerConfig::default();
    let plan = host_job_plan(
        &job,
        "android",
        "warm-android-1",
        "/tmp/wt",
        "/var/cache/ci",
        &[2, 3],
        2,
        true,
        true,
        &cfg,
    );
    assert_eq!(plan.scope_unit.as_deref(), Some("warm-android-1"));
    assert!(plan.argv.contains(&"systemd-run".to_string()));
    assert!(plan.argv.contains(&"AllowedCPUs=2,3".to_string()));
    assert!(plan.argv.windows(3).any(|w| w == ["taskset", "-c", "2,3"]));
    assert_eq!(
        plan.env.get("CARGO_TARGET_DIR").map(String::as_str),
        Some("/var/cache/ci/target/android")
    );
    assert_eq!(
        plan.env.get("RUSTC_WRAPPER").map(String::as_str),
        Some("sccache")
    );
    assert_eq!(
        plan.env.get("GRADLE_OPTS").map(String::as_str),
        Some("-Dorg.gradle.daemon.registry.base=/var/cache/ci/gradle-daemons/warm-android-1")
    );

    // Two android runs with distinct scope units never share a Gradle daemon registry:
    let plan2 = host_job_plan(
        &job,
        "android",
        "warm-android-2",
        "/tmp/wt",
        "/var/cache/ci",
        &[2, 3],
        2,
        true,
        true,
        &cfg,
    );
    assert_ne!(plan.env.get("GRADLE_OPTS"), plan2.env.get("GRADLE_OPTS"));

    // Console host job names TACHO_CAPTURE_CONTAINER and removes it on cleanup:
    let console_plan = host_job_plan(
        &job,
        "console",
        "warm-console-1",
        "/tmp/wt",
        "/var/cache/ci",
        &[2, 3],
        2,
        true,
        true,
        &cfg,
    );
    assert_eq!(
        console_plan
            .env
            .get("TACHO_CAPTURE_CONTAINER")
            .map(String::as_str),
        Some("warm-console-1-shots")
    );
    assert!(console_plan
        .argv
        .last()
        .unwrap()
        .contains("docker rm -f \"$TACHO_CAPTURE_CONTAINER\""));
}

// --- Cases 163–178: test_kit.py & test_jobs.py (202–213) & test_waits_end_on_facts.py (195–201) ---

#[test]
fn test_waits_end_on_facts_and_kill_group_escalates_term_to_kill() {
    assert!(matches!(
        wait_until_fact(true, 10, 5000, 50, "db-ready"),
        WaitDecision::Ready { .. }
    ));
    assert!(matches!(
        wait_until_fact(false, 5000, 5000, 50, "db-ready"),
        WaitDecision::Aborted { .. }
    ));

    // Process group termination: SIGTERM first, then SIGKILL after grace_ms:
    assert!(matches!(
        kill_group_decision(999, true, false, 0, 2000),
        KillPhaseDecision::SendSigTerm { pgid: 999, .. }
    ));
    assert!(matches!(
        kill_group_decision(999, true, true, 500, 2000),
        KillPhaseDecision::WaitAfterTerm { .. }
    ));
    assert!(matches!(
        kill_group_decision(999, true, true, 2000, 2000),
        KillPhaseDecision::SendSigKill { pgid: 999, .. }
    ));
    assert!(matches!(
        kill_group_decision(999, false, true, 100, 2000),
        KillPhaseDecision::Exited { .. }
    ));

    assert!(prove_timeout_message("clippy", 300).contains("TIMED OUT after 300s"));
    assert!(matches!(
        preview_wait_decision(true, true, 120, 5000, ""),
        WaitDecision::Ready { .. }
    ));
    assert!(matches!(
        preview_wait_decision(false, false, 120, 5000, "panic at startup"),
        WaitDecision::Aborted { .. }
    ));
    assert!(matches!(
        service_health_decision("healthy", 100, 5000),
        WaitDecision::Ready { .. }
    ));
    assert!(matches!(
        service_health_decision("unhealthy", 100, 5000),
        WaitDecision::Aborted { .. }
    ));
}

#[test]
fn test_prove_plan_attribution_and_pr_refusals() {
    let rules = vec![AreaRule {
        area: "console".to_string(),
        prefixes: vec!["apps/console/".to_string()],
        checks: vec![("console-typecheck".to_string(), "pnpm check".to_string())],
        needs_screenshot: true,
        isolate_batch: false,
        unit_prefixes: BTreeMap::new(),
    }];
    // Narrow Rust module check:
    let plan = proof_plan(
        &["crates/tacho-core/src/decide/verify/plan.rs".to_string()],
        &rules,
    );
    assert!(plan
        .iter()
        .any(|(_, cmd)| cmd.contains("cargo test -p tacho-core --lib -- decide::verify::plan::")));

    // Empty file list conservative fallback:
    let fallback = conservative_proof_plan(&[], &rules);
    assert!(fallback.iter().any(|(n, _)| n == "test-workspace"));

    // Attribution check catches Co-Authored-By, Generated with, and robot emoji:
    let probs = attribution(&[
        "abc12345 Fix bug\n\nCo-Authored-By: Someone <a@b.com>",
        "Generated with Automated Tool",
        " Title \u{1F916}",
    ]);
    assert_eq!(probs.len(), 3);

    // Proof tree mismatch & missing UI screenshot refused by pr_refusals:
    let bad_proof = ProofSummary {
        head: "sha1".to_string(),
        tree: "tree-old".to_string(),
        base: "base1".to_string(),
        ok: true,
        checks: Vec::new(),
        screenshots: Vec::new(),
        timestamp: 100,
        changed_files: Vec::new(),
    };
    let refusals = pr_refusals(
        "Update UI",
        "Touches console",
        &["sha1 Update UI".to_string()],
        &[],
        &["apps/console/src/App.tsx".to_string()],
        &[],
        "sha1",
        "tree-new",
        Some(&bad_proof),
        &rules,
    );
    assert!(refusals.iter().any(|r| r.contains("proof is stale")));
    assert!(refusals.iter().any(|r| r.contains("no screenshots")));
}

// --- Cases 224–233: test_ci_warm_cache.py & test_resource_budget.py ---

#[test]
fn test_warm_cache_repo_id_and_subdirs_validation() {
    assert!(validate_repo_id("demo-repo_1.0").is_ok());
    assert!(validate_repo_id("../escape").is_err());
    assert!(validate_repo_id("bad/slash").is_err());
    assert!(validate_repo_id("").is_err());

    let cfg = CiRunnerConfig::default();
    let subdirs: BTreeSet<String> = cfg.cache_subdirs.iter().cloned().collect();
    assert!(verify_cache_root(true, &subdirs, &cfg).is_ok());
    assert!(verify_cache_root(false, &subdirs, &cfg).is_err());
    let mut missing = subdirs.clone();
    missing.remove("sccache");
    assert!(verify_cache_root(true, &missing, &cfg).is_err());
}

#[test]
fn test_resource_budget_validates_cpuset_memory_and_pids() {
    let cpus = validate_resource_budget("0-3,6", "12G", 4096, 8).unwrap();
    assert_eq!(cpus, [0, 1, 2, 3, 6].into_iter().collect());
    // Out-of-range CPU index rejected:
    assert!(validate_resource_budget("0-8", "12G", 4096, 8).is_err());
    // Inverted CPU range rejected:
    assert!(validate_resource_budget("5-2", "12G", 4096, 8).is_err());
    // Tiny pids_max rejected:
    assert!(validate_resource_budget("0-1", "12G", 10, 8).is_err());
}
