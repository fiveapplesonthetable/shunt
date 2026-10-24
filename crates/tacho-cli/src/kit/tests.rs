//! Unit tests for `kit` (`start`, `prove`, `pr`, `job`, `wait`, `kill`, `note`, `starve`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::{
    parse_env, AreaRule, BatchMode, BatchRun, ProofCheck, ProofSummary,
};
use tacho_daemon::{FleetState, WorktreeEntry};

use super::pr::{
    check_forbidden_attribution, format_pr_body_with_outputs, push_args_for_branch,
    validate_pr_preflight, PrPreflightInput,
};
use super::procs::{
    format_pid_record, parse_pid_record, pid_alive_with_starttime, proc_pid_cwd, proc_pid_pgid,
    proc_pid_starttime, validate_job_name, verify_kill_target, wait_for_numeric_pid,
};
use super::prove::{
    changed_files_since, extract_screen_from_screenshot_path, filter_files_for_area_mapping,
    plan_prove_checks, plan_rust_unit_checks, run_prove_step, stray_screenshots_for_files,
};
use super::start::{
    allocate_port_block, format_tachoenv_content, is_full_hex_sha, merge_path,
    resolve_pinned_base_sha, run_provisioner_bounded, PORT_BASE, PORT_BLOCK_SIZE,
};
use super::starve::{
    parse_cpu_list_str, plan_starve_slice, stop_registered_burner, RegisteredBurner,
    BURNERS_PER_CPU, STARVE_MAX_SECS,
};
use super::tree_of_worktree;

