use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tacho_e2e::{
    perfetto_bin_path, traced_bin_path, traced_probes_bin_path, DecodedTrace, TestEnv,
};

struct TracedDaemons {
    traced: Option<Child>,
    probes: Option<Child>,
}

impl Drop for TracedDaemons {
    fn drop(&mut self) {
        if let Some(mut p) = self.probes.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        if let Some(mut t) = self.traced.take() {
            let _ = t.kill();
            let _ = t.wait();
        }
    }
}

fn run_git(repo: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git failed to execute");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn send_claude_hook(env: &TestEnv, agent: &str, payload: serde_json::Value) {
    let mut cmd = env.tacho_cmd();
    cmd.args(["hook", "claude"])
        .env("TACHO_AGENT", agent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn hook");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    let res = child.wait().unwrap();
    assert!(res.success(), "hook failed for agent {agent}");
}

#[test]
fn test_multi_agent_git_chaos() {
    if tacho_core::find_trace_processor().is_none() {
        eprintln!("Skipping test_multi_agent_git_chaos: trace_processor_shell binary not found (set $TACHO_TP or add to PATH)");
        return;
    }

    let env = TestEnv::new().unwrap();

    // 1. Create hermetic temp git repo with stacked branches:
    // main -> feat/auth-base -> feat/auth-jwt -> feat/db-migration
    let repo_dir = env.temp_dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let (ok, _, _) = run_git(&repo_dir, &["init", "-b", "main"]);
    if !ok {
        // Fallback for older git versions
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["checkout", "-b", "main"]);
    }
    run_git(&repo_dir, &["config", "user.name", "Fleet Test Bot"]);
    run_git(&repo_dir, &["config", "user.email", "bot@example.com"]);

    let src_dir = repo_dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        repo_dir.join("Cargo.toml"),
        r#"[package]
name = "fleet-app"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"fn main() {
    println!("fleet app main");
}
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("schema.rs"),
        r#"pub struct User {
    pub id: u64,
    pub name: String,
}
"#,
    )
    .unwrap();

    run_git(&repo_dir, &["add", "."]);
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-m", "initial commit"]);
    assert!(ok, "initial commit failed: {err}");

    // Branch 1: feat/auth-base
    let (ok, _, err) = run_git(&repo_dir, &["checkout", "-b", "feat/auth-base"]);
    assert!(ok, "checkout feat/auth-base failed: {err}");
    fs::write(
        src_dir.join("schema.rs"),
        r#"pub struct User {
    pub id: u64,
    pub name: String,
    pub auth_token: String,
}
"#,
    )
    .unwrap();
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-am", "auth-base: add auth_token"]);
    assert!(ok, "commit feat/auth-base failed: {err}");

    // Branch 2: feat/auth-jwt
    let (ok, _, err) = run_git(&repo_dir, &["checkout", "-b", "feat/auth-jwt"]);
    assert!(ok, "checkout feat/auth-jwt failed: {err}");
    fs::write(
        src_dir.join("auth.rs"),
        r#"pub fn verify_jwt(tok: &str) -> bool {
    !tok.is_empty()
}
"#,
    )
    .unwrap();
    run_git(&repo_dir, &["add", "src/auth.rs"]);
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-m", "auth-jwt: add verify_jwt"]);
    assert!(ok, "commit feat/auth-jwt failed: {err}");

    // Branch 3: feat/db-migration (conflicting edit on src/schema.rs)
    let (ok, _, err) = run_git(&repo_dir, &["checkout", "-b", "feat/db-migration"]);
    assert!(ok, "checkout feat/db-migration failed: {err}");
    fs::write(
        src_dir.join("schema.rs"),
        r#"pub struct User {
    pub id: u64,
    pub name: String,
    pub password_hash: String,
}
"#,
    )
    .unwrap();
    let (ok, _, err) = run_git(
        &repo_dir,
        &["commit", "-am", "db-migration: add password_hash"],
    );
    assert!(ok, "commit feat/db-migration failed: {err}");

    // 2. Start live unprivileged traced instance if available
    let has_traced = perfetto_bin_path().is_some()
        && traced_bin_path().is_some()
        && traced_probes_bin_path().is_some();
    let producer_sock = env.runtime_dir.join("producer.sock");
    let consumer_sock = env.runtime_dir.join("consumer.sock");
    let mut _daemons_guard: Option<TracedDaemons> = None;

    if has_traced {
        let traced_bin = traced_bin_path().unwrap();
        let probes_bin = traced_probes_bin_path().unwrap();

        let mut traced_cmd = Command::new(&traced_bin);
        traced_cmd
            .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
            .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let traced_child = traced_cmd.spawn().expect("failed to spawn traced");

        let ready =
            TestEnv::wait_for_path(&producer_sock) && TestEnv::wait_for_path(&consumer_sock);
        assert!(ready, "traced private sockets failed to bind");

        let mut probes_cmd = Command::new(&probes_bin);
        probes_cmd
            .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
            .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let probes_child = probes_cmd.spawn().expect("failed to spawn traced_probes");

        _daemons_guard = Some(TracedDaemons {
            traced: Some(traced_child),
            probes: Some(probes_child),
        });

        // Start tacho system session
        let mut start_cmd = env.tacho_cmd();
        start_cmd
            .args(["system", "start"])
            .env("TACHO_PERFETTO", perfetto_bin_path().unwrap())
            .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
            .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock);
        let status = start_cmd.status().expect("tacho system start failed");
        assert!(status.success());
    }

    // Configure fast 200ms sampler interval so process tree slices, memory counters,
    // and worktree progress milestones are captured at high resolution during the test.
    fs::write(
        env.home_dir.join("config.toml"),
        "[sampler]\ninterval_ms = 200\n",
    )
    .unwrap();

    // Ensure daemon is running
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    // 3. Declare tasks and allowed paths (enabling OFF_BRIEF detector)
    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "auth-task",
            "--agent",
            "worker-auth",
            "--paths",
            "src/schema.rs,src/auth.rs",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task auth-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "db-task",
            "--agent",
            "worker-db",
            "--paths",
            "src/schema.rs",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task db-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "api-task",
            "--agent",
            "worker-api",
            "--paths",
            "src/api.rs",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task api-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "rogue-task",
            "--agent",
            "worker-rogue",
            "--paths",
            "src/main.rs",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task rogue-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let repo_str = repo_dir.to_str().unwrap();

    // 4. Start sessions for 6 concurrent agents
    // (supervisor, worker-auth, worker-db, worker-api, worker-rogue, ci-train)
    // Spawn real process trees for workers and ci-train so /proc + cgroup process tree slices
    // (`procs/slot:*`), `proc_count`, `cpu`, and `rss` tracks are populated for every active
    // worker in the example trace.
    let spawn_worker_tree = |label: &str, tool_cmd: &str| -> Child {
        let script = format!(
            // A long-lived parent with a long-lived tool child (no pacing
            // sleeps): the tree exists until the test kills the parent, and
            // PR_SET_PDEATHSIG takes the child down with it.
            "import ctypes, subprocess; subprocess.run(['bash', '-c', 'exec -a {} sleep 600'], preexec_fn=lambda: ctypes.CDLL(None).prctl(1, 9))",
            tool_cmd
        );
        Command::new("python3")
            .args(["-c", &script])
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {label} process tree: {e}"))
    };
    let auth_proc = spawn_worker_tree("worker-auth", "cargo-check");
    let db_proc = spawn_worker_tree("worker-db", "sqlx-migrate");
    let api_proc = spawn_worker_tree("worker-api", "rustc");
    let rogue_proc = spawn_worker_tree("worker-rogue", "grep");
    let ci_proc = spawn_worker_tree("ci-train", "cargo-nextest");

    let agents = [
        (
            "supervisor",
            "sess-supervisor",
            Some("orchestrator"),
            Some(0),
        ),
        (
            "worker-auth",
            "sess-worker-auth",
            Some("worker"),
            Some(auth_proc.id()),
        ),
        (
            "worker-db",
            "sess-worker-db",
            Some("worker"),
            Some(db_proc.id()),
        ),
        (
            "worker-api",
            "sess-worker-api",
            Some("worker"),
            Some(api_proc.id()),
        ),
        (
            "worker-rogue",
            "sess-worker-rogue",
            Some("worker"),
            Some(rogue_proc.id()),
        ),
        (
            "ci-train",
            "sess-ci-train",
            Some("reviewer"),
            Some(ci_proc.id()),
        ),
    ];
    for (agent, sess, role, pid_opt) in &agents {
        let mut start_payload = serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": sess,
            "agent": agent,
            "cwd": repo_str,
            "model": "claude-3-5-sonnet",
            "role": role,
        });
        if let Some(pid) = pid_opt {
            start_payload["pid"] = serde_json::json!(pid);
        }
        send_claude_hook(&env, agent, start_payload);
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": sess,
                "prompt": format!("Initial prompt for {agent}"),
            }),
        );
    }
    // Allow initial sampler tick to record baseline process tree and worktree state
    env.flush();

    // Transition core tasks to InProgress now that workers have started
    for t_slug in ["auth-task", "db-task", "api-task", "rogue-task"] {
        let out = env
            .tacho_cmd()
            .args(["emit", "state", &format!("task:{t_slug}"), "InProgress"])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    // 5. worker-auth edits src/schema.rs and executes overlapping tools (populating lane 1)
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-edit",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/schema.rs",
            },
        }),
    );
    // Overlapping second tool call while tool-auth-edit is still open -> allocated to lane 1
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-parallel-read",
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/auth.rs",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-parallel-read",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-edit",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-build",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build -p fleet-app",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-build",
            "tool_response": { "exit_code": 0 },
        }),
    );

    // 6. Launch supervisor watch process: `tacho watch --until stalled`
    let watch_child = env
        .tacho_cmd()
        .args(["watch", "--until", "stalled"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tacho watch");

    // 7. worker-db runs `git rebase feat/auth-base` which fails with conflict,
    // and loops 4x with identical command + failure -> triggers LOOP and FAILING!
    for i in 1..=4 {
        send_claude_hook(
            &env,
            "worker-db",
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": "sess-worker-db",
                "tool_use_id": format!("tool-db-rebase-{i}"),
                "tool_name": "Bash",
                "tool_input": {
                    "command": "git rebase feat/auth-base",
                },
            }),
        );
        send_claude_hook(
            &env,
            "worker-db",
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": "sess-worker-db",
                "tool_use_id": format!("tool-db-rebase-{i}"),
                "tool_response": {
                    "exit_code": 1,
                    "error": "CONFLICT (content): Merge conflict in src/schema.rs",
                },
            }),
        );
    }

    // 8. supervisor watch catches worker-db's stall and terminates cleanly
    // Drain its pipes on a thread while the pidfd tells us when it exits.
    let watch_fd = tacho_e2e::PidFd::open(watch_child.id());
    let watch_reader = std::thread::spawn(move || watch_child.wait_with_output().unwrap());
    let watch_exited = watch_fd.is_none_or(|fd| fd.wait_exit(Some(tacho_e2e::SUITE_SAFETY_CAP)));
    assert!(
        watch_exited,
        "tacho watch --until stalled did not terminate on worker-db stall"
    );
    let watch_output = watch_reader.join().unwrap();
    assert!(watch_output.status.success());

    // Verify status output reports worker-db as stalled
    let status_out = env
        .tacho_cmd()
        .args(["status"])
        .output()
        .expect("tacho status failed");
    assert!(status_out.status.success());
    let status_txt = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_txt.contains("worker-db")
            && (status_txt.contains("Stalled") || status_txt.contains("LOOP")),
        "worker-db should be Stalled or have LOOP flag in status: {status_txt}"
    );

    // Supervisor inspects report and queries
    let report_out = env
        .tacho_cmd()
        .args(["report", "worker-db"])
        .output()
        .expect("tacho report failed");
    assert!(report_out.status.success());
    let report_str = String::from_utf8_lossy(&report_out.stdout);
    assert!(report_str.contains("worker-db"));

    let edits_out = env
        .tacho_cmd()
        .args(["query", "edits-by-file"])
        .output()
        .expect("tacho query edits-by-file failed");
    assert!(edits_out.status.success());

    let fail_out = env
        .tacho_cmd()
        .args(["query", "failing-commands"])
        .output()
        .expect("tacho query failing-commands failed");
    assert!(fail_out.status.success());

    // 9. worker-db ends its stalled turn, supervisor messages worker-db to unblock it,
    // and worker-db starts a new turn (linked via FLOW_MESSAGE_DELIVERY) to abort rebase, edit schema, and test.
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "TurnEnd",
            "session_id": "sess-worker-db",
            "input_tokens": 3_100,
            "output_tokens": 950,
            "cached_tokens": 1_200,
            "cost_micros": 14_500,
        }),
    );
    let msg_out = env
        .tacho_cmd()
        .args([
            "emit",
            "message",
            "--from",
            "supervisor",
            "--to",
            "worker-db",
            "--bytes",
            "72",
            "--id",
            "msg-unblock-db",
        ])
        .output()
        .unwrap();
    assert!(
        msg_out.status.success(),
        "emit message failed: {}",
        String::from_utf8_lossy(&msg_out.stderr)
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-worker-db",
            "prompt": "Supervisor unblocked db-task: abort rebase and update schema",
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-abort",
            "tool_name": "Bash",
            "tool_input": {
                "command": "git rebase --abort",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-abort",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-edit",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/schema.rs",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-edit",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-test",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo test",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-db",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-db",
            "tool_use_id": "tool-db-test",
            "tool_response": { "exit_code": 0 },
        }),
    );

    // 10. Multi-way build contention + Subagent Wait flow:
    // worker-auth, worker-api, and ci-train execute overlapping Build/Test commands
    send_claude_hook(
        &env,
        "worker-api",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-api",
            "tool_use_id": "tool-api-build",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build --bin api",
            },
        }),
    );
    send_claude_hook(
        &env,
        "ci-train",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-ci-train",
            "tool_use_id": "tool-ci-test",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo test --workspace",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-test",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo test -p auth",
            },
        }),
    );
    // worker-auth waits on Subagent ci-train
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "Wait",
            "session_id": "sess-worker-auth",
            "target": "ci-train",
            "on": "subagent",
        }),
    );

    send_claude_hook(
        &env,
        "worker-api",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-api",
            "tool_use_id": "tool-api-build",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "ci-train",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-ci-train",
            "tool_use_id": "tool-ci-test",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "WaitEnd",
            "session_id": "sess-worker-auth",
        }),
    );
    send_claude_hook(
        &env,
        "worker-auth",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-auth",
            "tool_use_id": "tool-auth-test",
            "tool_response": { "exit_code": 0 },
        }),
    );

    // 11. Stacked PR Workflow: 6 stacked PRs (pr-01 .. pr-06)
    // with TaskQueued, TaskState, Review findings, git -C rebases, and gh pr commands
    for pr_idx in 1..=6 {
        let pr_slug = format!("pr-{:02}", pr_idx);
        let feat_file = format!("src/feature_{:02}.rs", pr_idx);

        let agent = if pr_idx % 3 == 0 {
            "worker-auth"
        } else if pr_idx % 3 == 1 {
            "worker-api"
        } else {
            "worker-db"
        };

        // Declare task
        let out = env
            .tacho_cmd()
            .args([
                "emit", "task", &pr_slug, "--agent", agent, "--paths", &feat_file,
            ])
            .output()
            .unwrap();
        assert!(out.status.success());

        // Queued -> InProgress
        let out = env
            .tacho_cmd()
            .args(["emit", "state", &format!("task:{pr_slug}"), "InProgress"])
            .output()
            .unwrap();
        assert!(out.status.success());

        let sess = format!("sess-{agent}");
        let pr_branch = format!("feature/pr-{:02}", pr_idx);

        // Real git branch and checkout with `git -C <repo>`
        let (ok, _, _) = run_git(&repo_dir, &["checkout", "-b", &pr_branch]);
        if !ok {
            let _ = run_git(&repo_dir, &["checkout", &pr_branch]);
        }

        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-git-checkout-{pr_idx}"),
                "tool_name": "Bash",
                "tool_input": {
                    "command": format!("git -C {} checkout -b {}", repo_dir.display(), pr_branch),
                },
            }),
        );
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-git-checkout-{pr_idx}"),
                "tool_response": { "exit_code": 0 },
            }),
        );

        // Edit feature file
        fs::write(
            repo_dir.join(&feat_file),
            format!("pub fn feature_{:02}() -> usize {{ {} }}\n", pr_idx, pr_idx),
        )
        .unwrap();

        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-edit-{pr_idx}"),
                "tool_name": "Edit",
                "tool_input": {
                    "file_path": feat_file,
                },
            }),
        );
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-edit-{pr_idx}"),
                "tool_response": { "exit_code": 0 },
            }),
        );

        // Git commit
        let _ = run_git(&repo_dir, &["add", &feat_file]);
        let _ = run_git(
            &repo_dir,
            &["commit", "-m", &format!("feat: implement {pr_slug}")],
        );
        // Deterministically run a sampler tick so WorktreeCommit is recorded for this PR
        env.flush();

        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-git-commit-{pr_idx}"),
                "tool_name": "Bash",
                "tool_input": {
                    "command": format!("git -C {} commit -m 'feat: implement {}'", repo_dir.display(), pr_slug),
                },
            }),
        );
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-git-commit-{pr_idx}"),
                "tool_response": { "exit_code": 0 },
            }),
        );

        // gh pr create
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-gh-create-{pr_idx}"),
                "tool_name": "Bash",
                "tool_input": {
                    "command": format!("gh pr create --title 'PR {}' --body 'Stacked PR implementation'", pr_idx),
                },
            }),
        );
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-gh-create-{pr_idx}"),
                "tool_response": { "exit_code": 0 },
            }),
        );

        // gh pr checks
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-gh-checks-{pr_idx}"),
                "tool_name": "Bash",
                "tool_input": {
                    "command": "gh pr checks",
                },
            }),
        );
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": sess,
                "tool_use_id": format!("tool-gh-checks-{pr_idx}"),
                "tool_response": { "exit_code": 0 },
            }),
        );

        // Stack sync / rebase
        if pr_idx % 2 == 0 {
            send_claude_hook(
                &env,
                agent,
                serde_json::json!({
                    "hook_event_name": "PreToolUse",
                    "session_id": sess,
                    "tool_use_id": format!("tool-gh-sync-{pr_idx}"),
                    "tool_name": "Bash",
                    "tool_input": {
                        "command": "gh stack sync",
                    },
                }),
            );
            send_claude_hook(
                &env,
                agent,
                serde_json::json!({
                    "hook_event_name": "PostToolUse",
                    "session_id": sess,
                    "tool_use_id": format!("tool-gh-sync-{pr_idx}"),
                    "tool_response": { "exit_code": 0 },
                }),
            );
        }

        // Review findings
        if pr_idx == 2 || pr_idx == 5 {
            let _ = env
                .tacho_cmd()
                .args(["emit", "state", &format!("task:{pr_slug}"), "Review"])
                .output();
            let _ = env
                .tacho_cmd()
                .args([
                    "emit",
                    "finding",
                    &pr_slug,
                    "--severity",
                    "minor",
                    "--path",
                    &feat_file,
                    "--line",
                    "25",
                    "--title",
                    "Style and formatting review",
                ])
                .output();
        }
        if pr_idx == 4 {
            let _ = env
                .tacho_cmd()
                .args(["emit", "state", &format!("task:{pr_slug}"), "Review"])
                .output();
            let _ = env
                .tacho_cmd()
                .args([
                    "emit",
                    "finding",
                    &pr_slug,
                    "--severity",
                    "major",
                    "--path",
                    &feat_file,
                    "--line",
                    "80",
                    "--title",
                    "Error handling validation",
                ])
                .output();
        }

        // Complete PR
        let out = env
            .tacho_cmd()
            .args(["emit", "state", &format!("task:{pr_slug}"), "Done"])
            .output()
            .unwrap();
        assert!(out.status.success());

        // Complete a turn with token/cost metrics and start next turn after each PR batch
        if pr_idx % 2 == 0 && pr_idx < 6 {
            for (w_agent, w_sess, base_tok) in [
                ("worker-auth", "sess-worker-auth", 4_200u64),
                ("worker-api", "sess-worker-api", 3_800u64),
                ("worker-db", "sess-worker-db", 4_500u64),
            ] {
                send_claude_hook(
                    &env,
                    w_agent,
                    serde_json::json!({
                        "hook_event_name": "TurnEnd",
                        "session_id": w_sess,
                        "input_tokens": base_tok + (pr_idx as u64) * 450,
                        "output_tokens": 1_200 + (pr_idx as u64) * 240,
                        "cached_tokens": 2_000 + (pr_idx as u64) * 300,
                        "cost_micros": 18_000 + (pr_idx as u64) * 2_700,
                    }),
                );
                send_claude_hook(
                    &env,
                    w_agent,
                    serde_json::json!({
                        "hook_event_name": "UserPromptSubmit",
                        "session_id": w_sess,
                        "prompt": format!("Continue stacked PR batch after {pr_slug}"),
                    }),
                );
            }
        }
    }

    // Complete core feature tasks
    for t_slug in ["auth-task", "db-task", "api-task"] {
        let out = env
            .tacho_cmd()
            .args(["emit", "state", &format!("task:{t_slug}"), "Done"])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    // 12. Killed Worker + Takeover Scenario:
    // worker-flaky starts a real process for pr-07 with an open test,
    // process is SIGKILL'd mid-test, tacho-daemon reaps worker-flaky,
    // and worker-rescue takes over the branch, fixes the test, and completes the PR!
    let flaky_slug = "pr-07";
    let flaky_file = "src/feature_07.rs";

    // Declare task
    let out = env
        .tacho_cmd()
        .args(["emit", "task", flaky_slug, "--paths", flaky_file])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = env
        .tacho_cmd()
        .args(["emit", "state", &format!("task:{flaky_slug}"), "InProgress"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Spawn a real child process tree that represents worker-flaky
    let mut flaky_proc = Command::new("bash")
        .args(["-c", "exec -a cargo-test-hung sleep 60"])
        .spawn()
        .expect("failed to spawn worker-flaky process");
    let flaky_pid = flaky_proc.id();

    send_claude_hook(
        &env,
        "worker-flaky",
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-worker-flaky",
            "agent": "worker-flaky",
            "pid": flaky_pid,
            "task": flaky_slug,
            "cwd": repo_str,
            "model": "claude-3-5-sonnet",
        }),
    );
    send_claude_hook(
        &env,
        "worker-flaky",
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-worker-flaky",
            "prompt": "Implement pr-07 and run test suite",
        }),
    );

    // worker-flaky creates branch and starts hung test
    let (ok, _, _) = run_git(&repo_dir, &["checkout", "-b", "feature/pr-07"]);
    if !ok {
        let _ = run_git(&repo_dir, &["checkout", "feature/pr-07"]);
    }
    // Also check out an earlier commit and return to feature/pr-07 so HeadMoved is recorded
    let _ = run_git(&repo_dir, &["checkout", "main"]);
    env.flush();
    let _ = run_git(&repo_dir, &["checkout", "feature/pr-07"]);
    env.flush();

    send_claude_hook(
        &env,
        "worker-flaky",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-flaky",
            "tool_use_id": "tool-flaky-test",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo test --test hung_test",
            },
        }),
    );

    // Let sampler record worker-flaky's live process tree before SIGKILL
    env.flush();

    // Kill the worker-flaky process with SIGKILL (simulating process crash / OOM killer)
    let _ = flaky_proc.kill();
    let _ = flaky_proc.wait();

    // Verify worker-flaky process is gone
    assert!(!TestEnv::is_pid_alive(flaky_pid));

    // Wait for sampler / tick
    env.flush();

    // worker-rescue takes over the task and finishes the branch
    let rescue_proc = spawn_worker_tree("worker-rescue", "cargo-test");
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-worker-rescue",
            "agent": "worker-rescue",
            "pid": rescue_proc.id(),
            "task": flaky_slug,
            "cwd": repo_str,
            "model": "claude-3-5-sonnet",
        }),
    );
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-worker-rescue",
            "prompt": "Take over pr-07 from crashed worker-flaky and complete the PR",
        }),
    );

    fs::write(
        repo_dir.join(flaky_file),
        "pub fn feature_07() -> usize { 7 }\n",
    )
    .unwrap();

    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-edit",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": flaky_file,
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-edit",
            "tool_response": { "exit_code": 0 },
        }),
    );

    let _ = run_git(&repo_dir, &["add", flaky_file]);
    let _ = run_git(
        &repo_dir,
        &["commit", "-m", "feat: worker-rescue finishes pr-07"],
    );
    env.flush();

    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-test",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo test",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-test",
            "tool_response": { "exit_code": 0 },
        }),
    );

    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-gh-create",
            "tool_name": "Bash",
            "tool_input": {
                "command": "gh pr create --title 'PR 07' --body 'Rescued and completed PR 07'",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-rescue",
            "tool_use_id": "tool-rescue-gh-create",
            "tool_response": { "exit_code": 0 },
        }),
    );

    let out = env
        .tacho_cmd()
        .args(["emit", "state", &format!("task:{flaky_slug}"), "Done"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // 13. worker-rogue triggers OFF_BRIEF and BURN stalls:
    // Edits prod/secrets.yaml (outside declared task paths `src/main.rs`)
    send_claude_hook(
        &env,
        "worker-rogue",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-worker-rogue",
            "tool_use_id": "tool-rogue-edit",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "prod/secrets.yaml",
            },
        }),
    );
    send_claude_hook(
        &env,
        "worker-rogue",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-worker-rogue",
            "tool_use_id": "tool-rogue-edit",
            "tool_response": { "exit_code": 0 },
        }),
    );
    // Burns > 200k tokens in turn with 0 progress edits
    send_claude_hook(
        &env,
        "worker-rogue",
        serde_json::json!({
            "hook_event_name": "TurnEnd",
            "session_id": "sess-worker-rogue",
            "input_tokens": 10_000,
            "output_tokens": 250_000,
            "cost_micros": 500_000,
        }),
    );
    let _ = env
        .tacho_cmd()
        .args(["emit", "state", "task:rogue-task", "Failed"])
        .output();

    // Verify worker-rogue is flagged in status
    let status_out = env
        .tacho_cmd()
        .args(["status"])
        .output()
        .expect("tacho status failed");
    assert!(status_out.status.success());
    let status_txt = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_txt.contains("worker-rogue"),
        "worker-rogue not found in tacho status: {status_txt}"
    );

    // Close supervisor's initial turn before daemon crash so Turn 1 and Turn 2 are cleanly paired
    send_claude_hook(
        &env,
        "supervisor",
        serde_json::json!({
            "hook_event_name": "TurnEnd",
            "session_id": "sess-supervisor",
            "input_tokens": 1_800,
            "output_tokens": 450,
            "cached_tokens": 900,
            "cost_micros": 8_500,
        }),
    );

    // 12. Mid-scenario chaos: SIGKILL daemon, verify tacho trace fallback, auto-spawn recovery
    let orig_pid = env.daemon_pid().expect("daemon PID should be present");
    let daemon_fd = tacho_e2e::PidFd::open(orig_pid).expect("daemon must be alive");
    assert!(
        daemon_fd.signal(libc::SIGKILL),
        "failed to send SIGKILL to daemon"
    );
    let killed = daemon_fd.wait_exit(Some(tacho_e2e::SUITE_SAFETY_CAP));
    assert!(killed, "daemon did not die after SIGKILL");

    // Verify tacho trace succeeds directly from SQLite read-only DB fallback
    let ro_trace_path = env.home_dir.join("trace/ro_fallback.perfetto-trace");
    let ro_out = env
        .tacho_cmd()
        .args(["trace", "-o", ro_trace_path.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(ro_out.status.success(), "tacho trace fallback failed");
    let ro_stderr = String::from_utf8_lossy(&ro_out.stderr);
    assert!(
        ro_stderr.contains("opening events.sqlite read-only") || ro_stderr.contains("read-only"),
        "expected read-only DB fallback message on stderr: {ro_stderr}"
    );
    assert!(ro_trace_path.exists());
    let ro_oracle = env
        .run_oracle(&ro_trace_path)
        .expect("oracle failed on fallback trace");
    assert!(
        ro_oracle.error_stats.is_empty(),
        "fallback trace oracle errors: {:?}",
        ro_oracle.error_stats
    );

    // Next hook auto-spawns daemon detached
    send_claude_hook(
        &env,
        "supervisor",
        serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-supervisor",
            "prompt": "Supervisor resumed monitoring after daemon crash",
        }),
    );
    assert!(env.wait_for_api_sock());
    let new_pid = env.daemon_pid().expect("new daemon PID should exist");
    assert_ne!(orig_pid, new_pid, "new daemon should have distinct PID");

    // Complete final turns on active workers so Turn slices and token/cost counters are finalized
    for (w_agent, w_sess, in_tok, out_tok, cached_tok, cost_us) in [
        (
            "worker-auth",
            "sess-worker-auth",
            6_400u64,
            2_100u64,
            3_500u64,
            32_000u64,
        ),
        (
            "worker-api",
            "sess-worker-api",
            5_900u64,
            1_950u64,
            3_100u64,
            29_500u64,
        ),
        (
            "worker-db",
            "sess-worker-db",
            6_800u64,
            2_400u64,
            3_200u64,
            35_000u64,
        ),
        (
            "worker-rescue",
            "sess-worker-rescue",
            4_100u64,
            1_500u64,
            1_800u64,
            21_000u64,
        ),
        (
            "ci-train",
            "sess-ci-train",
            3_200u64,
            900u64,
            1_500u64,
            14_000u64,
        ),
        (
            "supervisor",
            "sess-supervisor",
            2_500u64,
            650u64,
            1_200u64,
            11_000u64,
        ),
    ] {
        send_claude_hook(
            &env,
            w_agent,
            serde_json::json!({
                "hook_event_name": "TurnEnd",
                "session_id": w_sess,
                "input_tokens": in_tok,
                "output_tokens": out_tok,
                "cached_tokens": cached_tok,
                "cost_micros": cost_us,
            }),
        );
    }

    // Close completed worker runs so Run and procs slices span their full lifetime in Perfetto UI
    for (w_agent, w_sess) in [
        ("worker-auth", "sess-worker-auth"),
        ("worker-api", "sess-worker-api"),
        ("worker-db", "sess-worker-db"),
        ("worker-rescue", "sess-worker-rescue"),
        ("ci-train", "sess-ci-train"),
        ("worker-rogue", "sess-worker-rogue"),
        ("supervisor", "sess-supervisor"),
    ] {
        send_claude_hook(
            &env,
            w_agent,
            serde_json::json!({
                "hook_event_name": "SessionEnd",
                "session_id": w_sess,
                "reason": "completed",
            }),
        );
    }

    // 13. Final trace capture: write to /tmp/tacho-realistic-fleet.perfetto-trace
    let fleet_trace_path = PathBuf::from("/tmp/tacho-realistic-fleet.perfetto-trace");
    let mut final_trace_cmd = env.tacho_cmd();
    if has_traced {
        final_trace_cmd
            .args([
                "trace",
                "--system",
                "-o",
                fleet_trace_path.to_str().unwrap(),
            ])
            .env("TACHO_PERFETTO", perfetto_bin_path().unwrap())
            .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
            .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock);
    } else {
        final_trace_cmd.args(["trace", "-o", fleet_trace_path.to_str().unwrap()]);
    }

    let trace_res = final_trace_cmd.output().expect("final trace export failed");
    assert!(
        trace_res.status.success(),
        "final trace capture failed: {}",
        String::from_utf8_lossy(&trace_res.stderr)
    );
    assert!(fleet_trace_path.exists(), "fleet trace file does not exist");

    // 14. Oracle + DecodedTrace verification
    let oracle = env
        .run_oracle(&fleet_trace_path)
        .expect("oracle verification failed");
    assert!(
        oracle.error_stats.is_empty(),
        "Fleet trace oracle reported error stats: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0, "expected slice rows");
    assert!(oracle.track_count > 0, "expected track rows");

    // Verify zero unclosed slices in Perfetto SQLite slice table
    let unclosed_rows = env
        .query_trace(
            &fleet_trace_path,
            "SELECT count(*) FROM slice WHERE dur < 0;",
        )
        .expect("query unclosed slices failed");
    let unclosed_sql: i64 = unclosed_rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse().ok())
        .unwrap_or(-1);
    assert_eq!(
        unclosed_sql, 0,
        "Expected 0 unclosed slices (dur < 0) in fleet trace, got {unclosed_sql}"
    );

    // Deep structural assertions on decoded Perfetto protobuf trace
    let decoded = DecodedTrace::from_file(&fleet_trace_path).unwrap();

    // (a) Agent `turns`, `lane:N` (`lane 1`), `stalls`, `worktree` commits/head moves,
    //     `procs/slot N`, and step-function counters (`cpu`, `rss`, `commits`, `files_edited`)
    assert!(
        !decoded
            .slices_on("tacho/agents/worker-auth/run 1/turns")
            .is_empty(),
        "worker-auth must have turn slices on turns track"
    );
    let lane1_slices = decoded.slices_on("tacho/agents/worker-auth/run 1/lane 1");
    assert!(
        !lane1_slices.is_empty(),
        "worker-auth overlapping tool call must be projected onto lane 1 (`lane:1`)"
    );
    let db_stalls = decoded.slices_on("tacho/agents/worker-db/stalls");
    assert!(
        db_stalls.iter().any(|s| s.name.starts_with("Stall [LOOP]")),
        "worker-db stalls track must contain Stall [LOOP] slice, got: {db_stalls:?}"
    );
    let rogue_stalls = decoded.slices_on("tacho/agents/worker-rogue/stalls");
    assert!(
        rogue_stalls
            .iter()
            .any(|s| s.name.starts_with("Stall [OFF_BRIEF]")),
        "worker-rogue stalls track must contain Stall [OFF_BRIEF] slice, got: {rogue_stalls:?}"
    );
    assert!(
        rogue_stalls
            .iter()
            .any(|s| s.name.starts_with("Stall [BURN]")),
        "worker-rogue stalls track must contain Stall [BURN] slice, got: {rogue_stalls:?}"
    );

    // Worktree commits and head moves across active agents
    let all_slices = decoded.all_tacho_slices();
    assert!(
        all_slices
            .iter()
            .any(|s| s.track_path.ends_with("/worktree") && s.name.starts_with("WorktreeCommit:")),
        "Fleet trace must contain WorktreeCommit slices on agent worktree tracks"
    );
    assert!(
        all_slices
            .iter()
            .any(|s| s.track_path.ends_with("/worktree") && s.name.starts_with("HeadMoved:")),
        "Fleet trace must contain HeadMoved slices on agent worktree tracks"
    );

    // Process tree slots (`procs/slot N`)
    assert!(
        all_slices
            .iter()
            .any(|s| s.track_path.contains("/procs/slot ")),
        "Fleet trace must contain process tree slices on procs/slot N tracks"
    );

    // Step-function counters (`cpu`, `rss`, `commits`, `files_edited`)
    for suffix in ["/cpu", "/rss", "/commits", "/files_edited"] {
        let has_counter = decoded.tracks.keys().any(|&u| {
            let p = decoded.path(u);
            p.starts_with("tacho/agents/")
                && p.ends_with(suffix)
                && !decoded.counter_samples_on(&p).is_empty()
        });
        assert!(
            has_counter,
            "Expected non-empty step-function counter samples on an agent *{suffix} track"
        );
    }

    // (c) `FLOW_MESSAGE_DELIVERY` flow events connecting inter-agent messages to recipient turns
    let msg_links = decoded.message_delivery_links();
    assert!(
        msg_links.iter().any(|(from, to, _)| {
            from == "tacho/messages::Message: supervisor -> worker-db"
                && to.starts_with("tacho/agents/worker-db/run 1/turns::Turn 2")
        }),
        "Expected FLOW_MESSAGE_DELIVERY linking supervisor -> worker-db message to worker-db Turn 2, got: {msg_links:#?}"
    );
    decoded.assert_no_dangling_terminating_flows();

    // (d) Zero orphaned/unclosed slices at trace snapshot boundary
    let unclosed_tacho = decoded.unclosed_tacho_slices();
    assert!(
        unclosed_tacho.is_empty(),
        "Expected zero unclosed tacho slices at trace snapshot boundary, found: {unclosed_tacho:#?}"
    );

    // 15. Verify all 9 PerfettoSQL queries run cleanly on /tmp/tacho-realistic-fleet.perfetto-trace
    let queries = [
        "agents-now",
        "failing-commands",
        "edits-by-file",
        "open-waits",
        "stalls",
        "contention",
        "system-during-builds",
        "time-by-class",
        "tokens-by-agent",
    ];

    let fleet_str = fleet_trace_path.to_str().unwrap();
    for q in queries {
        let q_out = env
            .tacho_cmd()
            .args(["query", q, "--trace", fleet_str])
            .output()
            .unwrap_or_else(|e| panic!("failed to run query {q}: {e}"));
        assert!(
            q_out.status.success(),
            "query {q} failed: {}",
            String::from_utf8_lossy(&q_out.stderr)
        );
        let q_stdout = String::from_utf8_lossy(&q_out.stdout);
        println!("=== QUERY: {q} ===");
        println!("{q_stdout}");
    }

    // Write example trace to $TACHO_EXAMPLE_TRACE_OUT if set, else only to the temp dir
    let example_trace_dest = if let Ok(val) = std::env::var("TACHO_EXAMPLE_TRACE_OUT") {
        PathBuf::from(val.trim())
    } else {
        env.temp_dir
            .path()
            .join("tacho-example-fleet.perfetto-trace")
    };
    let copy_res = fs::copy(&fleet_trace_path, &example_trace_dest);
    assert!(
        copy_res.is_ok(),
        "failed to copy trace to {}: {:?}",
        example_trace_dest.display(),
        copy_res.err()
    );
    println!(
        "Successfully wrote example fleet trace to {}",
        example_trace_dest.display()
    );

    // Stop system session cleanly if active
    if has_traced {
        let mut stop_cmd = env.tacho_cmd();
        stop_cmd
            .args(["system", "stop"])
            .env("TACHO_PERFETTO", perfetto_bin_path().unwrap())
            .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock);
        let _ = stop_cmd.status();
    }

    for mut p in [
        auth_proc,
        db_proc,
        api_proc,
        rogue_proc,
        rescue_proc,
        ci_proc,
    ] {
        let _ = p.kill();
        let _ = p.wait();
    }
}
