use tacho_core::{Input, SessionKey};

use crate::hooks::{blake3_digest_8, classify_tool, parse_exit_status, HookDiagnostics, HookEnv};

/// Maps a Gemini CLI hook payload into tachograph `Input` events.
pub fn map(payload: &serde_json::Value, env: &HookEnv) -> Vec<Input> {
    let mut diags = HookDiagnostics::default();
    map_with_diagnostics(payload, env, &mut diags)
}

/// Maps a Gemini CLI hook payload into tachograph `Input` events, recording diagnostics.
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
            let git_tree = obj
                .get("git_tree")
                .or_else(|| obj.get("tree"))
                .and_then(|v| v.as_str())
                .map(String::from);

            vec![Input::RunStart {
                source_ts_ns: Some(env.invoked_at_ns),
                provider: "gemini".to_string(),
                session_id: session_id.to_string(),
                agent: env.agent.clone(),
                task: env.task.clone(),
                pid: env.ppid,
                cwd,
                model,
                parent_session: None,
                git_tree,
            }]
        }
        Some("BeforeAgent") => {
            let prompt = obj
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            vec![
                Input::Prompt {
                    source_ts_ns: Some(env.invoked_at_ns),
                    session: SessionKey::new("gemini", session_id),
                    text_len: prompt.len() as u64,
                    digest: Some(blake3_digest_8(prompt)),
                },
                Input::TurnStart {
                    source_ts_ns: Some(env.invoked_at_ns),
                    session: SessionKey::new("gemini", session_id),
                    model: None,
                },
            ]
        }
        Some("BeforeTool") => {
            let tool_call = obj.get("tool_call").unwrap_or(&serde_json::Value::Null);
            let tool_name = tool_call
                .get("name")
                .or_else(|| obj.get("tool_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_use_id = tool_call
                .get("id")
                .or_else(|| tool_call.get("call_id"))
                .or_else(|| tool_call.get("tool_use_id"))
                .and_then(|v| v.as_str())
                .unwrap_or(&tool_name)
                .to_string();

            let args = tool_call.get("args").unwrap_or(tool_call);

            let input = classify_tool(&tool_name, args);

            vec![Input::ToolBegin {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("gemini", session_id),
                tool_use_id,
                tool_name,
                input,
            }]
        }
        Some("AfterTool") => {
            let tool_call = obj.get("tool_call").unwrap_or(&serde_json::Value::Null);
            let tool_name = tool_call
                .get("name")
                .or_else(|| obj.get("tool_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_use_id = tool_call
                .get("id")
                .or_else(|| tool_call.get("call_id"))
                .or_else(|| tool_call.get("tool_use_id"))
                .and_then(|v| v.as_str())
                .unwrap_or(&tool_name)
                .to_string();

            let result_str = obj.get("result").and_then(|v| v.as_str());

            let result_bytes = result_str.map(|s| s.len() as u64);

            let error = match obj.get("error") {
                Some(err) if !err.is_null() => {
                    if let Some(b) = err.as_bool() {
                        b
                    } else if let Some(s) = err.as_str() {
                        !s.is_empty()
                    } else {
                        true
                    }
                }
                _ => false,
            };

            let exit_code = obj
                .get("exit_code")
                .or_else(|| obj.get("exitCode"))
                .or_else(|| obj.get("status"))
                .and_then(|v| v.as_u64())
                .map(|n| n as i32)
                .or_else(|| {
                    obj.get("error")
                        .and_then(|err| err.as_str())
                        .and_then(parse_exit_status)
                })
                .or(if !error { Some(0) } else { None });

            vec![Input::ToolEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("gemini", session_id),
                tool_use_id,
                error,
                result_bytes,
                exit_code,
            }]
        }
        Some("AfterAgent") => {
            vec![Input::TurnEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("gemini", session_id),
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                text_bytes: None,
                cost_micros: None,
            }]
        }
        Some("SessionEnd") => {
            let reason = obj.get("reason").and_then(|v| v.as_str()).map(String::from);

            vec![Input::RunEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("gemini", session_id),
                exit_code: Some(0),
                reason,
            }]
        }
        _ => {
            diags.unknown_payloads += 1;
            Vec::new()
        }
    }
}