fn init_test_repo(dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command failed");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "engineer@example.com"]);
    run(&["config", "user.name", "Test Engineer"]);
    fs::write(dir.join("README.md"), "# Test Repo\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "Initial commit"]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn test_start_resolves_pinned_base_sha_and_refuses_unknown_revisions() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let main_sha = init_test_repo(&repo);
    assert!(is_full_hex_sha(&main_sha));

    let mut state = FleetState::default();

    // 1. Resolving "main" returns the exact 40-char hex commit SHA (never the branch name "main")
    let (resolved_main, base_pr) = resolve_pinned_base_sha(&repo, "main", &state).unwrap();
    assert_eq!(resolved_main, main_sha);
    assert!(is_full_hex_sha(&resolved_main));
    assert_eq!(base_pr, None);

    // 2. Resolving a cached PR number returns its pinned head SHA and Some(pr_number)
    state.open_prs_cache.push(PullFacts {
        number: 42,
        branch: "feat-42".to_string(),
        head: main_sha.clone(),
        base: "main".to_string(),
        state: "OPEN".to_string(),
        ..Default::default()
    });
    let (pr_sha, pr_num) = resolve_pinned_base_sha(&repo, "42", &state).unwrap();
    assert_eq!(pr_sha, main_sha);
    assert_eq!(pr_num, Some(42));

    let (pr_prefixed_sha, pr_prefixed_num) =
        resolve_pinned_base_sha(&repo, "pr:42", &state).unwrap();
    assert_eq!(pr_prefixed_sha, main_sha);
    assert_eq!(pr_prefixed_num, Some(42));

    // 3. Unknown PR or unknown branch/SHA is strictly refused without falling back to HEAD
    assert!(resolve_pinned_base_sha(&repo, "999999", &state).is_err());
    assert!(resolve_pinned_base_sha(&repo, "pr:999999", &state).is_err());
    assert!(resolve_pinned_base_sha(&repo, "nonexistent-branch-xyz", &state).is_err());
    assert!(resolve_pinned_base_sha(&repo, "", &state).is_err());
}

#[test]
fn test_start_allocates_disjoint_20_port_blocks_and_idempotent_rerun() {
    let mut state = FleetState::default();

    let p1 = allocate_port_block("agent-alpha", &state).unwrap();
    assert!(p1 >= PORT_BASE);
    state.worktrees.insert(
        "agent-alpha".to_string(),
        WorktreeEntry {
            slug: "agent-alpha".to_string(),
            path: "/tmp/wt/agent-alpha".to_string(),
            branch: "agent-alpha".to_string(),
            base_sha: "a".repeat(40),
            base_pr: None,
            port: p1,
            port_last: p1 + PORT_BLOCK_SIZE - 1,
            created_at: 1000,
        },
    );

    let p2 = allocate_port_block("agent-beta", &state).unwrap();
    assert_ne!(p1, p2);
    let range1: BTreeSet<u16> = (p1..p1 + PORT_BLOCK_SIZE).collect();
    let range2: BTreeSet<u16> = (p2..p2 + PORT_BLOCK_SIZE).collect();
    assert!(
        range1.is_disjoint(&range2),
        "port blocks collided: {range1:?} vs {range2:?}"
    );
    assert_eq!(allocate_port_block("agent-alpha", &state).unwrap(), p1);
}

#[test]
fn test_start_generates_complete_tachoenv_and_merges_toolchain_path() {
    let merged = merge_path(
        &[
            PathBuf::from("/opt/toolchain/bin"),
            PathBuf::from("/usr/local/bin"),
        ],
        "/usr/local/bin:/usr/bin:/bin",
    );
    assert_eq!(merged, "/opt/toolchain/bin:/usr/local/bin:/usr/bin:/bin");

    let wt_env = WorktreeEntry {
        slug: "feat-auth".to_string(),
        path: "/worktrees/feat-auth".to_string(),
        branch: "feat-auth".to_string(),
        base_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
        base_pr: Some(15),
        port: 20020,
        port_last: 20039,
        created_at: 1700000000,
    };
    let mut extra = BTreeMap::new();
    extra.insert(
        "DATABASE_URL".to_string(),
        "postgres://127.0.0.1:20022/test_feat_auth".to_string(),
    );

    let env_text = format_tachoenv_content(
        &wt_env,
        Path::new("/worktrees/.cargo-target"),
        &merged,
        Some("Ada Lovelace"),
        Some("ada@example.com"),
        &extra,
    );
    let parsed = parse_env(&env_text);

    assert_eq!(parsed.get("SLUG").unwrap(), "feat-auth");
    assert_eq!(parsed.get("WORKTREE").unwrap(), "/worktrees/feat-auth");
    assert_eq!(
        parsed.get("BASE_SHA").unwrap(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(parsed.get("BASE_PR").unwrap(), "15");
    assert_eq!(parsed.get("PORT").unwrap(), "20020");
    assert_eq!(parsed.get("PORT_LAST").unwrap(), "20039");
    assert_eq!(parsed.get("CONSOLE_URL").unwrap(), "http://localhost:20028");
    assert_eq!(parsed.get("SHOT_URL").unwrap(), "http://localhost:20029");
    assert_eq!(
        parsed.get("CARGO_TARGET_DIR").unwrap(),
        "/worktrees/.cargo-target"
    );
    assert_eq!(parsed.get("CARGO_INCREMENTAL").unwrap(), "0");
    assert_eq!(parsed.get("PATH").unwrap(), &merged);
    assert_eq!(parsed.get("GIT_AUTHOR_NAME").unwrap(), "Ada Lovelace");
    assert_eq!(parsed.get("GIT_COMMITTER_NAME").unwrap(), "Ada Lovelace");
    assert_eq!(parsed.get("GIT_AUTHOR_EMAIL").unwrap(), "ada@example.com");
    assert_eq!(
        parsed.get("GIT_COMMITTER_EMAIL").unwrap(),
        "ada@example.com"
    );
    assert_eq!(
        parsed.get("DATABASE_URL").unwrap(),
        "postgres://127.0.0.1:20022/test_feat_auth"
    );
}

#[test]
fn test_start_first_install_exceeding_cap_returns_exit_code_3() {
    let tmp = tempfile::tempdir().unwrap();
    let env_vars = BTreeMap::new();

    // 1. Provisioner that exits with code 3 or 124 (cap exceeded) returns Ok(Err(3)) immediately
    let res_cap = run_provisioner_bounded(tmp.path(), "exit 124", &env_vars, 30).unwrap();
    assert_eq!(res_cap, Err(3));

    let res_code3 = run_provisioner_bounded(tmp.path(), "exit 3", &env_vars, 30).unwrap();
    assert_eq!(res_code3, Err(3));

    // 2. Provisioner that succeeds returns Ok(Ok(stdout))
    let res_ok = run_provisioner_bounded(
        tmp.path(),
        "echo DATABASE_URL=postgres://localhost/db",
        &env_vars,
        30,
    )
    .unwrap();
    assert_eq!(
        res_ok.unwrap().trim(),
        "DATABASE_URL=postgres://localhost/db"
    );
}

#[test]
fn test_prove_maps_diff_to_area_and_units_and_rust_modules() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let base_sha = init_test_repo(&repo);

    // Create staged, unstaged, and untracked changes
    fs::create_dir_all(repo.join("crates/api/src/auth")).unwrap();
    fs::create_dir_all(repo.join("crates/api/tests")).unwrap();
    fs::write(
        repo.join("crates/api/src/auth/mod.rs"),
        "pub fn login() {}\n",
    )
    .unwrap();
    fs::write(
        repo.join("crates/api/tests/integration.rs"),
        "#[test] fn t() {}\n",
    )
    .unwrap();

    let changed = changed_files_since(&repo, &base_sha).unwrap();
    assert!(changed.contains(&"crates/api/src/auth/mod.rs".to_string()));
    assert!(changed.contains(&"crates/api/tests/integration.rs".to_string()));

    // Verify Rust unit check planning maps module & integration test suite
    let rust_checks = plan_rust_unit_checks(&changed);
    assert_eq!(rust_checks.len(), 2);
    assert_eq!(rust_checks[0].area, "api");
    assert_eq!(rust_checks[0].units, vec!["api::auth"]);
    assert_eq!(rust_checks[0].cmd, "cargo test -p api --lib -- auth::");
    assert_eq!(rust_checks[1].area, "api");
    assert_eq!(rust_checks[1].units, vec!["api::test::integration"]);
    assert_eq!(rust_checks[1].cmd, "cargo test -p api --test integration");

    // Touching lib.rs upgrades to whole-crate check
    let mut with_lib = changed.clone();
    with_lib.push("crates/api/src/lib.rs".to_string());
    let crate_checks = plan_rust_unit_checks(&with_lib);
    assert_eq!(crate_checks.len(), 1);
    assert_eq!(crate_checks[0].cmd, "cargo test -p api");

    // Touching root Cargo.toml upgrades to workspace check
    let mut with_root = with_lib;
    with_root.push("Cargo.toml".to_string());
    let ws_checks = plan_rust_unit_checks(&with_root);
    assert_eq!(ws_checks.len(), 1);
    assert_eq!(ws_checks[0].cmd, "cargo test --workspace");
}

#[test]
fn test_prove_tree_hash_staleness_and_screenshot_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let _base_sha = init_test_repo(&repo);

    let tree1 = tree_of_worktree(&repo).unwrap();
    // Internal .tachoproof or .tachoenv files do NOT alter the worktree tree hash
    fs::create_dir_all(repo.join(".tachoproof")).unwrap();
    fs::write(repo.join(".tachoproof/summary.json"), "{}").unwrap();
    fs::write(repo.join(".tachoenv"), "PORT=20000\n").unwrap();
    let tree_after_internal = tree_of_worktree(&repo).unwrap();
    assert_eq!(tree1, tree_after_internal);

    // Modifying or adding any source file changes the tree hash immediately
    fs::write(repo.join("README.md"), "# Modified\n").unwrap();
    let tree2 = tree_of_worktree(&repo).unwrap();
    assert_ne!(
        tree1, tree2,
        "modifying a file must invalidate the proof tree"
    );

    // Committing another change changes the tree hash again
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "Update readme"])
        .current_dir(&repo)
        .output()
        .unwrap();
    fs::write(repo.join("new_file.rs"), "fn main() {}\n").unwrap();
    let tree3 = tree_of_worktree(&repo).unwrap();
    assert_ne!(tree2, tree3);

    // Screenshot scope rules:
    assert_eq!(
        extract_screen_from_screenshot_path("screenshots/billing/invoice.png").as_deref(),
        Some("billing")
    );
    let files = vec![
        "src/screens/dashboard/view.tsx".to_string(),
        "screenshots/dashboard/main.png".to_string(),
        "screenshots/billing/stray.png".to_string(),
    ];
    let stray = stray_screenshots_for_files(&repo, &files);
    assert_eq!(stray, vec!["screenshots/billing/stray.png".to_string()]);

    let filtered = filter_files_for_area_mapping(&repo, &files);
    assert!(!filtered.contains(&"screenshots/billing/stray.png".to_string()));
    assert!(filtered.contains(&"screenshots/dashboard/main.png".to_string()));
}

