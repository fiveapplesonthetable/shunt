//! Deterministic unit tests for all `fleet` submodules (`store`, `dispatch`, `batch`,
//! `train`, `land`, `shape`, `watch`, `tidy`, `ops`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use tacho_core::decide::dispatch::{builtin_providers, AgentRun, Hold, PullFacts, STALE_HOLD_SECS};
use tacho_core::decide::land::TrainRecord;
use tacho_core::decide::verify::{AreaRule, FailedRecord, PassedRecord};
use tacho_core::decide::watch::{ProcEntry, TidyDecision};
use tacho_daemon::{DecisionRecord, FleetState, FleetStore};

use super::*;

/// Runs `git args` in `dir` hermetically and returns trimmed stdout.
///
/// `GIT_CEILING_DIRECTORIES` is set to the parent of `dir`'s tempdir so git
/// never walks up into an enclosing repository (e.g. when `TMPDIR` lives in
/// a checkout), and stdin is null with prompts disabled so git cannot block.
fn git(dir: &Path, args: &[&str]) -> String {
    let ceiling = std::env::temp_dir();
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", &ceiling)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fresh tempdir from which git cannot discover any repository.
///
/// The product's `git_stdout` deliberately lets git walk up (fleets run in nested repos), so a
/// "not a repo" test must sit somewhere no enclosing checkout exists. Tries `TMPDIR`, then
/// `/tmp` and `/dev/shm`, verifying each with an unbounded `git rev-parse`; panics with a clear
/// message instead of silently testing the wrong thing.
fn non_repo_tempdir() -> tempfile::TempDir {
    let roots = [std::env::temp_dir(), "/tmp".into(), "/dev/shm".into()];
    for root in &roots {
        let Ok(dir) = tempfile::tempdir_in(root) else {
            continue;
        };
        let discovered = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("spawn git: {e}"));
        if !discovered.success() {
            return dir;
        }
    }
    panic!("every temp root {roots:?} is inside a git repository; set TMPDIR outside any repo");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    fs::write(dir.join("a.txt"), "base\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "base"]);
}

// =============================================================================
// Existing Core Safety & Atomic Store Tests
// =============================================================================

#[test]
fn rescue_archives_dirty_and_untracked_work_without_stash() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt-é漢");
    fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    fs::write(repo.join("a.txt"), "edited\n").unwrap();
    fs::write(repo.join("new file ✓.txt"), "untracked\n").unwrap();
    let mut store = FleetStore::open(&tmp.path().join("events.sqlite")).unwrap();

    let rescue_ref = rescue_worktree(&mut store, &repo, 42).unwrap();

    assert!(rescue_ref.starts_with("refs/heads/rescue/"), "{rescue_ref}");
    assert!(rescue_ref.ends_with("-42"), "{rescue_ref}");
    assert_eq!(
        git(&repo, &["show", &format!("{rescue_ref}:a.txt")]),
        "edited"
    );
    assert_eq!(
        git(&repo, &["show", &format!("{rescue_ref}:new file ✓.txt")]),
        "untracked"
    );
    // The rescue commit sits on top of HEAD, and the shared stash is untouched.
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{rescue_ref}^")]),
        git(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&repo, &["stash", "list"]), "");
    // The worktree itself is not modified by the rescue.
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "edited\n");
}

