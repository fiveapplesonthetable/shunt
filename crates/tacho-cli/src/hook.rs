//! Hook invocation handler for Claude Code, Gemini CLI, and Jetski.
//!
//! Hard constraints:
//! 1. Wall-clock budget: strictly <= 200 ms total.
//! 2. Output: prints NOTHING to stdout (stdout is intercepted by agent hook runners).
//! 3. Exit code: ALWAYS exits 0.
//! 4. Stdin limit: capped at 16 MiB.
//! 5. Absent daemon: spawns daemon detached without waiting, drops the event,
//!    and increments `$TACHO_HOME/hook-drops`.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tacho_core::{Input, ToolInput};
use tacho_ingest::hooks::{claude, gemini, jetski, HookEnv};

use crate::cli::Provider;
use crate::client::DaemonClient;

const HARD_BUDGET_MS: u64 = 200;
const MAX_STDIN_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB

/// Handles the `tacho hook <provider>` execution.
pub fn execute_hook(provider: Provider) {
    let start_time = Instant::now();
    let client = DaemonClient::new();
    let home = crate::paths::resolve_home();

    // 1. Read stdin with 16 MiB cap
    let mut raw_bytes = Vec::new();
    if io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_end(&mut raw_bytes)
        .is_err()
    {
        record_hook_drop(&home);
        return;
    }

    if raw_bytes.is_empty() {
        return;
    }

    // 2. Parse JSON
    let val: serde_json::Value = match serde_json::from_slice(&raw_bytes) {
        Ok(v) => v,
        Err(_) => {
            record_hook_drop(&home);
            return;
        }
    };

    // Optional worker guardrail enforcement when opted in via TACHO_GUARDRAILS=1 or TACHO_MANAGED_WORKER=1
    let guardrails_enabled = std::env::var("TACHO_GUARDRAILS").ok().as_deref() == Some("1")
        || std::env::var("TACHO_MANAGED_WORKER").ok().as_deref() == Some("1");
    if let Ok(agent_key) = std::env::var("TACHO_AGENT") {
        if !agent_key.is_empty() {
            let inbox_file = home.join("inbox").join(format!("{agent_key}.txt"));
            if inbox_file.is_file() {
                if let Ok(msg) = std::fs::read_to_string(&inbox_file) {
                    let _ = std::fs::remove_file(&inbox_file);
                    if !msg.trim().is_empty() {
                        eprintln!("[FLEET MESSAGE for {agent_key}]: {}", msg.trim());
                    }
                }
            }
        }
    }
    if guardrails_enabled {
        let event_name = val
            .get("hook_event_name")
            .or_else(|| val.get("event"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if event_name.is_empty()
            || event_name == "PreToolUse"
            || event_name == "BeforeTool"
            || event_name == "tool_begin"
        {
            let tool_name = val
                .get("tool_name")
                .or_else(|| val.get("tool"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_shell = matches!(
                tool_name,
                "Bash" | "bash" | "run_shell_command" | "run_command" | "shell"
            );
            if is_shell {
                let cmd = val
                    .get("tool_input")
                    .and_then(|ti| {
                        ti.get("command")
                            .or_else(|| ti.get("cmd"))
                            .or_else(|| ti.get("CommandLine"))
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let run_in_bg = val
                    .get("tool_input")
                    .and_then(|ti| ti.get("run_in_background"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let under_starve = std::env::var("TACHO_STARVE").ok().as_deref() == Some("1");
                if let Err(refusal) =
                    tacho_core::decide::dispatch::guard_bash_tool(run_in_bg, cmd, under_starve)
                {
                    eprintln!("{refusal}");
                    std::process::exit(2);
                }
            }
        }
    }

    // 3. Map provider payload to typed Input messages via tacho-ingest::hooks
    let mut inputs = map_hook_payload(provider, &val);
    if inputs.is_empty() {
        return;
    }

    // Check remaining time before connecting
    let elapsed = start_time.elapsed();
    if elapsed >= Duration::from_millis(HARD_BUDGET_MS - 10) {
        record_hook_drop(&home);
        return;
    }

    // 4. Try fast connection to api.sock
    let stream = match client.try_connect_api_fast() {
        Ok(s) => s,
        Err(_) => {
            // Daemon is absent: spawn detached without waiting, drop event, record drop
            let _ = client.spawn_daemon_detached();
            record_hook_drop(&home);
            return;
        }
    };

    // Enrich git_tree only when daemon is present so absent-daemon drops stay <1ms
    enrich_git_trees(&mut inputs, &val);

    // 5. Stream inputs with remaining wall budget as write timeout
    let remaining_ms = HARD_BUDGET_MS.saturating_sub(start_time.elapsed().as_millis() as u64);
    if remaining_ms <= 5 {
        record_hook_drop(&home);
        return;
    }

    let _ = send_inputs_with_timeout(stream, &inputs, Duration::from_millis(remaining_ms));
}

/// Atomically increments the drop counter in `$TACHO_HOME/hook-drops` under `flock(LOCK_EX)`.
pub fn record_hook_drop(home_dir: &Path) {
    let _ = crate::paths::ensure_dir_0700(home_dir);
    let drop_file = home_dir.join("hook-drops");
    if let Ok(mut file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&drop_file)
    {
        unsafe {
            let _ = libc::flock(file.as_raw_fd(), libc::LOCK_EX);
        }
        let mut s = String::new();
        let _ = file.read_to_string(&mut s);
        let count = s.trim().parse::<u64>().unwrap_or(0);
        if file.seek(SeekFrom::Start(0)).is_ok() && file.set_len(0).is_ok() {
            let _ = file.write_all((count + 1).to_string().as_bytes());
            let _ = file.flush();
        }
        unsafe {
            let _ = libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn resolve_git_tree(cwd: Option<&Path>) -> Option<String> {
    let dir = cwd.unwrap_or_else(|| Path::new("."));
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD^{tree}"])
        .current_dir(dir)
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Maps hook payloads from Claude, Gemini, or Jetski into typed `Input` events via `tacho_ingest::hooks`.
fn map_hook_payload(provider: Provider, val: &serde_json::Value) -> Vec<Input> {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let agent = std::env::var("TACHO_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| val.get("agent").and_then(|v| v.as_str()).map(String::from));
    let task = std::env::var("TACHO_TASK")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| val.get("task").and_then(|v| v.as_str()).map(String::from));
    let ppid = val
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|p| p as u32)
        .or_else(|| {
            let p = unsafe { libc::getppid() };
            if p > 0 {
                Some(p as u32)
            } else {
                None
            }
        })
        .filter(|&p| p > 0);

    let env = HookEnv {
        agent,
        task,
        invoked_at_ns: now_ns,
        ppid,
    };

    match provider {
        Provider::Claude => claude::map(val, &env),
        Provider::Gemini => gemini::map(val, &env),
        Provider::Jetski => jetski::map(val, &env),
    }
}

fn enrich_git_trees(inputs: &mut [Input], val: &serde_json::Value) {
    let top_cwd = val.get("cwd").and_then(|v| v.as_str());
    for input in inputs {
        match input {
            Input::RunStart { cwd, git_tree, .. } => {
                if git_tree.is_none() {
                    *git_tree = resolve_git_tree(cwd.as_deref().or(top_cwd).map(Path::new));
                }
            }
            Input::ToolBegin {
                input: ToolInput::Shell { cwd, git_tree, .. },
                ..
            } => {
                if cwd.is_none() {
                    *cwd = top_cwd.map(String::from);
                }
                if git_tree.is_none() {
                    *git_tree = resolve_git_tree(cwd.as_deref().map(Path::new));
                }
            }
            _ => {}
        }
    }
}

/// Sends mapped input lines to `api.sock` with a write/read timeout and awaits the daemon queue ack.
fn send_inputs_with_timeout(
    mut stream: UnixStream,
    inputs: &[Input],
    timeout: Duration,
) -> io::Result<()> {
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;

    for input in inputs {
        let msg = serde_json::json!({ "input": input });
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        stream.write_all(&line)?;
    }
    stream.flush()?;

    let mut reader = io::BufReader::new(stream);
    let mut ack = String::new();
    for _ in inputs {
        ack.clear();
        if io::BufRead::read_line(&mut reader, &mut ack).unwrap_or(0) == 0 {
            break;
        }
    }
    Ok(())
}