#[test]
fn test_prove_step_execution_and_area_rule_substitution() {
    let tmp = tempfile::tempdir().unwrap();
    let env_vars = BTreeMap::new();
    let (rc_ok, _, out_ok) = run_prove_step(
        tmp.path(),
        "echo \"area=$TACHO_AREA units=$TACHO_UNITS\"",
        &env_vars,
        "core",
        &["u1".to_string(), "u2".to_string()],
        30,
    )
    .unwrap();
    assert_eq!(rc_ok, 0);
    assert!(out_ok.contains("area=core units=u1,u2"));

    let rules = vec![AreaRule {
        area: "backend".to_string(),
        prefixes: vec!["crates/".to_string()],
        unit_prefixes: BTreeMap::from([("api".to_string(), vec!["crates/api/".to_string()])]),
        checks: vec![("unit".to_string(), "cargo test -p {units}".to_string())],
        isolate_batch: false,
        needs_screenshot: false,
    }];
    let planned = plan_prove_checks(tmp.path(), &["crates/api/src/lib.rs".to_string()], &rules);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].cmd, "cargo test -p api");
}

#[test]
fn test_pr_preflight_refuses_all_violations_and_allows_clean_proof() {
    let valid_proof = ProofSummary {
        head: "b".repeat(40),
        tree: "tree1234567890".to_string(),
        base: "a".repeat(40),
        ok: true,
        timestamp: 1000,
        checks: vec![ProofCheck {
            name: "core:unit".to_string(),
            cmd: "cargo test -p core".to_string(),
            ok: true,
            area: Some("core".to_string()),
            units: vec!["core".to_string()],
            duration_ms: 120,
            output_tail: "test result: ok. 5 passed; 0 failed".to_string(),
        }],
        changed_files: Vec::new(),
        screenshots: Vec::new(),
    };

    let clean_input = PrPreflightInput {
        title: "Fix query planner edge case".to_string(),
        description: "Sessions expire; the code generator is rerun when schema changes."
            .to_string(),
        base_sha: "a".repeat(40),
        head_sha: "b".repeat(40),
        head_tree: "tree1234567890".to_string(),
        dirty_files: Vec::new(),
        changed_files: vec!["crates/core/src/lib.rs".to_string()],
        commits: vec![(
            "b123456789abcdef".to_string(),
            "Fix query planner edge case\n\nPlain commit body.".to_string(),
        )],
        merge_commits: Vec::new(),
        proof: Some(valid_proof.clone()),
        stray_screenshots: Vec::new(),
        area_rules: Vec::new(),
        shots_anyway: false,
    };
    assert!(
        validate_pr_preflight(&clean_input).is_empty(),
        "clean preflight should have zero errors"
    );

    // 1. Empty diff, dirty tree, merge commit, stale proof, Co-Authored-By, Generated with, session link, stray PNG
    let bad_input = PrPreflightInput {
        title: "".to_string(),
        description: "Generated with Claude Code\nhttps://claude.ai/code/session_01AbCdEfGhIjKlMn"
            .to_string(),
        base_sha: "a".repeat(40),
        head_sha: "b".repeat(40),
        head_tree: "different_tree_hash".to_string(),
        dirty_files: vec!["M crates/core/src/lib.rs".to_string()],
        changed_files: Vec::new(),
        commits: vec![(
            "deadbeefcafebabe".to_string(),
            "WIP\n\nCo-Authored-By: Helper <helper@example.com>".to_string(),
        )],
        merge_commits: vec!["feedface12345678".to_string()],
        proof: Some(valid_proof.clone()),
        stray_screenshots: vec!["screenshots/unrelated/shot.png".to_string()],
        area_rules: Vec::new(),
        shots_anyway: false,
    };
    let errs = validate_pr_preflight(&bad_input);
    let joined = errs.join("\n");
    assert!(joined.contains("PR title cannot be empty"), "{joined}");
    assert!(joined.contains("diff against base"), "{joined}");
    assert!(joined.contains("uncommitted changes"), "{joined}");
    assert!(joined.contains("merge commits are not allowed"), "{joined}");
    assert!(joined.contains("proof is stale"), "{joined}");
    assert!(
        joined.contains("commit deadbeef contains a forbidden 'Co-Authored-By:' trailer"),
        "{joined}"
    );
    assert!(joined.contains("Generated with"), "{joined}");
    assert!(joined.contains("session link"), "{joined}");
    assert!(
        joined.contains("stray PNG screenshots outside touched screens"),
        "{joined}"
    );

    // 2. With `shots_anyway: true`, stray screenshots are permitted
    let mut shots_override = clean_input.clone();
    shots_override.stray_screenshots = vec!["screenshots/unrelated/shot.png".to_string()];
    assert!(!validate_pr_preflight(&shots_override).is_empty());
    shots_override.shots_anyway = true;
    assert!(validate_pr_preflight(&shots_override).is_empty());

    // 3. Robot emoji is refused
    let mut emoji_err = Vec::new();
    check_forbidden_attribution("PR body", "Done \u{1F916}", &mut emoji_err);
    assert_eq!(emoji_err.len(), 1);
}