#[test]
fn rescue_fails_loudly_outside_a_repo_so_tidy_keeps_the_tree() {
    let tmp = non_repo_tempdir();
    let plain = tmp.path().join("not-a-repo");
    fs::create_dir(&plain).unwrap();
    fs::write(plain.join("precious.txt"), "keep me").unwrap();
    let mut store = FleetStore::open(&tmp.path().join("events.sqlite")).unwrap();
    assert!(rescue_worktree(&mut store, &plain, 1).is_err());

    let decision = TidyDecision::RemoveTree {
        path: plain.to_string_lossy().into_owned(),
        rescue: true,
        because: "test".into(),
    };
    apply_tidy_decisions(&mut store, &[decision], 1, false).unwrap();
    assert_eq!(
        fs::read_to_string(plain.join("precious.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn git_stdout_reports_failure_instead_of_empty_output() {
    let tmp = non_repo_tempdir();
    let err = git_stdout(tmp.path(), &["rev-parse", "HEAD"]).unwrap_err();
    assert!(err.to_string().contains("git rev-parse failed"), "{err}");
}

#[test]
fn write_atomic_replaces_whole_file_and_leaves_no_temp() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("snapshot.json");
    fs::write(&path, "old-but-longer-content").unwrap();
    let big = "ü".repeat(1 << 20);
    write_atomic(&path, big.as_bytes()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), big);
    write_atomic(&path, b"{}").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn write_atomic_into_missing_dir_errors() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(write_atomic(&tmp.path().join("nope/x.json"), b"1").is_err());
}

#[test]
fn verify_agent_pid_refuses_mismatched_starttime_and_dead_pids() {
    let me = std::process::id();
    let st = proc_pid_starttime(me).expect("own starttime");
    assert!(verify_agent_pid(me, "k", None, Some(st)));
    assert!(!verify_agent_pid(me, "k", None, Some(st + 1)));
    assert!(!verify_agent_pid(0, "k", None, None));
    // No recorded start time and no attribution: refuse.
    assert!(!verify_agent_pid(me, "no-such-agent-key-xyz", None, None));
}

// =============================================================================
// Goal 2: `next` Priority Instruction Tests
// =============================================================================

#[test]
fn next_priority_order_hands_then_fix_then_land_then_stale_hold_then_working_then_tick() {
    let now = 100_000;
    let mut st = FleetState::default();
    let mut live = BTreeSet::new();

    // 6. Default when idle: "run tick"
    assert_eq!(compute_next_instruction(&st, &live, now), "run tick");

    // 5. Working when an agent is live
    st.agents.insert(
        "worker-1".to_string(),
        AgentRun {
            key: "worker-1".to_string(),
            pr: Some(10),
            ..Default::default()
        },
    );
    live.insert("worker-1".to_string());
    assert_eq!(
        compute_next_instruction(&st, &live, now),
        "working: 1 agent(s), 0 batch(es)"
    );

    // 4. Stale hold (> 24h) beats working
    st.holds.insert(
        "pr:50".to_string(),
        Hold {
            reason: "waiting on security review".to_string(),
            since: now - STALE_HOLD_SECS - 3600,
        },
    );
    let inst_stale = compute_next_instruction(&st, &live, now);
    assert!(
        inst_stale.starts_with("decide: release or retire pr:50"),
        "got: {inst_stale}"
    );

    // 3. Ready PR to land beats stale hold
    st.passed.insert(
        20,
        PassedRecord {
            pr: 20,
            sha: "abc".to_string(),
            main_sha: "base".to_string(),
            batch_id: "b001".to_string(),
            passed_at: now,
            tree: "tree".to_string(),
            areas: vec!["core".to_string()],
        },
    );
    let inst_land = compute_next_instruction(&st, &live, now);
    assert!(
        inst_land.contains("ask the owner to land PR #20"),
        "got: {inst_land}"
    );

    // 2. Failed PR fix beats ask owner to land
    st.failed.insert(
        30,
        FailedRecord {
            pr: 30,
            sha: "def".to_string(),
            batch_id: "b002".to_string(),
            failed_at: now,
            kind: "code".to_string(),
            summary: "test_auth failed".to_string(),
            failed_tests: vec!["test_auth".to_string()],
            counterexample: None,
            log: String::new(),
        },
    );
    let inst_fix = compute_next_instruction(&st, &live, now);
    assert!(
        inst_fix.starts_with("tacho session fix-30"),
        "got: {inst_fix}"
    );

    // 1. Conflicting PR rebase (`hands`) beats failed PR fix, BUT skips PR #10 which live `worker-1` holds!
    st.open_prs_cache = vec![
        PullFacts {
            number: 10,
            branch: "feat-10".to_string(),
            base: "main".to_string(),
            state: "OPEN".to_string(),
            mergeable: "CONFLICTING".to_string(),
            ..Default::default()
        },
        PullFacts {
            number: 12,
            branch: "feat-12".to_string(),
            base: "main".to_string(),
            state: "OPEN".to_string(),
            mergeable: "CONFLICTING".to_string(),
            ..Default::default()
        },
    ];
    let inst_hands = compute_next_instruction(&st, &live, now);
    assert!(
        inst_hands.starts_with("tacho session rebase-12"),
        "expected PR #10 to be skipped (held by live worker-1) and PR #12 selected, got: {inst_hands}"
    );
}

// =============================================================================
// Goal 3: `todo`, `retire`, `session` Unit Tests
// =============================================================================

#[test]
fn detect_brief_source_reads_file_or_briefs_subdir_and_rejects_missing_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::write(cwd.join("task.md"), "Read from direct file\n").unwrap();
    fs::create_dir_all(cwd.join("briefs")).unwrap();
    fs::write(cwd.join("briefs/sub.md"), "Read from briefs dir\n").unwrap();

    // 1. Direct file auto-detection
    let r1 = detect_brief_source(cwd, Some("task.md"), None).unwrap();
    assert_eq!(r1.text, "Read from direct file\n");
    assert_eq!(r1.path.as_deref(), Some("task.md"));

    // 2. `briefs/` subdirectory auto-detection
    let r2 = detect_brief_source(cwd, Some("sub.md"), None).unwrap();
    assert_eq!(r2.text, "Read from briefs dir\n");
    assert_eq!(r2.path.as_deref(), Some("briefs/sub.md"));

    // 3. Inline brief string with spaces
    let r3 = detect_brief_source(cwd, Some("Fix race in wal.rs"), None).unwrap();
    assert_eq!(r3.text, "Fix race in wal.rs");
    assert!(r3.path.is_none());

    // 4. Nonexistent `.md` path errors instead of queueing path string
    assert!(detect_brief_source(cwd, Some("missing-file.md"), None).is_err());
}

