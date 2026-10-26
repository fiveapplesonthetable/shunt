use super::*;

fn bash_payload(cmd: &str, run_in_background: Option<bool>) -> serde_json::Value {
    let mut input = serde_json::json!({ "command": cmd });
    if let Some(bg) = run_in_background {
        input["run_in_background"] = serde_json::Value::Bool(bg);
    }
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": input,
    })
}

#[test]
fn test_burner_guard_refuses_direct_burners() {
    for cmd in [
        "yes",
        "yes >/dev/null",
        "yes > /dev/null 2>&1 &",
        "/usr/bin/yes >/dev/null",
        "stress --cpu 4",
        "/bin/stress -c 2",
        "stress-ng --cpu 4 --timeout 60s",
        "/usr/local/bin/stress-ng --matrix 0",
    ] {
        let res = burner_guard_check(cmd, false);
        assert!(res.is_err(), "expected burner refusal for `{cmd}`");
        let msg = res.unwrap_err();
        assert!(msg.contains("is a CPU burner"), "unexpected message: {msg}");
        assert!(msg.contains("tacho starve --"), "unexpected message: {msg}");
    }
}

#[test]
fn test_burner_guard_refuses_all_wrappers_and_nested_sh_c() {
    for cmd in [
        "timeout 30 yes >/dev/null",
        "timeout -k 5 30s stress --cpu 2",
        "timeout --signal=KILL 1.5m stress-ng --cpu 1",
        "env FOO=1 BAR=baz yes >/dev/null",
        "env -i PATH=/usr/bin stress --cpu 2",
        "sudo yes >/dev/null",
        "sudo -u root stress-ng --cpu 4",
        "nice yes >/dev/null",
        "nice -n 10 stress --cpu 2",
        "nice -n -5 stress-ng --cpu 2",
        "ionice -c 3 yes >/dev/null",
        "ionice -c 2 -n 7 stress --cpu 4",
        "nohup yes >/dev/null &",
        "setsid stress --cpu 2",
        "taskset -c 0-3 yes >/dev/null",
        "taskset 0x3 stress-ng --cpu 2",
        "time yes >/dev/null",
        "exec yes >/dev/null",
        "command yes >/dev/null",
        "xargs -n 1 stress",
        "sh -c 'yes >/dev/null'",
        "bash -c \"stress --cpu 4\"",
        "/bin/sh -lc 'timeout 10 nice -n 5 ionice -c 3 yes >/dev/null'",
        "env FOO=1 sudo -u nobody nice -n 10 ionice -c 3 nohup timeout 60 sh -c 'stress-ng --cpu 2'",
        "(yes >/dev/null)",
        "{ stress --cpu 2; }",
        "echo setup && timeout 10 yes >/dev/null",
        "true || stress --cpu 1",
        "for i in 1 2; do yes >/dev/null; done",
    ] {
        let res = burner_guard_check(cmd, false);
        assert!(res.is_err(), "expected wrapper burner refusal for `{cmd}`");
    }
}

#[test]
fn test_burner_guard_allows_yes_feeding_pipe_and_tacho_starve() {
    for cmd in [
        "yes | cargo install foo",
        "yes n | ./configure",
        "sh -c 'yes | head -n 5'",
        "tacho starve -- cargo test",
        "tacho starve -- stress-ng --cpu 4",
        "tacho starve -- yes >/dev/null",
        "cargo test --workspace",
        "git status",
        "echo yes",
        "grep stress Cargo.toml",
    ] {
        let res = burner_guard_check(cmd, false);
        assert!(res.is_ok(), "expected `{cmd}` to be allowed, got: {res:?}");
    }

    assert!(burner_guard_check("yes >/dev/null", true).is_ok());
    assert!(burner_guard_check("stress-ng --cpu 4", true).is_ok());
    assert!(burner_guard_check("stress --cpu 2 | cat", false).is_err());
    assert!(burner_guard_check("stress-ng --cpu 2 | cat", false).is_err());
    assert!(burner_guard_check("yes | yes", false).is_err());
}

#[test]
fn test_no_background_bash_guardrail() {
    let blocked = bash_payload("cargo test", Some(true));
    let err = check_pre_tool_use_guardrails(&blocked, false).unwrap_err();
    assert!(err.contains("Denied: this session exits when you write your final message"));
    assert!(err.contains("tacho wait"));

    let allowed_false = bash_payload("cargo test", Some(false));
    assert!(check_pre_tool_use_guardrails(&allowed_false, false).is_ok());

    let allowed_none = bash_payload("cargo test", None);
    assert!(check_pre_tool_use_guardrails(&allowed_none, false).is_ok());
}

