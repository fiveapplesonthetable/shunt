use tacho_core::{Input, SessionKey, WaitOn};

use crate::hooks::{blake3_digest_8, canonical_json, classify_tool, HookDiagnostics, HookEnv};

/// Maps a Claude Code hook payload into tachograph `Input` events.
pub fn map(payload: &serde_json::Value, env: &HookEnv) -> Vec<Input> {
    let mut diags = HookDiagnostics::default();
    map_with_diagnostics(payload, env, &mut diags)
}

/// Maps a Claude Code hook payload into tachograph `Input` events, recording diagnostics.
pub fn map_with_diagnostics(
    payload: &serde_json::Value,
    env: &HookEnv,
    diags: &mut HookDiagnostics,
) -> Vec<Input> {
    let Some(obj) = payload.as_object() else {
        diags.unknown_payloads += 1;
        return Vec::new();
    };

    let Some(session_id) = obj.get("session_id").and_then(|v| v.as_str()) else {
        diags.missing_fields += 1;
        return Vec::new();
    };

    let event_name = obj
        .get("hook_event_name")
        .or_else(|| obj.get("event"))
        .or_else(|| obj.get("type"))
        .and_then(|v| v.as_str());

    match event_name {
        Some("SessionStart") => {
            let cwd = obj.get("cwd").and_then(|v| v.as_str()).map(String::from);
            let model = obj.get("model").and_then(|v| v.as_str()).map(String::from);
            let parent_session = obj
                .get("parent_session")
                .or_else(|| obj.get("parent_session_id"))
                .or_else(|| obj.get("parent_tool_use_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let git_tree = obj
                .get("git_tree")
                .or_else(|| obj.get("tree"))
                .and_then(|v| v.as_str())
                .map(String::from);

            vec![Input::RunStart {
                source_ts_ns: Some(env.invoked_at_ns),
                provider: "claude".to_string(),
                session_id: session_id.to_string(),
                agent: env.agent.clone(),
                task: env.task.clone(),
                pid: env.ppid,
                cwd,
                model,
                parent_session,
                git_tree,
                role: None,
            }]
        }
        Some("UserPromptSubmit") => {
            let prompt = obj
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            vec![
                Input::Prompt {
                    source_ts_ns: Some(env.invoked_at_ns),
                    session: SessionKey::new("claude", session_id),
                    text_len: prompt.len() as u64,
                    digest: Some(blake3_digest_8(prompt)),
                },
                Input::TurnStart {
                    source_ts_ns: Some(env.invoked_at_ns),
                    session: SessionKey::new("claude", session_id),
                    model: None,
                },
            ]
        }
        Some("PreToolUse") => {
            let tool_use_id = obj
                .get("tool_use_id")
                .or_else(|| obj.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_name = obj
                .get("tool_name")
                .or_else(|| obj.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_input = obj
                .get("tool_input")
                .or_else(|| obj.get("input"))
                .unwrap_or(&serde_json::Value::Null);

            let input = classify_tool(&tool_name, tool_input);

            vec![Input::ToolBegin {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                tool_use_id,
                tool_name,
                input,
            }]
        }
        Some("PostToolUse") => {
            let tool_use_id = obj
                .get("tool_use_id")
                .or_else(|| obj.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_resp = obj.get("tool_response").or_else(|| obj.get("response"));

            let exit_code = tool_resp
                .and_then(|r| r.get("exit_code"))
                .and_then(|v| v.as_i64())
                .map(|c| c as i32);

            let mut error = exit_code.map(|c| c != 0).unwrap_or(false);
            if let Some(r) = tool_resp {
                if let Some(err_val) = r.get("error") {
                    if let Some(b) = err_val.as_bool() {
                        if b {
                            error = true;
                        }
                    } else if let Some(s) = err_val.as_str() {
                        if !s.is_empty() {
                            error = true;
                        }
                    }
                }
                if let Some(is_err) = r.get("is_error").and_then(|v| v.as_bool()) {
                    if is_err {
                        error = true;
                    }
                }
            }

            let result_bytes = tool_resp.map(|r| {
                if let (Some(out), Some(err)) = (
                    r.get("stdout").and_then(|v| v.as_str()),
                    r.get("stderr").and_then(|v| v.as_str()),
                ) {
                    (out.len() + err.len()) as u64
                } else if let Some(content) = r.get("content").and_then(|v| v.as_str()) {
                    content.len() as u64
                } else if let Some(res) = r.get("result").and_then(|v| v.as_str()) {
                    res.len() as u64
                } else {
                    canonical_json(r).len() as u64
                }
            });

            vec![Input::ToolEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                tool_use_id,
                error,
                result_bytes,
                exit_code,
            }]
        }
        Some("Stop") | Some("TurnEnd") | Some("AssistantMessage") => {
            let input_tokens = obj
                .get("input_tokens")
                .or_else(|| obj.get("inputTokens"))
                .and_then(|v| v.as_u64());
            let output_tokens = obj
                .get("output_tokens")
                .or_else(|| obj.get("outputTokens"))
                .and_then(|v| v.as_u64());
            let cached_tokens = obj
                .get("cached_tokens")
                .or_else(|| obj.get("cachedTokens"))
                .and_then(|v| v.as_u64());
            let cost_micros = obj
                .get("cost_micros")
                .or_else(|| obj.get("costMicros"))
                .and_then(|v| v.as_u64());
            vec![Input::TurnEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                input_tokens,
                output_tokens,
                cached_tokens,
                text_bytes: None,
                cost_micros,
            }]
        }
        Some("Wait") | Some("WaitBegin") => {
            let on = match obj.get("on").and_then(|v| v.as_str()) {
                Some("subagent") => Some(WaitOn::Subagent),
                Some("input") => Some(WaitOn::Input),
                Some("message") => Some(WaitOn::Message),
                Some("rate_limit") => Some(WaitOn::RateLimit),
                _ => Some(WaitOn::Other),
            };
            let target = obj.get("target").and_then(|v| v.as_str()).map(String::from);
            vec![Input::Wait {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                target,
                on,
            }]
        }
        Some("WaitEnd") => {
            vec![Input::WaitEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                timed_out: obj
                    .get("timed_out")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }]
        }
        Some("SubagentStop") => {
            let reason = obj
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .map(String::from);

            let target_session = obj
                .get("subagent_id")
                .or_else(|| obj.get("agent_id"))
                .and_then(|v| v.as_str())
                .unwrap_or(session_id);

            vec![Input::RunEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", target_session),
                exit_code: Some(0),
                reason,
            }]
        }
        Some("PreCompact") => {
            vec![Input::Compacted {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                tokens_before: None,
                tokens_after: None,
                bytes_compacted: None,
            }]
        }
        Some("SessionEnd") => {
            let reason = obj.get("reason").and_then(|v| v.as_str()).map(String::from);
            let exit_code = if reason.as_deref() == Some("error") {
                Some(1)
            } else {
                Some(0)
            };

            vec![Input::RunEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                exit_code,
                reason,
            }]
        }
        Some("Notification") => {
            let notification_type = obj
                .get("notification_type")
                .and_then(|v| v.as_str())
                .unwrap_or("notification")
                .to_string();

            let on = if notification_type == "permission_prompt" {
                Some(WaitOn::Input)
            } else {
                Some(WaitOn::Other)
            };

            vec![Input::Wait {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("claude", session_id),
                target: Some(notification_type),
                on,
            }]
        }
        _ => {
            diags.unknown_payloads += 1;
            Vec::new()
        }
    }
}