#[test]
fn infer_provider_from_model_maps_gpt_and_claude_families() {
    assert_eq!(infer_provider_from_model("gpt-5.4"), Some("codex"));
    assert_eq!(infer_provider_from_model("o3-mini"), Some("codex"));
    assert_eq!(infer_provider_from_model("claude-opus-4-6"), Some("claude"));
    assert_eq!(infer_provider_from_model("sonnet"), Some("claude"));
    assert_eq!(infer_provider_from_model("opus"), Some("claude"));
    assert_eq!(infer_provider_from_model("haiku"), Some("claude"));
    assert_eq!(infer_provider_from_model("gemini-2.5-pro"), Some("gemini"));
}

#[test]
fn retire_reason_validation_and_session_argv_without_shell() {
    assert!(validate_retire_reason("   ").is_err());
    assert_eq!(
        validate_retire_reason("  superseded by #42 ").unwrap(),
        "superseded by #42"
    );

    let providers = builtin_providers();
    let (prog, argv, _env) = build_session_argv(
        "claude",
        Some("claude-sonnet-4-6"),
        Some("high"),
        None,
        "Fix bug",
        "/tmp/wt",
        "fix-bug",
        &providers,
    );
    assert_eq!(prog, "claude");
    assert!(!argv.iter().any(|a| a == "-c" || a == "sh" || a == "bash"));
}

// =============================================================================
// Goal 4: `batch` Origin, Counterexample, and Env Precedence Tests
// =============================================================================

#[test]
fn batch_origin_and_proptest_counterexample_detection() {
    assert_eq!(BatchOrigin::parse("daemon"), BatchOrigin::Daemon);
    assert_eq!(BatchOrigin::parse("manual"), BatchOrigin::Manual);
    assert_eq!(BatchOrigin::parse("train"), BatchOrigin::Train);

    assert!(has_proptest_counterexample(
        "thread 'prop' panicked at 'Test failed: minimal failing input: x = 42'"
    ));
    assert!(has_proptest_counterexample("proptest: counterexample: [0]"));
    assert!(!has_proptest_counterexample(
        "test core::wal::test_checkpoint ... FAILED"
    ));
}

#[test]
fn select_batch_checks_filters_by_requested_areas() {
    let rules = vec![
        AreaRule {
            area: "core".to_string(),
            prefixes: vec!["crates/tacho-core/".to_string()],
            unit_prefixes: BTreeMap::new(),
            checks: vec![("core".to_string(), "cargo test -p tacho-core".to_string())],
            isolate_batch: false,
            needs_screenshot: false,
        },
        AreaRule {
            area: "cli".to_string(),
            prefixes: vec!["crates/tacho-cli/".to_string()],
            unit_prefixes: BTreeMap::new(),
            checks: vec![("cli".to_string(), "cargo test -p tacho-cli".to_string())],
            isolate_batch: false,
            needs_screenshot: false,
        },
    ];
    let files = vec![
        "crates/tacho-core/src/lib.rs".to_string(),
        "crates/tacho-cli/src/main.rs".to_string(),
    ];
    let req = vec!["core".to_string()];
    let (areas, checks) = select_batch_checks(&files, &rules, Some(&req));
    assert_eq!(areas, vec!["core".to_string()]);
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].area, "core");
}