#[test]
fn test_git_and_trailer_guardrails_in_pre_tool_use() {
    for bad_cmd in [
        "git stash",
        "git stash push -m wip",
        "git commit --no-verify -m 'wip'",
        "git push --no-verify",
        "git commit -m 'Fix bug\n\nCo-Authored-By: Bot <bot@example.com>'",
    ] {
        let payload = bash_payload(bad_cmd, None);
        assert!(
            check_pre_tool_use_guardrails(&payload, false).is_err(),
            "expected refusal for `{bad_cmd}`"
        );
    }
}

#[test]
fn test_hostile_hook_inputs_and_string_numeric_background_flags() {
    // String and numeric variants of run_in_background must also be refused
    for hostile_bg in [
        serde_json::json!({"command": "cargo test", "run_in_background": "true"}),
        serde_json::json!({"command": "cargo test", "run_in_background": "1"}),
        serde_json::json!({"command": "cargo test", "run_in_background": 1}),
        serde_json::json!({"command": "cargo test", "background": true}),
        serde_json::json!({"command": "cargo test", "async": "true"}),
    ] {
        assert!(
            no_background_bash_check(&hostile_bg).is_err(),
            "expected background refusal for {hostile_bg}"
        );
    }

    // Deeply nested wrapper CPU burner (`sudo env timeout 10 sh -c "yes > /dev/null"`)
    let nested = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {
            "command": "sudo env timeout 10 sh -c \"yes > /dev/null\""
        }
    });
    assert!(check_pre_tool_use_guardrails(&nested, false).is_err());

    // Malformed / non-object payloads must not panic
    for weird in [
        serde_json::json!(null),
        serde_json::json!(42),
        serde_json::json!("not an object"),
        serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": "string-not-object"}),
        serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": 12345}}),
    ] {
        let _ = check_pre_tool_use_guardrails(&weird, false);
    }
}

#[test]
fn test_diff_guards_in_worker_edit_and_pre_push_hooks() {
    // 1. Worker Edit hook refuses loosened limits in non-test code
    let edit_loosened = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "src/server.rs",
            "old_string": "const TIMEOUT_MS: u64 = 1000;",
            "new_string": "const TIMEOUT_MS: u64 = 5000;"
        }
    });
    let err_limit = check_pre_tool_use_guardrails(&edit_loosened, false).unwrap_err();
    assert!(err_limit.contains("loosened limit in diff"), "{err_limit}");

    // 2. Worker Write hook refuses vague waits in test files
    let write_wait = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "tests/ui.spec.ts",
            "content": "await page.waitForTimeout(500);\n"
        }
    });
    let err_wait = check_pre_tool_use_guardrails(&write_wait, false).unwrap_err();
    assert!(err_wait.contains("vague wait in test diff"), "{err_wait}");

    // 3. Worker Edit hook refuses time-based test assertions in test files
    let edit_time_test = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "crates/app/tests/perf_test.rs",
            "old_string": "assert_eq!(res, 42);",
            "new_string": "assert!(elapsed.as_millis() < 200);"
        }
    });
    let err_time = check_pre_tool_use_guardrails(&edit_time_test, false).unwrap_err();
    assert!(
        err_time.contains("time-based test assertion in diff"),
        "{err_time}"
    );

    // 4. Pre-push hook (`git push` / `tacho pr`) in a real git worktree refuses when committed diff violates guards
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    run_git(&["init", "-q", "-b", "main"]);
    run_git(&["config", "user.email", "t@example.com"]);
    run_git(&["config", "user.name", "Tester"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/limits.rs"),
        "pub const MAX_RETRIES: u32 = 3;\n",
    )
    .unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-q", "-m", "initial"]);

    run_git(&["checkout", "-q", "-b", "feat/loosen"]);
    std::fs::write(
        repo.join("src/limits.rs"),
        "pub const MAX_RETRIES: u32 = 10;\n",
    )
    .unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-q", "-m", "bump retries"]);

    let push_payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": repo.to_string_lossy(),
        "tool_input": {
            "command": "git push origin HEAD"
        }
    });
    let err_push = check_pre_tool_use_guardrails(&push_payload, false).unwrap_err();
    assert!(
        err_push.contains("Refused by pre-push diff guardrails")
            && err_push.contains("loosened limit"),
        "{err_push}"
    );
}