#[test]
fn test_pr_push_args_and_body_formatting_with_proof_commands_and_outputs() {
    let first_push = push_args_for_branch("feat-alpha", false);
    assert_eq!(
        first_push,
        vec![
            "push",
            "--no-verify",
            "-u",
            "origin",
            "HEAD:refs/heads/feat-alpha"
        ]
    );

    let update_push = push_args_for_branch("feat-alpha", true);
    assert_eq!(
        update_push,
        vec![
            "push",
            "--no-verify",
            "--force-with-lease",
            "-u",
            "origin",
            "HEAD:refs/heads/feat-alpha"
        ]
    );

    let proof = ProofSummary {
        head: "b".repeat(40),
        tree: "c".repeat(40),
        base: "a".repeat(40),
        ok: true,
        timestamp: 1000,
        checks: vec![ProofCheck {
            name: "core:test".to_string(),
            cmd: "cargo test -p core".to_string(),
            ok: true,
            area: Some("core".to_string()),
            units: vec!["core".to_string()],
            duration_ms: 1500,
            output_tail: "running 3 tests\ntest result: ok.".to_string(),
        }],
        changed_files: Vec::new(),
        screenshots: Vec::new(),
    };
    let body = format_pr_body_with_outputs("Fixes cache invalidation.", &proof, Some(12));
    assert!(body.contains("Fixes cache invalidation."));
    assert!(body.contains("Stacks on #12."));
    assert!(body.contains("## Proof"));
    assert!(body.contains("$ cargo test -p core"));
    assert!(body.contains("running 3 tests\ntest result: ok."));
}