#[test]
fn parse_systemd_env_file_and_load_fleet_env_precedence() {
    let parsed = parse_systemd_env_file(
        "# comment\nTACHO_MAX_WORKERS=\"8\"\nexport CUSTOM_VAR='from-file'\n",
    );
    assert_eq!(
        parsed.get("TACHO_MAX_WORKERS").map(String::as_str),
        Some("8")
    );
    assert_eq!(
        parsed.get("CUSTOM_VAR").map(String::as_str),
        Some("from-file")
    );

    let tmp = tempfile::tempdir().unwrap();
    let env_path = tmp.path().join("fleet.env");
    fs::write(&env_path, "PATH=/overridden-by-file\nFILE_ONLY_KEY=hello\n").unwrap();
    let merged = load_fleet_env(Some(&env_path));
    // Process environment `PATH` must win over `fleet.env`!
    assert_ne!(
        merged.get("PATH").map(String::as_str),
        Some("/overridden-by-file")
    );
    assert_eq!(
        merged.get("FILE_ONLY_KEY").map(String::as_str),
        Some("hello")
    );
}

// =============================================================================
// Goal 5: `train`, `release-train`, `land` Tests
// =============================================================================

#[test]
fn order_train_candidates_places_non_frontend_first_and_groups_adjacent_screens() {
    let prs = vec![
        PullFacts {
            number: 1,
            files: vec![
                "ui/screens/login.tsx".to_string(),
                "ui/screens/a.tsx".to_string(),
                "ui/screens/b.tsx".to_string(),
            ],
            ..Default::default()
        },
        PullFacts {
            number: 2,
            files: vec![
                "crates/core/a.rs".to_string(),
                "crates/core/b.rs".to_string(),
            ],
            ..Default::default()
        },
        PullFacts {
            number: 3,
            files: vec!["crates/core/c.rs".to_string()],
            ..Default::default()
        },
        PullFacts {
            number: 4,
            files: vec!["ui/screens/settings.tsx".to_string()],
            ..Default::default()
        },
        PullFacts {
            number: 5,
            files: vec!["ui/screens/login.png".to_string()],
            ..Default::default()
        },
    ];
    let ordered = order_train_candidates(&prs, Some(4));
    let nums: Vec<u64> = ordered.iter().map(|p| p.number).collect();
    // Non-frontend (#3 size 1, #2 size 2) come before frontend (#4 size 1, #5 size 1)
    assert_eq!(nums, vec![3, 2, 4, 5]);
}

#[test]
fn classify_train_conflict_applies_png_generators_and_rebase_policies() {
    assert_eq!(
        classify_train_conflict(&["shots/home.png".to_string()]),
        TrainConflictResolution::TakeLaterPng {
            files: vec!["shots/home.png".to_string()]
        }
    );
    assert_eq!(
        classify_train_conflict(&["config/generators.json".to_string()]),
        TrainConflictResolution::RegenerateGeneratorsJson {
            files: vec!["config/generators.json".to_string()]
        }
    );
    assert_eq!(
        classify_train_conflict(&["shots/home.png".to_string(), "src/main.rs".to_string()]),
        TrainConflictResolution::DropAndQueueRebase {
            conflicting_files: vec!["src/main.rs".to_string()]
        }
    );
}

#[test]
fn transition_release_notice_never_retries_uncertain_state() {
    let pending = NoticeState::Pending;
    let (uncertain, attempted1) =
        transition_release_notice(&pending, "sha-111", 1000, |_| Err("smtp timeout".into()));
    assert!(attempted1);
    assert_eq!(uncertain.as_str(), "Uncertain");

    // Calling transition_release_notice again for the same head must NOT retry!
    let mut called = false;
    let (after, attempted2) = transition_release_notice(&uncertain, "sha-111", 1010, |_| {
        called = true;
        Ok(())
    });
    assert!(!attempted2);
    assert!(!called);
    assert_eq!(after, uncertain);

    // If the train head advances to a new SHA (`sha-222`), a fresh notice is allowed.
    let (sent_new, attempted3) = transition_release_notice(&after, "sha-222", 1020, |_| Ok(()));
    assert!(attempted3);
    assert_eq!(sent_new.as_str(), "Sent");
}

