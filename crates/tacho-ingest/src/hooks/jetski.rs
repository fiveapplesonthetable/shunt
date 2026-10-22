use tacho_core::{Input, SessionKey};

use crate::hooks::{classify_tool, parse_exit_status, HookDiagnostics, HookEnv};

/// Maps a Jetski hook payload into tachograph `Input` events.
pub fn map(payload: &serde_json::Value, env: &HookEnv) -> Vec<Input> {
    let mut diags = HookDiagnostics::default();
    map_with_diagnostics(payload, env, &mut diags)
}

/// Maps a Jetski hook payload into tachograph `Input` events, recording diagnostics.
pub fn map_with_diagnostics(
    payload: &serde_json::Value,
    env: &HookEnv,
    diags: &mut HookDiagnostics,
) -> Vec<Input> {
    let Some(obj) = payload.as_object() else {
        diags.unknown_payloads += 1;
        return Vec::new();
    };

    let Some(session_id) = obj
        .get("conversationId")
        .or_else(|| obj.get("session_id"))
        .and_then(|v| v.as_str())
    else {
        diags.missing_fields += 1;
        return Vec::new();
    };

    let event_name = obj
        .get("hook_event_name")
        .or_else(|| obj.get("hookEventName"))
        .or_else(|| obj.get("hook_name"))
        .or_else(|| obj.get("hookName"))
        .or_else(|| obj.get("event"))
        .or_else(|| obj.get("type"))
        .and_then(|v| v.as_str());

    // Disambiguate event if hook_event_name is omitted
    let resolved_event = if let Some(e) = event_name {
        e
    } else if obj.contains_key("terminationReason") || obj.contains_key("executionNum") {
        "Stop"
    } else if obj.contains_key("result") && obj.contains_key("toolCall") {
        "PostToolUse"
    } else if obj.contains_key("toolCall") {
        "PreToolUse"
    } else if obj.contains_key("modelOutput")
        || obj.contains_key("modelThinking")
        || obj.contains_key("model_output")
        || obj.contains_key("model_thinking")
        || obj.contains_key("terminationBehavior")
    {
        "PostInvocation"
    } else if obj.contains_key("invocationNum") {
        "PreInvocation"
    } else if obj.contains_key("workspacePaths") {
        "SessionStart"
    } else {
        diags.unknown_payloads += 1;
        return Vec::new();
    };

    match resolved_event {
        "SessionStart" => {
            let cwd = obj
                .get("workspacePaths")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from);

            let model = obj
                .get("modelName")
                .or_else(|| obj.get("model"))
                .and_then(|v| v.as_str())
                .map(String::from);

            vec![Input::RunStart {
                source_ts_ns: Some(env.invoked_at_ns),
                provider: "jetski".to_string(),
                session_id: session_id.to_string(),
                agent: env.agent.clone(),
                task: env.task.clone(),
                pid: env.ppid,
                cwd,
                model,
                parent_session: None,
            }]
        }
        "PreInvocation" => {
            let model = obj
                .get("modelName")
                .or_else(|| obj.get("model"))
                .and_then(|v| v.as_str())
                .map(String::from);

            vec![Input::TurnStart {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("jetski", session_id),
                model,
            }]
        }
        "PreToolUse" => {
            let tool_call = obj.get("toolCall").unwrap_or(&serde_json::Value::Null);
            let tool_name = tool_call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_use_id = if let Some(idx) = obj.get("stepIdx").and_then(|v| v.as_i64()) {
                format!("step-{}", idx)
            } else {
                tool_name.clone()
            };

            let args = tool_call.get("args").unwrap_or(tool_call);
            let input = classify_tool(&tool_name, args);

            vec![Input::ToolBegin {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("jetski", session_id),
                tool_use_id,
                tool_name,
                input,
            }]
        }
        "PostToolUse" => {
            let tool_call = obj.get("toolCall").unwrap_or(&serde_json::Value::Null);
            let tool_name = tool_call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let tool_use_id = if let Some(idx) = obj.get("stepIdx").and_then(|v| v.as_i64()) {
                format!("step-{}", idx)
            } else {
                tool_name
            };

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

            let exit_code = if let Some(err) = obj.get("error") {
                if let Some(s) = err.as_str() {
                    parse_exit_status(s)
                } else {
                    err.as_u64().map(|n| n as i32)
                }
            } else if let Some(code) = obj
                .get("exit_code")
                .or_else(|| obj.get("exitCode"))
                .and_then(|v| v.as_u64())
            {
                Some(code as i32)
            } else if !error {
                Some(0)
            } else {
                None
            };

            vec![Input::ToolEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("jetski", session_id),
                tool_use_id,
                error,
                result_bytes,
                exit_code,
            }]
        }
        "PostInvocation" => {
            vec![Input::TurnEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("jetski", session_id),
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                text_bytes: None,
                cost_micros: None,
            }]
        }
        "Stop" => {
            let term_reason = obj
                .get("terminationReason")
                .and_then(|v| v.as_str())
                .map(String::from);

            let exit_code = if term_reason.as_deref() == Some("ERROR") {
                Some(1)
            } else {
                Some(0)
            };

            vec![Input::RunEnd {
                source_ts_ns: Some(env.invoked_at_ns),
                session: SessionKey::new("jetski", session_id),
                exit_code,
                reason: term_reason,
            }]
        }
        _ => {
            diags.unknown_payloads += 1;
            Vec::new()
        }
    }
}
