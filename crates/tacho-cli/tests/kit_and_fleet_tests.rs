//! End-to-end integration tests for the worker kit (`start`, `prove`, `job`, `wait`, `kill`, `pr`,
//! `note`, `starve`), hook guardrails (`TACHO_GUARDRAILS=1`), fleet orchestrator (`todo`, `hold`,
//! `release`, `retire`, `attach`, `batch`, `flakes`, `train`, `land`, `tidy`, `tick`, `events`),
//! and Perfetto trace projection (`tacho trace`).

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tacho_core::decide::dispatch::PullFacts;
use tacho_core::decide::verify::AreaRule;
use tacho_daemon::FleetStore;

fn tacho_bin() -> String {
    env!("CARGO_BIN_EXE_tacho").to_string()
}

fn init_git_repo(dir: &Path) -> String {
    let run = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command failed to spawn");
        assert!(st.success(), "git {:?} failed", args);
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "dev@example.com"]);
    run(&["config", "user.name", "Developer"]);
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
fn test_kit_start_prove_and_pr_preflight() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let repo_dir = tmp.path().join("repo");
    let wt_root = tmp.path().join("worktrees");
    fs::create_dir_all(&repo_dir).unwrap();
    let base_sha = init_git_repo(&repo_dir);

    // Configure an area rule with a fast check command so `tacho prove` doesn't need a full cargo workspace
    let mut store = FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |st| {
            st.area_rules.push(AreaRule {
                area: "docs".to_string(),
                prefixes: vec!["docs/".to_string(), "README.md".to_string()],
                checks: vec![("docs-check".to_string(), "test -f README.md".to_string())],
                isolate_batch: false,
                needs_screenshot: false,
            });
        })
        .unwrap();

    // 1. Run `tacho start feat-alpha`
    let out = Command::new(tacho_bin())
        .args([
            "start",
            "feat-alpha",
            "--base",
            &base_sha,
            "--dir",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tacho start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let wt_dir = wt_root.join("feat-alpha");
    assert!(wt_dir.join(".tachoenv").is_file());

    // Idempotency: re-running `tacho start feat-alpha` reuses the exact same port block
    let st1 = store.load().unwrap();
    let p1 = st1.worktrees.get("feat-alpha").unwrap().port;
    let out2 = Command::new(tacho_bin())
        .args(["start", "feat-alpha", "--dir", wt_root.to_str().unwrap()])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let st2 = store.load().unwrap();
    assert_eq!(st2.worktrees.get("feat-alpha").unwrap().port, p1);

    // 2. Make a change in the worktree and commit it
    fs::write(wt_dir.join("README.md"), "# Test Repo\nUpdated docs.\n").unwrap();
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&wt_dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "Update README documentation"])
        .current_dir(&wt_dir)
        .status()
        .unwrap();

    // 3. `tacho pr --dry-run` BEFORE `tacho prove` must be refused
    let pr_before_prove = Command::new(tacho_bin())
        .args([
            "pr",
            "Update README documentation",
            "-m",
            "Adds documentation details to README.",
            "--dry-run",
        ])
        .current_dir(&wt_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(!pr_before_prove.status.success());
    assert!(
        String::from_utf8_lossy(&pr_before_prove.stderr).contains("no proof found"),
        "stderr: {}",
        String::from_utf8_lossy(&pr_before_prove.stderr)
    );

    // 4. Run `tacho prove`
    let prove_out = Command::new(tacho_bin())
        .args(["prove"])
        .current_dir(&wt_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        prove_out.status.success(),
        "tacho prove failed: {}",
        String::from_utf8_lossy(&prove_out.stderr)
    );
    assert!(wt_dir.join(".tachoproof/summary.json").is_file());

    // 5. `tacho pr --dry-run` now succeeds and prints the Proof table
    let pr_ok = Command::new(tacho_bin())
        .args([
            "pr",
            "Update README documentation",
            "-m",
            "Adds documentation details to README.",
            "--dry-run",
        ])
        .current_dir(&wt_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        pr_ok.status.success(),
        "tacho pr --dry-run failed: {}",
        String::from_utf8_lossy(&pr_ok.stderr)
    );
    let stdout = String::from_utf8_lossy(&pr_ok.stdout);
    assert!(stdout.contains("## Proof"), "stdout: {stdout}");
    assert!(stdout.contains("docs"), "stdout: {stdout}");

    // 6. Dirtying a tracked file invalidates the proof tree and causes `tacho pr` to refuse
    fs::write(wt_dir.join("README.md"), "# Dirty after prove\n").unwrap();
    let pr_dirty = Command::new(tacho_bin())
        .args([
            "pr",
            "Update README documentation",
            "-m",
            "Adds documentation details to README.",
            "--dry-run",
        ])
        .current_dir(&wt_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(!pr_dirty.status.success());
    assert!(
        String::from_utf8_lossy(&pr_dirty.stderr).contains("uncommitted changes"),
        "stderr: {}",
        String::from_utf8_lossy(&pr_dirty.stderr)
    );
}

#[test]
fn test_kit_job_wait_and_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    // 1. Short job name `db` (2 chars) is accepted and `tacho wait` streams all output
    let job_out = Command::new(tacho_bin())
        .args(["job", "db", "--", "bash", "-c", "echo hello-job && exit 0"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        job_out.status.success(),
        "job failed: {}",
        String::from_utf8_lossy(&job_out.stderr)
    );

    let wait_out = Command::new(tacho_bin())
        .args(["wait", "db", "--timeout", "10"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(wait_out.status.success());
    assert!(String::from_utf8_lossy(&wait_out.stdout).contains("hello-job"));

    // 2. Long-running job returns exit code 3 on timeout, and `tacho kill` terminates its process group
    let bg_out = Command::new(tacho_bin())
        .args(["job", "srv", "--", "sleep", "60"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(bg_out.status.success());

    let wait_timeout = Command::new(tacho_bin())
        .args(["wait", "srv", "--timeout", "1"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert_eq!(wait_timeout.status.code(), Some(3));

    let kill_out = Command::new(tacho_bin())
        .args(["kill", "srv", "--now"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(kill_out.status.success());
}

#[test]
fn test_hook_guardrails_opt_in_and_passive_default() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");

    let bad_payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {
            "command": "git stash"
        }
    })
    .to_string();

    // 1. Default mode (without TACHO_GUARDRAILS=1) is 100% passive: exits 0 and prints nothing to stdout
    let mut child_passive = Command::new(tacho_bin())
        .args(["hook", "claude"])
        .env("TACHO_HOME", &tacho_home)
        .env_remove("TACHO_GUARDRAILS")
        .env_remove("TACHO_MANAGED_WORKER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child_passive
        .stdin
        .take()
        .unwrap()
        .write_all(bad_payload.as_bytes())
        .unwrap();
    let out_passive = child_passive.wait_with_output().unwrap();
    assert_eq!(out_passive.status.code(), Some(0));
    assert!(out_passive.stdout.is_empty());

    // 2. Managed worker mode (`TACHO_GUARDRAILS=1`) blocks `git stash`, `run_in_background`, `--no-verify`, etc. with exit code 2
    for payload in [
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "git stash" }
        }),
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test", "run_in_background": true }
        }),
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "git commit --no-verify -m 'wip'" }
        }),
    ] {
        let mut child = Command::new(tacho_bin())
            .args(["hook", "claude"])
            .env("TACHO_HOME", &tacho_home)
            .env("TACHO_GUARDRAILS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(2), "expected exit 2 for {payload}");
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn test_fleet_batch_bisect_flake_train_land_and_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    let main_sha = init_git_repo(&repo_dir);

    // 1. Queue a compare task (`--compare sonnet,codex`) and verify expansion
    let todo_out = Command::new(tacho_bin())
        .args([
            "todo",
            "opt-cache",
            "Optimize query cache",
            "--area",
            "core",
            "--compare",
            "sonnet,codex",
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(todo_out.status.success());
    let store = FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    let st = store.load().unwrap();
    assert_eq!(st.todo.len(), 2);
    assert_eq!(st.todo[0].key, "opt-cache-sonnet");
    assert_eq!(st.todo[1].key, "opt-cache-codex");

    // 2. Seed open PRs #10 and #11 for batch verification
    let mut store = FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    store
        .mutate(None, |st| {
            st.open_prs_cache = vec![
                PullFacts {
                    number: 10,
                    branch: "feat-10".to_string(),
                    head: main_sha.clone(),
                    base: "main".to_string(),
                    state: "OPEN".to_string(),
                    draft: false,
                    mergeable: "MERGEABLE".to_string(),
                    files: vec!["src/a.rs".to_string()],
                    title: "Feature 10".to_string(),
                    ..Default::default()
                },
                PullFacts {
                    number: 11,
                    branch: "feat-11".to_string(),
                    head: main_sha.clone(),
                    base: "main".to_string(),
                    state: "OPEN".to_string(),
                    draft: false,
                    mergeable: "MERGEABLE".to_string(),
                    files: vec!["src/b.rs".to_string()],
                    title: "Feature 11".to_string(),
                    ..Default::default()
                },
            ];
        })
        .unwrap();

    // 3. Test flake retry (`r0` fails, `r0-again` passes -> batch passes and records flake)
    let flake_marker = tmp.path().join("flake-once");
    let flake_cmd = format!(
        "if [ ! -f '{}' ]; then touch '{}'; echo 'test core::flaky_test ... FAILED'; exit 1; else exit 0; fi",
        flake_marker.display(),
        flake_marker.display()
    );
    let batch_flake = Command::new(tacho_bin())
        .args(["batch", "10", "--cmd", &flake_cmd])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        batch_flake.status.success(),
        "batch flake retry should pass: {}",
        String::from_utf8_lossy(&batch_flake.stderr)
    );
    let st_after_flake = store.load().unwrap();
    assert!(st_after_flake.passed.contains_key(&10));
    assert!(!st_after_flake.flakes.is_empty());

    // 4. Test binary bisection blaming culprit PR #11 (`k >= 2` fails, `k < 2` passes) and auto-rerunning PR #10
    let bisect_cmd = "if [ \"$TACHO_BATCH_PROBE_K\" -ge 2 ]; then echo 'test b::bad_test ... FAILED'; echo 'proptest: minimal failing input: x = 0'; exit 1; else exit 0; fi";
    store
        .mutate(None, |st| {
            st.passed.clear();
            st.batches.clear();
        })
        .unwrap();
    let batch_bisect = Command::new(tacho_bin())
        .args(["batch", "10", "11", "--cmd", bisect_cmd])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(!batch_bisect.status.success());
    let st_after_bisect = store.load().unwrap();
    assert!(
        st_after_bisect.failed.contains_key(&11),
        "PR #11 should be blamed as culprit"
    );
    assert!(
        !st_after_bisect.failed.contains_key(&10),
        "PR #10 must not be blamed"
    );
    assert!(
        st_after_bisect.passed.contains_key(&10),
        "PR #10 should pass on automatic rerun"
    );

    // 5. Open a merge train from the green batch for PR #10 and land it
    let green_batch_id = st_after_bisect
        .batches
        .iter()
        .find(|b| b.status == "passed" && b.prs == vec![10])
        .unwrap()
        .id
        .clone();

    let train_out = Command::new(tacho_bin())
        .args(["train", "--from-batch", &green_batch_id])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(train_out.status.success());

    let land_out = Command::new(tacho_bin())
        .args(["land"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        land_out.status.success(),
        "tacho land failed: {}",
        String::from_utf8_lossy(&land_out.stderr)
    );
    let st_after_land = store.load().unwrap();
    assert!(st_after_land.merged_prs.contains(&10));

    // 6. Export a Perfetto trace (`tacho trace`) and verify non-empty valid snapshot
    let trace_path = tmp.path().join("fleet.perfetto-trace");
    let trace_out = Command::new(tacho_bin())
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        trace_out.status.success(),
        "tacho trace failed: {}",
        String::from_utf8_lossy(&trace_out.stderr)
    );
    let trace_bytes = fs::read(&trace_path).unwrap();
    assert!(
        trace_bytes.len() > 256,
        "trace should contain fleet tracks and events"
    );
}

#[test]
fn test_fleet_tick_dispatch_reap_land_mismatch_and_tidy_rescue() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let repo_dir = tmp.path().join("repo");
    let wt_root = tmp.path().join("worktrees");
    fs::create_dir_all(&repo_dir).unwrap();
    let main_sha = init_git_repo(&repo_dir);

    // 1. Start a worktree and make uncommitted edits, then verify `tacho tidy --min-age 0` rescues it to `refs/tacho/rescue/...`
    let start_out = Command::new(tacho_bin())
        .args([
            "start",
            "dirty-wt",
            "--base",
            &main_sha,
            "--dir",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(start_out.status.success());
    let wt_dir = wt_root.join("dirty-wt");
    fs::write(wt_dir.join("uncommitted.txt"), "precious work\n").unwrap();

    let tidy_out = Command::new(tacho_bin())
        .args(["tidy", "--min-age", "0"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        tidy_out.status.success(),
        "tidy failed: {}",
        String::from_utf8_lossy(&tidy_out.stderr)
    );
    let refs_out = Command::new("git")
        .args(["for-each-ref", "refs/tacho/rescue/"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    let refs_str = String::from_utf8_lossy(&refs_out.stdout);
    assert!(
        refs_str.contains("refs/tacho/rescue/dirty-wt-"),
        "expected rescue ref in: {refs_str}"
    );

    // 2. Queue a task, run `tacho tick` with a mock `TACHO_AGENT_CMD` that prints `PR: #42`, then run `tacho tick` again to reap it
    let todo_out = Command::new(tacho_bin())
        .args([
            "todo",
            "auto-task",
            "Implement feature 42",
            "--area",
            "core",
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(todo_out.status.success());

    let tick1 = Command::new(tacho_bin())
        .args(["tick"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .env("TACHO_AGENT_CMD", "echo 'Working...' && echo 'PR: #42'")
        .output()
        .unwrap();
    assert!(tick1.status.success());

    // Wait briefly for the fast mock command to exit, then run tick #2 to reap & attribute PR #42
    std::thread::sleep(std::time::Duration::from_millis(200));
    let tick2 = Command::new(tacho_bin())
        .args(["tick"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(tick2.status.success());

    let store = FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    let st = store.load().unwrap();
    let run = st
        .agents
        .get("auto-task")
        .expect("auto-task should be in agents");
    assert_eq!(run.pr, Some(42));
    assert_eq!(run.pr_source.as_deref(), Some("stated"));
    assert!(tacho_home.join("snapshot.json").is_file());
}

#[test]
fn test_strict_start_base_db_plugin_live_tell_and_batch_head_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let tacho_home = tmp.path().join("tacho-home");
    let repo_dir = tmp.path().join("repo");
    let wt_root = tmp.path().join("worktrees");
    fs::create_dir_all(&repo_dir).unwrap();
    let main_sha = init_git_repo(&repo_dir);

    // 1. `tacho start` strictly refuses an unresolvable PR number or revision without falling back to HEAD
    let bad_pr_start = Command::new(tacho_bin())
        .args([
            "start",
            "bad-pr-wt",
            "--base",
            "pr:999999",
            "--dir",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        !bad_pr_start.status.success(),
        "tacho start with non-existent PR base must fail rather than falling back to HEAD"
    );

    let bad_rev_start = Command::new(tacho_bin())
        .args([
            "start",
            "bad-rev-wt",
            "--base",
            "nonexistent-branch-xyz",
            "--dir",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(!bad_rev_start.status.success());

    // 2. `tacho start` runs per-agent database setup plugin (`TACHO_DB_UP_CMD`) and writes DATABASE_URL into `.tachoenv`
    let db_start = Command::new(tacho_bin())
        .args([
            "start",
            "db-wt",
            "--base",
            &main_sha,
            "--dir",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .env(
            "TACHO_DB_UP_CMD",
            "echo \"DATABASE_URL=postgres://postgres:postgres@127.0.0.1:${DB_PORT}/test_${SLUG}\"",
        )
        .output()
        .unwrap();
    assert!(
        db_start.status.success(),
        "tacho start with TACHO_DB_UP_CMD failed: {}",
        String::from_utf8_lossy(&db_start.stderr)
    );
    let env_txt = fs::read_to_string(wt_root.join("db-wt").join(".tachoenv")).unwrap();
    let parsed_env = tacho_core::decide::verify::parse_env(&env_txt);
    let db_url = parsed_env
        .get("DATABASE_URL")
        .expect("DATABASE_URL should be present in .tachoenv");
    assert!(db_url.starts_with("postgres://postgres:postgres@127.0.0.1:"));
    assert!(db_url.ends_with("/test_db-wt"));

    // 3. Live `tacho tell` delivers to a running worker's inbox without killing/restarting it,
    // and the next `tacho hook claude` drains and injects the message.
    let mut store = FleetStore::open(&tacho_home.join("events.sqlite")).unwrap();
    let my_pid = std::process::id(); // Guaranteed alive PID
    store
        .mutate(None, |st| {
            st.agents.insert(
                "live-worker".to_string(),
                tacho_core::decide::dispatch::AgentRun {
                    key: "live-worker".to_string(),
                    pid: Some(my_pid),
                    provider: "claude".to_string(),
                    worktree: wt_root.join("db-wt").to_string_lossy().to_string(),
                    started_at: 1000,
                    ..Default::default()
                },
            );
        })
        .unwrap();

    let tell_out = Command::new(tacho_bin())
        .args(["tell", "live-worker", "Please also add a migration test"])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(tell_out.status.success());
    let inbox_file = tacho_home.join("inbox").join("live-worker.txt");
    assert!(
        inbox_file.is_file(),
        "inbox file should exist for live worker"
    );

    let hook_payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "tool_input": { "file_path": "README.md" }
    })
    .to_string();
    let mut hook_child = Command::new(tacho_bin())
        .args(["hook", "claude"])
        .env("TACHO_HOME", &tacho_home)
        .env("TACHO_AGENT", "live-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook_child
        .stdin
        .take()
        .unwrap()
        .write_all(hook_payload.as_bytes())
        .unwrap();
    let hook_out = hook_child.wait_with_output().unwrap();
    assert_eq!(hook_out.status.code(), Some(0));
    let hook_stderr = String::from_utf8_lossy(&hook_out.stderr);
    assert!(
        hook_stderr.contains("Please also add a migration test"),
        "expected live tell message in hook stderr, got: {hook_stderr}"
    );
    assert!(
        !inbox_file.exists(),
        "inbox file should be drained after hook delivery"
    );

    // 4. `tacho batch` refuses to mark a batch as passed if a PR's head SHA moves while the batch is running
    store
        .mutate(None, |st| {
            st.open_prs_cache = vec![PullFacts {
                number: 77,
                branch: "feat-77".to_string(),
                head: main_sha.clone(),
                base: "main".to_string(),
                state: "OPEN".to_string(),
                mergeable: "MERGEABLE".to_string(),
                files: vec!["src/lib.rs".to_string()],
                title: "Feature 77".to_string(),
                ..Default::default()
            }];
        })
        .unwrap();

    // While the batch command runs, mutate PR #77's head SHA in SQLite to simulate a force-push mid-batch
    let db_path = tacho_home.join("events.sqlite");
    let move_head_cmd = format!(
        "sqlite3 '{}' \"UPDATE fleet_state SET state_json = replace(state_json, '\\\"head\\\":\\\"{}\\\"', '\\\"head\\\":\\\"deadbeef9999\\\"') WHERE id = 1;\"",
        db_path.display(),
        main_sha
    );
    let batch_moved = Command::new(tacho_bin())
        .args(["batch", "77", "--cmd", &move_head_cmd])
        .current_dir(&repo_dir)
        .env("TACHO_HOME", &tacho_home)
        .output()
        .unwrap();
    assert!(
        !batch_moved.status.success(),
        "batch should refuse to pass when PR head moves mid-batch"
    );
    let st_after_move = store.load().unwrap();
    assert!(
        !st_after_move.passed.contains_key(&77),
        "PR #77 must NOT be marked passed when its head moved during batch"
    );
    assert!(st_after_move
        .batches
        .iter()
        .any(|b| b.prs == vec![77] && b.status == "head-moved"));
}