#[test]
fn verify_train_land_integrity_checks_base_and_post_merge_tree() {
    let train = TrainRecord {
        id: "t001".to_string(),
        prs: vec![10, 11],
        shas: BTreeMap::new(),
        base: "base-sha-1".to_string(),
        tree: "verified-tree-sha".to_string(),
        batch_id: "b001".to_string(),
        status: "open".to_string(),
        created_at: 100,
        ..Default::default()
    };

    // 1. Exact match succeeds
    assert!(verify_train_land_integrity(&train, "base-sha-1", "verified-tree-sha").is_ok());

    // 2. Moved `main` is refused
    assert!(verify_train_land_integrity(&train, "base-sha-2", "verified-tree-sha").is_err());

    // 3. Post-merge tree mismatch is flagged
    assert!(verify_train_land_integrity(&train, "base-sha-1", "different-tree-sha").is_err());
}

// =============================================================================
// Goal 6: `replay` PR Branch & Decision Replay Tests
// =============================================================================

#[test]
fn replay_pr_branch_preserves_unknown_replays_clean_and_marks_conflicts_needs_hands() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let mut store = FleetStore::open(&tmp.path().join("events.sqlite")).unwrap();

    // 1. UNKNOWN mergeable state is preserved untouched
    let unknown_pr = PullFacts {
        number: 70,
        branch: "feat-70".to_string(),
        base: "main".to_string(),
        mergeable: "UNKNOWN".to_string(),
        ..Default::default()
    };
    let out_unknown = replay_pr_branch(&mut store, &repo, &unknown_pr, false).unwrap();
    assert_eq!(out_unknown, PrReplayOutcome::PreservedUnknown { pr: 70 });

    // 2. Create a clean feature branch `feat-71` and advance `main` on a non-overlapping file
    git(&repo, &["checkout", "-q", "-b", "feat-71"]);
    fs::write(repo.join("b.txt"), "feature 71\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-q", "-m", "feat 71"]);
    let feat71_sha = git(&repo, &["rev-parse", "HEAD"]);

    // Create a conflicting feature branch `feat-72` that edits `a.txt`
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feat-72"]);
    fs::write(repo.join("a.txt"), "conflicting feat 72\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "feat 72"]);
    let feat72_sha = git(&repo, &["rev-parse", "HEAD"]);

    // Advance `main` on `a.txt`
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("a.txt"), "main advanced\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "advance main"]);

    // Clean replay of PR #71 succeeds
    let clean_pr = PullFacts {
        number: 71,
        branch: "feat-71".to_string(),
        head: feat71_sha,
        base: "main".to_string(),
        mergeable: "MERGEABLE".to_string(),
        files: vec!["b.txt".to_string()],
        ..Default::default()
    };
    let out_clean = replay_pr_branch(&mut store, &repo, &clean_pr, false).unwrap();
    assert!(matches!(
        out_clean,
        PrReplayOutcome::Replayed { pr: 71, .. }
    ));

    // Conflicting replay of PR #72 marks `needs-hands` and queues `rebase-72`
    let conflict_pr = PullFacts {
        number: 72,
        branch: "feat-72".to_string(),
        head: feat72_sha,
        base: "main".to_string(),
        mergeable: "CONFLICTING".to_string(),
        files: vec!["a.txt".to_string()],
        ..Default::default()
    };
    let out_conflict = replay_pr_branch(&mut store, &repo, &conflict_pr, false).unwrap();
    assert_eq!(
        out_conflict,
        PrReplayOutcome::NeedsHands {
            pr: 72,
            conflicting_files: vec!["a.txt".to_string()]
        }
    );
    let st = store.load().unwrap();
    assert!(st.todo.iter().any(|t| t.key == "rebase-72"));
}

// =============================================================================
// Goal 7: `tidy`, `events`, `watchdog`, `self-update`, `setup`, `Notifier` Tests
// =============================================================================

