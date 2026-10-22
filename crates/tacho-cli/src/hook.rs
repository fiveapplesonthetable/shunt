//! Hook invocation handler for Claude Code, Gemini CLI, and Jetski.
//!
//! Hard constraints:
//! 1. Wall-clock budget: strictly <= 200 ms total.
//! 2. Output: prints NOTHING to stdout (stdout is intercepted by agent hook runners).
//! 3. Exit code: ALWAYS exits 0.
//! 4. Stdin limit: capped at 16 MiB.
//! 5. Absent daemon: spawns daemon detached without waiting, drops the event,
//!    and increments `$TACHO_HOME/hook-drops`.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    // 3. Map provider payload to NDJSON Input messages
    let inputs = map_hook_payload(provider, &val);
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

    // 5. Stream inputs with remaining wall budget as write timeout
    let remaining_ms = HARD_BUDGET_MS.saturating_sub(start_time.elapsed().as_millis() as u64);
    if remaining_ms <= 5 {
        record_hook_drop(&home);
        return;
    }

    let _ = send_inputs_with_timeout(stream, &inputs, Duration::from_millis(remaining_ms));
}

/// Atomically increments the drop counter in `$TACHO_HOME/hook-drops`.
pub fn record_hook_drop(home_dir: &Path) {
    let _ = crate::paths::ensure_dir_0700(home_dir);
    let drop_file = home_dir.join("hook-drops");
    let count = fs::read_to_string(&drop_file)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let _ = fs::write(&drop_file, (count + 1).to_string());
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

/// Maps hook payloads from Claude, Gemini, or Jetski into generic Input JSON objects.
fn map_hook_payload(provider: Provider, val: &serde_json::Value) -> Vec<serde_json::Value> {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let session_id = val
        .get("session_id")
        .or_else(|| val.get("sessionId"))
        .or_else(|| val.get("conversationId"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let provider_str = match provider {
        Provider::Claude => "claude",
        Provider::Gemini => "gemini",
        Provider::Jetski => "jetski",
    };

    let session_key = serde_json::json!({
        "provider": provider_str,
        "session_id": session_id,
    });

    let hook_event = val
        .get("hook_event_name")
        .or_else(|| val.get("event"))
        .or_else(|| val.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut result = Vec::new();

    match hook_event {
        "SessionStart" => {
            let agent_id = std::env::var("TACHO_AGENT")
                .ok()
                .or_else(|| val.get("agent").and_then(|v| v.as_str()).map(String::from));
            let cwd_str = val.get("cwd").and_then(|v| v.as_str());
            let cwd_path = cwd_str.map(Path::new);
            let git_tree = resolve_git_tree(cwd_path);
            result.push(serde_json::json!({
                "type": "run_start",
                "source_ts_ns": now_ns,
                "provider": provider_str,
                "session_id": session_id,
                "agent": agent_id,
                "pid": val.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32),
                "task": val.get("task").and_then(|v| v.as_str()),
                "cwd": cwd_str,
                "git_tree": git_tree,
                "model": val.get("model").or_else(|| val.get("modelName")).and_then(|v| v.as_str()),
            }));
        }
        "SessionEnd" | "Stop" => {
            result.push(serde_json::json!({
                "type": "run_end",
                "source_ts_ns": now_ns,
                "session": session_key,
                "reason": val.get("reason").or_else(|| val.get("stop_reason")).and_then(|v| v.as_str()),
            }));
        }
        "UserPromptSubmit" => {
            let prompt = val.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            result.push(serde_json::json!({
                "type": "prompt",
                "source_ts_ns": now_ns,
                "session": session_key.clone(),
                "text_len": prompt.len(),
            }));
            result.push(serde_json::json!({
                "type": "turn_start",
                "source_ts_ns": now_ns,
                "session": session_key,
            }));
        }
        "PreToolUse" | "BeforeTool" => {
            let tool_name = val
                .get("tool_name")
                .or_else(|| val.get("toolCall").and_then(|tc| tc.get("name")))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_id_owned;
            let tool_id = if let Some(id) = val.get("tool_use_id").and_then(|v| v.as_str()) {
                id
            } else if let Some(step) = val.get("stepIdx") {
                tool_id_owned = step.to_string();
                &tool_id_owned
            } else {
                "0"
            };

            let tool_in = val.get("tool_input").or_else(|| val.get("input"));
            let input = if let Some(cmd) = tool_in
                .and_then(|ti| ti.get("command").or_else(|| ti.get("cmd")))
                .and_then(|v| v.as_str())
            {
                let cwd_str = val.get("cwd").and_then(|v| v.as_str());
                let cwd_path = cwd_str.map(Path::new);
                let git_tree = resolve_git_tree(cwd_path);
                serde_json::json!({
                    "tool": "shell",
                    "argv": cmd,
                    "cwd": cwd_str,
                    "git_tree": git_tree,
                })
            } else if tool_name == "Edit"
                || tool_name == "StrReplace"
                || tool_name == "Write"
                || tool_name == "write_file"
                || tool_name == "edit_file"
            {
                let path = tool_in
                    .and_then(|ti| {
                        ti.get("path")
                            .or_else(|| ti.get("file_path"))
                            .or_else(|| ti.get("filePath"))
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                serde_json::json!({
                    "tool": "file_edit",
                    "path": path,
                })
            } else if tool_name == "View" || tool_name == "Read" || tool_name == "read_file" {
                let path = tool_in
                    .and_then(|ti| {
                        ti.get("path")
                            .or_else(|| ti.get("file_path"))
                            .or_else(|| ti.get("filePath"))
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                serde_json::json!({
                    "tool": "file_read",
                    "path": path,
                })
            } else {
                serde_json::json!({
                    "tool": "other",
                    "input_bytes": tool_in.map(|ti| ti.to_string().len()).unwrap_or(0),
                })
            };

            result.push(serde_json::json!({
                "type": "tool_begin",
                "source_ts_ns": now_ns,
                "session": session_key,
                "tool_use_id": tool_id,
                "tool_name": tool_name,
                "input": input,
            }));
        }
        "PostToolUse" | "AfterTool" => {
            let tool_id_owned;
            let tool_id = if let Some(id) = val.get("tool_use_id").and_then(|v| v.as_str()) {
                id
            } else if let Some(step) = val.get("stepIdx") {
                tool_id_owned = step.to_string();
                &tool_id_owned
            } else {
                "0"
            };
            let tool_resp = val.get("tool_response").or_else(|| val.get("response"));
            let mut exit_code = tool_resp
                .and_then(|r| {
                    r.get("exit_code")
                        .or_else(|| r.get("exitCode"))
                        .or_else(|| r.get("returncode"))
                        .or_else(|| r.get("status"))
                })
                .and_then(|v| v.as_i64())
                .map(|c| c as i32);
            let mut error = exit_code.map(|c| c != 0).unwrap_or(false);
            if let Some(err) = val
                .get("error")
                .or_else(|| tool_resp.and_then(|r| r.get("error")))
                .or_else(|| tool_resp.and_then(|r| r.get("is_error")))
            {
                if !err.is_null()
                    && (err.as_bool() == Some(true)
                        || err.as_str().map(|s| !s.is_empty()).unwrap_or(false))
                {
                    error = true;
                }
            }
            if error && exit_code.is_none() {
                exit_code = Some(1);
            }

            result.push(serde_json::json!({
                "type": "tool_end",
                "source_ts_ns": now_ns,
                "session": session_key,
                "tool_use_id": tool_id,
                "error": error,
                "exit_code": exit_code,
            }));
        }
        "TurnEnd" | "AssistantMessage" => {
            let tin = val
                .get("input_tokens")
                .or_else(|| val.get("inputTokens"))
                .and_then(|v| v.as_u64());
            let tout = val
                .get("output_tokens")
                .or_else(|| val.get("outputTokens"))
                .and_then(|v| v.as_u64());
            let cost = val
                .get("cost_micros")
                .or_else(|| val.get("costMicros"))
                .and_then(|v| v.as_u64());
            result.push(serde_json::json!({
                "type": "turn_end",
                "source_ts_ns": now_ns,
                "session": session_key,
                "input_tokens": tin,
                "output_tokens": tout,
                "cost_micros": cost,
            }));
        }
        "Wait" | "WaitBegin" => {
            let on_str = val.get("on").and_then(|v| v.as_str());
            let on_val = match on_str {
                Some("subagent") => serde_json::json!("subagent"),
                Some("input") => serde_json::json!("input"),
                Some("message") => serde_json::json!("message"),
                Some("rate_limit") => serde_json::json!("rate_limit"),
                _ => serde_json::json!("other"),
            };
            result.push(serde_json::json!({
                "type": "wait",
                "source_ts_ns": now_ns,
                "session": session_key,
                "target": val.get("target").and_then(|v| v.as_str()),
                "on": on_val,
            }));
        }
        "WaitEnd" => {
            result.push(serde_json::json!({
                "type": "wait_end",
                "source_ts_ns": now_ns,
                "session": session_key,
            }));
        }
        _ => {
            // Generic hook event mapping
            result.push(serde_json::json!({
                "type": "custom",
                "source_ts_ns": now_ns,
                "track": format!("{}:{}", provider_str, session_id),
                "op": "instant",
                "name": if hook_event.is_empty() { "hook_event" } else { hook_event },
            }));
        }
    }

    result
}

/// Sends mapped input lines to `api.sock` with a write timeout.
fn send_inputs_with_timeout(
    mut stream: UnixStream,
    inputs: &[serde_json::Value],
    timeout: Duration,
) -> io::Result<()> {
    stream.set_write_timeout(Some(timeout))?;

    for input in inputs {
        let msg = serde_json::json!({ "input": input });
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        stream.write_all(&line)?;
    }
    stream.flush()?;
    Ok(())
}
