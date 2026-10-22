use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tacho_e2e::{perfetto_bin_path, traced_bin_path, traced_probes_bin_path, TestEnv};

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

        let ready = env.wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            producer_sock.exists() && consumer_sock.exists()
        });
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

    // Ensure daemon is running
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    // 3. Declare tasks and allowed paths (enabling OFF_BRIEF detector)
    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "auth-task",
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
        .args(["emit", "task", "db-task", "--paths", "src/schema.rs"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task db-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env
        .tacho_cmd()
        .args(["emit", "task", "api-task", "--paths", "src/api.rs"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "emit task api-task failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env
        .tacho_cmd()
        .args(["emit", "task", "rogue-task", "--paths", "src/main.rs"])
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
    let agents = [
        ("supervisor", "sess-supervisor"),
        ("worker-auth", "sess-worker-auth"),
        ("worker-db", "sess-worker-db"),
        ("worker-api", "sess-worker-api"),
        ("worker-rogue", "sess-worker-rogue"),
        ("ci-train", "sess-ci-train"),
    ];
    for (agent, sess) in &agents {
        send_claude_hook(
            &env,
            agent,
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": sess,
                "agent": agent,
                "cwd": repo_str,
                "model": "claude-3-5-sonnet",
            }),
        );
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

    // 5. worker-auth edits src/schema.rs and builds cleanly
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
    let mut watch_child = env
        .tacho_cmd()
        .args(["watch", "--until", "stalled"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tacho watch");
    std::thread::sleep(Duration::from_millis(100));

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
        std::thread::sleep(Duration::from_millis(30));
    }

    // 8. supervisor watch catches worker-db's stall and terminates cleanly
    let watch_exited = env.wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        watch_child.try_wait().unwrap().is_some()
    });
    assert!(
        watch_exited,
        "tacho watch --until stalled did not terminate on worker-db stall"
    );
    let watch_output = watch_child.wait_with_output().unwrap();
    assert!(watch_output.status.success());

    // Verify status output reports worker-db as stalled
    let status_out = env
        .tacho_cmd()
        .args(["status", "--json"])
        .output()
        .expect("tacho status failed");
    assert!(status_out.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    let db_agent = status_json.as_array().and_then(|arr| {
        arr.iter()
            .find(|a| a.get("agent") == Some(&serde_json::json!("worker-db")))
    });
    assert!(db_agent.is_some(), "worker-db not found in tacho status");
    let db_obj = db_agent.unwrap();
    let state = db_obj.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let flags = db_obj.get("flags").and_then(|v| v.as_array());
    let has_loop = flags
        .map(|f| f.iter().any(|x| x == "LOOP"))
        .unwrap_or(false);
    assert!(
        state == "Stalled" || has_loop,
        "worker-db should be Stalled or have LOOP flag"
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

    // 9. Supervisor unblocks worker-db, which aborts rebase, edits schema, and tests
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

    // 11. Stacked PR Workflow: 30 stacked PRs (pr-01 .. pr-30)
    // with TaskQueued, TaskState, Review findings, git -C rebases, and gh pr commands
    for pr_idx in 1..=30 {
        let pr_slug = format!("pr-{:02}", pr_idx);
        let feat_file = format!("src/feature_{:02}.rs", pr_idx);

        // Declare task
        let out = env
            .tacho_cmd()
            .args(["emit", "task", &pr_slug, "--paths", &feat_file])
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

        let agent = if pr_idx % 3 == 0 {
            "worker-auth"
        } else if pr_idx % 3 == 1 {
            "worker-api"
        } else {
            "worker-db"
        };
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

        // Occasional stack sync / rebase
        if pr_idx > 1 && pr_idx % 4 == 0 {
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
        if pr_idx % 5 == 0 {
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
        if pr_idx % 7 == 0 {
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

        // Milestones
        if pr_idx == 10 {
            let _ = env
                .tacho_cmd()
                .args([
                    "emit",
                    "instant",
                    "milestones",
                    "Milestone: Foundation Stack Merged",
                ])
                .output();
        } else if pr_idx == 20 {
            let _ = env
                .tacho_cmd()
                .args([
                    "emit",
                    "instant",
                    "milestones",
                    "Milestone: Core Services Ready",
                ])
                .output();
        } else if pr_idx == 30 {
            let _ = env
                .tacho_cmd()
                .args([
                    "emit",
                    "instant",
                    "milestones",
                    "Milestone: Release v1.0.0-rc1 Stack Ready",
                ])
                .output();
        }
    }

    // 12. Killed Worker + Takeover Scenario:
    // worker-flaky starts a real process for pr-31 with an open test,
    // process is SIGKILL'd mid-test, tacho-daemon reaps worker-flaky,
    // and worker-rescue takes over the branch, fixes the test, and completes the PR!
    let flaky_slug = "pr-31";
    let flaky_file = "src/feature_31.rs";

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

    // Spawn a real child process that represents worker-flaky
    let mut flaky_proc = Command::new("sleep")
        .arg("60")
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

    // worker-flaky creates branch and starts hung test
    let (ok, _, _) = run_git(&repo_dir, &["checkout", "-b", "feature/pr-31"]);
    if !ok {
        let _ = run_git(&repo_dir, &["checkout", "feature/pr-31"]);
    }

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

    // Kill the worker-flaky process with SIGKILL (simulating process crash / OOM killer)
    let _ = flaky_proc.kill();
    let _ = flaky_proc.wait();

    // Verify worker-flaky process is gone
    assert!(!TestEnv::is_pid_alive(flaky_pid));

    // Wait for sampler / tick
    std::thread::sleep(Duration::from_millis(350));

    // worker-rescue takes over the task and finishes the branch
    send_claude_hook(
        &env,
        "worker-rescue",
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-worker-rescue",
            "agent": "worker-rescue",
            "task": flaky_slug,
            "cwd": repo_str,
            "model": "claude-3-5-sonnet",
        }),
    );

    fs::write(
        repo_dir.join(flaky_file),
        "pub fn feature_31() -> usize { 31 }\n",
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
        &["commit", "-m", "feat: worker-rescue finishes pr-31"],
    );

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
                "command": "gh pr create --title 'PR 31' --body 'Rescued and completed PR 31'",
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

    // Verify worker-rogue is flagged in status
    let status_out = env
        .tacho_cmd()
        .args(["status", "--json"])
        .output()
        .expect("tacho status failed");
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    let rogue_agent = status_json.as_array().and_then(|arr| {
        arr.iter()
            .find(|a| a.get("agent") == Some(&serde_json::json!("worker-rogue")))
    });
    assert!(
        rogue_agent.is_some(),
        "worker-rogue not found in tacho status"
    );

    // 12. Mid-scenario chaos: SIGKILL daemon, verify tacho trace fallback, auto-spawn recovery
    let orig_pid = env.daemon_pid().expect("daemon PID should be present");
    assert!(env.kill_daemon_9(), "failed to send SIGKILL to daemon");
    let killed = env.wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
        !TestEnv::is_pid_alive(orig_pid)
    });
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
    assert!(env.wait_for_api_sock(Duration::from_secs(4)));
    let new_pid = env.daemon_pid().expect("new daemon PID should exist");
    assert_ne!(orig_pid, new_pid, "new daemon should have distinct PID");

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

    // 14. Oracle verification
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

    // Copy generated trace to user Downloads directory
    let downloads_trace = PathBuf::from(
        "/usr/local/google/home/zezeozue/Downloads/tacho-example-fleet.perfetto-trace",
    );
    let copy_res = fs::copy(&fleet_trace_path, &downloads_trace);
    assert!(
        copy_res.is_ok(),
        "failed to copy trace to ~/Downloads: {:?}",
        copy_res.err()
    );
    println!(
        "Successfully wrote example fleet trace to {}",
        downloads_trace.display()
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
}