#[test]
fn tidy_live_process_holds_and_stale_runs_retirement() {
    let procs = vec![ProcEntry {
        pid: 9999,
        ppid: 1,
        pgid: 9999,
        state: "S".to_string(),
        cpu_ticks: 10,
        elapsed_secs: 30,
        comm: "cargo".to_string(),
        args: "cargo test --manifest-path /tmp/wt-alpha/Cargo.toml".to_string(),
        cwd: "/tmp/wt-beta/sub".to_string(),
        env_starve: false,
    }];
    let candidates = vec![
        "/tmp/wt-alpha".to_string(),
        "/tmp/wt-beta".to_string(),
        "/tmp/wt-unused".to_string(),
    ];
    let holds = scan_live_process_holds(&procs, &candidates);
    assert!(holds.paths.contains("/tmp/wt-alpha"));
    assert!(holds.paths.contains("/tmp/wt-beta"));
    assert!(!holds.paths.contains("/tmp/wt-unused"));

    let tmp = tempfile::tempdir().unwrap();
    let mut store = FleetStore::open(&tmp.path().join("events.sqlite")).unwrap();
    let now = 1_000_000;
    store
        .mutate(None, |st| {
            st.agents.insert(
                "old-dead".to_string(),
                AgentRun {
                    key: "old-dead".to_string(),
                    started_at: now - 10 * 86_400,
                    finished_at: Some(now - 10 * 86_400 + 60),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    assert!(retire_stale_runs(&mut store, 0, now, false).is_err());
    let retired = retire_stale_runs(&mut store, 7, now, false).unwrap();
    assert_eq!(retired, vec!["old-dead".to_string()]);
    assert!(store.load().unwrap().retired.contains("old-dead"));
}

#[test]
fn events_since_and_limit_filtering() {
    let now = 10_000;
    assert_eq!(parse_since_cutoff("10m", now).unwrap(), 10_000 - 600);
    assert_eq!(parse_since_cutoff("2h", now).unwrap(), 10_000 - 7_200);
    assert!(parse_since_cutoff("invalid", now).is_err());

    let records = vec![
        DecisionRecord {
            id: 1,
            ts: 9_000,
            category: "dispatch".to_string(),
            action: "todo".to_string(),
            target: "t1".to_string(),
            because: "b1".to_string(),
        },
        DecisionRecord {
            id: 2,
            ts: 9_500,
            category: "verify".to_string(),
            action: "batch_pass".to_string(),
            target: "b001".to_string(),
            because: "b2".to_string(),
        },
        DecisionRecord {
            id: 3,
            ts: 9_900,
            category: "dispatch".to_string(),
            action: "start".to_string(),
            target: "t1".to_string(),
            because: "b3".to_string(),
        },
    ];
    let filtered = filter_decision_events(&records, Some(9_400), None, None, 1);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, 3);
}

#[test]
fn watchdog_issue_open_update_close_lifecycle() {
    let now = 50_000;
    // 1. Down with no open issue -> Open
    let (step1, issue1) = run_watchdog_lifecycle(Some("tick is 15m old"), None, now, 3600);
    assert!(matches!(step1, WatchdogStep::Open { .. }));
    assert!(issue1.is_some());

    // 2. Still down before cooldown -> None
    let (step2, issue2) = run_watchdog_lifecycle(Some("tick is 20m old"), issue1, now + 600, 3600);
    assert_eq!(step2, WatchdogStep::None);

    // 3. Still down after cooldown -> Update
    let (step3, issue3) = run_watchdog_lifecycle(Some("tick is 80m old"), issue2, now + 4000, 3600);
    assert!(matches!(step3, WatchdogStep::Update { number: 1, .. }));

    // 4. Recovered -> Close
    let (step4, issue4) = run_watchdog_lifecycle(None, issue3, now + 4100, 3600);
    assert!(matches!(step4, WatchdogStep::Close { number: 1, .. }));
    assert!(issue4.is_none());
}

#[test]
fn self_update_rolls_back_on_failed_test_and_updates_on_green() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let base_sha = git(&repo, &["rev-parse", "HEAD"]);

    fs::write(repo.join("a.txt"), "candidate\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "candidate"]);
    let candidate_sha = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "--detach", &base_sha]);

    let mut store = FleetStore::open(&tmp.path().join("events.sqlite")).unwrap();

    // 1. Failed self-test rolls back to `base_sha` and records `refused_self_update_sha`
    let res_fail = run_self_update(&mut store, &repo, &candidate_sha, "false").unwrap();
    assert!(matches!(res_fail, SelfUpdateOutcome::RolledBack { .. }));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base_sha);
    assert_eq!(
        store.load().unwrap().refused_self_update_sha.as_deref(),
        Some(candidate_sha.as_str())
    );

    // 2. Subsequent attempt on refused SHA is skipped
    let res_skip = run_self_update(&mut store, &repo, &candidate_sha, "true").unwrap();
    assert!(matches!(res_skip, SelfUpdateOutcome::Skipped(_)));

    // 3. Clear refusal and verify green self-test fast-forwards
    store
        .mutate(None, |st| st.refused_self_update_sha = None)
        .unwrap();
    let res_ok = run_self_update(&mut store, &repo, &candidate_sha, "true").unwrap();
    assert_eq!(
        res_ok,
        SelfUpdateOutcome::Updated {
            from_sha: base_sha,
            to_sha: candidate_sha.clone(),
        }
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), candidate_sha);
}