#[test]
fn test_job_name_validation_and_pid_record_parsing() {
    for valid in ["db", "a", "server-1", "worker_2.test", "0123456789"] {
        assert!(
            validate_job_name(valid).is_ok(),
            "expected '{valid}' to be valid"
        );
    }
    for invalid in [
        "",
        "-bad",
        ".hidden",
        "../escape",
        "sub/dir",
        "Upper",
        "space name",
        &"a".repeat(41),
    ] {
        assert!(
            validate_job_name(invalid).is_err(),
            "expected '{invalid}' to be rejected"
        );
    }

    let formatted = format_pid_record(4242, Some(987654321));
    assert_eq!(formatted, "4242 987654321\n");
    assert_eq!(parse_pid_record(&formatted), Some((4242, Some(987654321))));
    assert_eq!(parse_pid_record("4242\n"), Some((4242, None)));
    assert_eq!(parse_pid_record("0\n"), None);
}

#[test]
fn test_wait_numeric_pid_and_kill_verification_guards() {
    // 1. Spawn a short-lived process group that exits immediately and wait on its numeric PID
    let mut quick = Command::new("true").process_group(0).spawn().unwrap();
    let quick_pid = quick.id();
    let _ = quick.wait();
    assert_eq!(wait_for_numeric_pid(quick_pid, 5).unwrap(), 0);

    // 2. Spawn a process group in a temporary worktree directory and verify live proc metadata
    let tmp = tempfile::tempdir().unwrap();
    let wt_dir = tmp.path().join("wt");
    let outside_dir = tmp.path().join("wt-other");
    fs::create_dir_all(&wt_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();

    let mut child = Command::new("cat")
        .current_dir(&wt_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let st = proc_pid_starttime(pid);
    let pgid = proc_pid_pgid(pid);
    let cwd = proc_pid_cwd(pid);
    let wt_str = wt_dir.to_string_lossy().to_string();
    let outside_str = outside_dir.to_string_lossy().to_string();

    assert!(pid_alive_with_starttime(pid, st));
    assert!(!pid_alive_with_starttime(pid, st.map(|s| s + 99999)));

    // Valid target passes all 3 checks: starttime, worktree cwd ownership, and pgid == pid
    assert!(verify_kill_target(pid, st, st, Some(&wt_str), cwd.as_deref(), pgid, &wt_str,).is_ok());

    // Mismatched starttime (reused PID) is refused
    let err_st = verify_kill_target(
        pid,
        st.map(|s| s + 1),
        st,
        Some(&wt_str),
        cwd.as_deref(),
        pgid,
        &wt_str,
    )
    .unwrap_err();
    assert!(err_st.contains("starttime mismatch"));

    // Process whose /proc/<pid>/cwd is outside the worktree (including sibling prefix `wt-other`) is refused
    let err_cwd = verify_kill_target(
        pid,
        st,
        st,
        Some(&wt_str),
        Some(&outside_str),
        pgid,
        &wt_str,
    )
    .unwrap_err();
    assert!(err_cwd.contains("outside worktree"));

    // Process that is not a process group leader (`pgid != pid`) is refused
    let err_pgid = verify_kill_target(
        pid,
        st,
        st,
        Some(&wt_str),
        cwd.as_deref(),
        Some(pid + 1),
        &wt_str,
    )
    .unwrap_err();
    assert!(err_pgid.contains("not a process group leader"));

    // Clean up child by closing its stdin and reaping
    drop(child.stdin.take());
    let _ = child.wait();
}

#[test]
fn test_starve_plan_4_burners_per_cpu_one_eighth_fleet_cap_and_highest_4_default() {
    assert_eq!(STARVE_MAX_SECS, 300);
    assert_eq!(BURNERS_PER_CPU, 4);

    // 1. On a 32-CPU fleet, 1/8 = 4 CPUs max; default picks the highest 4 CPUs (28-31) and 16 burners
    let plan32 = plan_starve_slice(32, None, None, true, false, None);
    assert_eq!(plan32.cpus, "28-31");
    assert_eq!(plan32.burners, 16);
    assert_eq!(
        plan32.wrap,
        Some(vec![
            "taskset".to_string(),
            "-c".to_string(),
            "28-31".to_string()
        ])
    );

    // 2. Requesting 5 CPUs on a 32-CPU fleet (> 1/8) is refused
    let over_limit = plan_starve_slice(32, None, Some("0-4"), true, false, None);
    assert!(over_limit.wrap.is_none());
    assert!(over_limit
        .problem
        .contains("exceeds the starve limit of 4 CPUs"));

    // 3. Requesting CPUs outside available affinity is refused
    let avail = parse_cpu_list_str("0-3,8-11").unwrap();
    let outside_aff = plan_starve_slice(32, None, Some("4-5"), true, false, Some(&avail));
    assert!(outside_aff.wrap.is_none());
    assert!(outside_aff
        .problem
        .contains("outside this process's available affinity"));

    // 4. Default selection with custom affinity `[0,1,2,3,8,9,10,11]` picks the highest 4 (`8-11`)
    let highest_avail = plan_starve_slice(32, None, None, false, true, Some(&avail));
    assert_eq!(highest_avail.cpus, "8-11");
    assert_eq!(highest_avail.burners, 16);
    assert_eq!(
        highest_avail.wrap,
        Some(vec![
            "systemd-run".to_string(),
            "--user".to_string(),
            "--scope".to_string(),
            "--quiet".to_string(),
            "-p".to_string(),
            "AllowedCPUs=8-11".to_string(),
        ])
    );
}

#[test]
fn test_starve_registered_burner_verification_and_preemption() {
    // Spawn a real `yes` process in its own process group with TACHO_STARVE=1
    let mut burner = Command::new("yes")
        .env("TACHO_STARVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = burner.id();
    let st = proc_pid_starttime(pid);

    // Mismatched starttime refuses to stop the process
    let wrong_reg = RegisteredBurner {
        pid,
        starttime: st.map(|s| s + 1000),
    };
    assert!(!stop_registered_burner(&wrong_reg));

    // Matching `(pid, starttime)` + `TACHO_STARVE=1` + `yes` + `pgid == pid` stops the burner
    let right_reg = RegisteredBurner { pid, starttime: st };
    assert!(stop_registered_burner(&right_reg));
    let _ = burner.wait();
}

#[test]
fn test_starve_refuses_when_merge_batch_is_running() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let mut store = tacho_daemon::FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |st| {
            st.batches.push(BatchRun {
                id: "b001".to_string(),
                group: "core".to_string(),
                prs: vec![10],
                shas: BTreeMap::new(),
                main_sha: "a".repeat(40),
                areas: vec!["core".to_string()],
                checks: Vec::new(),
                mode: BatchMode::Presubmit,
                status: "running".to_string(),
                started_at: 1000,
                finished_at: None,
                tree: None,
                pid: None,
                log: String::new(),
                culprit: None,
                skipped: Vec::new(),
                flakes: Vec::new(),
            });
        })
        .unwrap();

    let state = store.load().unwrap();
    assert!(state
        .batches
        .iter()
        .any(|b| b.status == "running" || b.status == "bisecting"));
}