#[test]
fn setup_idempotency_and_notifier_scrubber() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("tacho-home");
    let systemd = tmp.path().join("systemd");

    let rep1 = run_setup_with(
        &home,
        &systemd,
        &["git", "definitely-missing-bin-xyz"],
        false,
    )
    .unwrap();
    assert!(rep1.env_created);
    assert_eq!(rep1.units_installed.len(), 4);
    assert_eq!(
        rep1.missing_deps,
        vec!["definitely-missing-bin-xyz".to_string()]
    );

    // Second run is idempotent (`env_created == false`, symlinks `Noop`)
    let rep2 = run_setup_with(&home, &systemd, &["git"], false).unwrap();
    assert!(!rep2.env_created);
    assert_eq!(
        apply_link_action(
            &systemd.join("tacho.service"),
            &home.join("units/tacho.service"),
            true
        )
        .unwrap(),
        LinkAction::Noop
    );

    // Owner Notifier and snapshot writer scrub GitHub/API tokens
    let secret = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let notifier = Notifier::new();
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    let mut seen_body = String::new();
    let sent = notifier
        .notify_owner(
            &mut store,
            "fail-1",
            &format!("Alert {secret}"),
            &format!("Token leaked: {secret}"),
            |_subj, body| {
                seen_body = body.to_string();
                Ok(())
            },
        )
        .unwrap();
    assert!(sent);
    assert!(!seen_body.contains(secret));

    let snap_path = tmp.path().join("snapshot.json");
    write_scrubbed_snapshot(&snap_path, &serde_json::json!({ "note": secret })).unwrap();
    let snap_txt = fs::read_to_string(&snap_path).unwrap();
    assert!(!snap_txt.contains(secret));
}

#[test]
fn pid_alive_and_live_agent_keys_verify_starttime_and_fail_closed() {
    let my_pid = std::process::id();
    let my_st = proc_pid_starttime(my_pid).expect("/proc/self/stat starttime");

    assert!(pid_alive(my_pid));
    assert!(!pid_alive(0));
    assert!(!pid_alive(u32::MAX));
    assert!(pid_alive_matching(my_pid, Some(my_st)));
    assert!(!pid_alive_matching(my_pid, Some(my_st + 999)));

    let mut st = FleetState::default();
    st.agents.insert(
        "valid-agent".to_string(),
        AgentRun {
            key: "valid-agent".to_string(),
            pid: Some(my_pid),
            starttime: Some(my_st),
            ..Default::default()
        },
    );
    st.agents.insert(
        "recycled-pid-agent".to_string(),
        AgentRun {
            key: "recycled-pid-agent".to_string(),
            pid: Some(my_pid),
            starttime: Some(my_st + 1),
            ..Default::default()
        },
    );
    let live = live_agent_keys(&st);
    assert!(live.contains("valid-agent"));
    assert!(
        !live.contains("recycled-pid-agent"),
        "recycled PID with mismatched starttime must never be treated as live"
    );
    assert!(!verify_agent_pid(
        my_pid,
        "recycled-pid-agent",
        None,
        Some(my_st + 1)
    ));
}

#[test]
fn concurrent_fleet_store_mutations_preserve_integrity() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = std::sync::Arc::new(tmp.path().join("events.sqlite"));
    {
        let _store = FleetStore::open(&db_path).unwrap();
    }

    let mut handles = Vec::new();
    for t in 0..6usize {
        let db = std::sync::Arc::clone(&db_path);
        handles.push(std::thread::spawn(move || {
            let mut store = FleetStore::open(&db).unwrap();
            for i in 0..8usize {
                let key = format!("task-{t}-{i}");
                store
                    .mutate(Some(("dispatch", "todo", &key, "concurrent test")), |st| {
                        st.todo.push(tacho_core::decide::dispatch::TodoItem {
                            key: key.clone(),
                            brief: format!("Brief {key}"),
                            ..Default::default()
                        });
                    })
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let store = FleetStore::open(&db_path).unwrap();
    let final_state = store.load().unwrap();
    assert_eq!(final_state.todo.len(), 48);
}

#[test]
fn start_worker_whose_directory_is_main_checkout_never_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let main_checkout = tmp.path().join("repo");
    std::fs::create_dir_all(&main_checkout).unwrap();

    git(&main_checkout, &["init", "-q", "-b", "main"]);
    git(&main_checkout, &["config", "user.email", "t@example.com"]);
    git(&main_checkout, &["config", "user.name", "Tester"]);
    std::fs::write(main_checkout.join("README.md"), "main checkout\n").unwrap();
    git(&main_checkout, &["add", "README.md"]);
    git(&main_checkout, &["commit", "-q", "-m", "initial"]);

    let marker = tmp.path().join("worker-ran.marker");
    let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
    let main_str = main_checkout.to_string_lossy().to_string();
    let mut touch_provider = tacho_core::decide::dispatch::ProviderSpec::claude();
    touch_provider.name = "touch-marker".to_string();
    touch_provider.binary = "touch".to_string();
    touch_provider.start_args = vec![marker.display().to_string()];
    store
        .mutate(None, |st| {
            st.default_provider = "touch-marker".to_string();
            st.providers
                .insert("touch-marker".to_string(), touch_provider);
            st.worktrees.insert(
                "bad-worker".to_string(),
                tacho_daemon::WorktreeEntry {
                    slug: "bad-worker".to_string(),
                    path: main_str.clone(),
                    branch: "main".to_string(),
                    base_sha: String::new(),
                    base_pr: None,
                    port: 3300,
                    port_last: 3319,
                    created_at: 100,
                },
            );
        })
        .unwrap();

    let item = tacho_core::decide::dispatch::TodoItem {
        key: "bad-worker".to_string(),
        brief: "Attempt to run in main checkout".to_string(),
        provider: Some("touch-marker".to_string()),
        ..Default::default()
    };

    // 1. Attempt to start via recorded worktree pointing at main checkout: must be refused before spawn.
    let res1 = super::spawn_worker_for_item(&mut store, &item);
    assert!(
        res1.is_err(),
        "worker whose directory is the main checkout must be refused"
    );
    assert!(
        !marker.exists(),
        "worker whose directory is the main checkout must never run"
    );

    // 2. Attempt to start via explicit directory override pointing at main checkout: must also be refused.
    let item2 = tacho_core::decide::dispatch::TodoItem {
        key: "bad-worker-2".to_string(),
        brief: "Attempt explicit main checkout dir".to_string(),
        provider: Some("touch-marker".to_string()),
        ..Default::default()
    };
    let res2 = super::spawn_worker_in_dir(&mut store, &item2, Some(&main_checkout));
    assert!(res2.is_err());
    assert!(
        !marker.exists(),
        "worker whose directory is the main checkout must never run"
    );

    let st = store.load().unwrap();
    assert!(
        st.holds.contains_key("bad-worker"),
        "refused worker must be placed on hold"
    );
    assert!(
        st.holds.contains_key("bad-worker-2"),
        "refused worker must be placed on hold"
    );
    assert!(
        !st.agents.contains_key("bad-worker") && !st.agents.contains_key("bad-worker-2"),
        "refused worker must not be recorded as a running agent"
    );
    let decisions = store.recent_decisions(20, None, None).unwrap();
    assert!(
        decisions
            .iter()
            .any(|d| d.action == "start_refused" && d.target == "bad-worker"),
        "expected start_refused decision for bad-worker, got: {:?}",
        decisions
    );
}
