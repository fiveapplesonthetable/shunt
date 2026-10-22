use tacho_core::Input;
use tacho_ingest::hooks::HookEnv;
use tacho_ingest::streams::{StreamKind, StreamParser};

#[test]
fn test_canary_secrets_never_leak_in_debug_output() {
    let secret_prompt = "CANARY_SECRET_USER_PROMPT_ABC123XYZ";
    let secret_file_content = "CANARY_SECRET_FILE_CONTENT_VERY_CONFIDENTIAL_KEY_999";
    let secret_edit_old = "CANARY_SECRET_OLD_CODE_STRING_TO_REPLACE_888";
    let secret_edit_new = "CANARY_SECRET_NEW_CODE_STRING_REPLACEMENT_777";
    let secret_response_stdout = "CANARY_SECRET_COMMAND_OUTPUT_STDOUT_666";
    let secret_response_stderr = "CANARY_SECRET_COMMAND_OUTPUT_STDERR_555";
    let secret_other_tool = "CANARY_SECRET_UNCLASSIFIED_TOOL_PAYLOAD_444";

    let env = HookEnv {
        agent: Some("worker-1".to_string()),
        task: Some("task-canary".to_string()),
        invoked_at_ns: 42_000_000,
        ppid: Some(101),
    };

    let mut all_produced_inputs: Vec<Input> = Vec::new();

    // 1. Claude UserPromptSubmit containing secret prompt
    let prompt_payload = serde_json::json!({
        "session_id": "sess-canary",
        "hook_event_name": "UserPromptSubmit",
        "prompt": format!("Please analyze this token: {}", secret_prompt),
    });
    all_produced_inputs.extend(tacho_ingest::hooks::claude::map(&prompt_payload, &env));

    // 2. Claude PreToolUse Write containing secret file content
    let write_payload = serde_json::json!({
        "session_id": "sess-canary",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_write_1",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "src/secret.key",
            "content": format!("PRIVATE KEY:\n{}", secret_file_content),
        }
    });
    all_produced_inputs.extend(tacho_ingest::hooks::claude::map(&write_payload, &env));

    // 3. Claude PreToolUse Edit containing secret replacement
    let edit_payload = serde_json::json!({
        "session_id": "sess-canary",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_edit_1",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "src/config.rs",
            "old_string": secret_edit_old,
            "new_string": secret_edit_new,
        }
    });
    all_produced_inputs.extend(tacho_ingest::hooks::claude::map(&edit_payload, &env));

    // 4. Claude PostToolUse containing secret stdout and stderr
    let post_tool_payload = serde_json::json!({
        "session_id": "sess-canary",
        "hook_event_name": "PostToolUse",
        "tool_use_id": "toolu_write_1",
        "tool_name": "Write",
        "tool_response": {
            "exit_code": 0,
            "stdout": secret_response_stdout,
            "stderr": secret_response_stderr,
        }
    });
    all_produced_inputs.extend(tacho_ingest::hooks::claude::map(&post_tool_payload, &env));

    // 5. Claude PreToolUse Other containing secret payload
    let other_payload = serde_json::json!({
        "session_id": "sess-canary",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "toolu_custom_1",
        "tool_name": "CustomUnknownTool",
        "tool_input": {
            "sensitive_param": secret_other_tool,
        }
    });
    all_produced_inputs.extend(tacho_ingest::hooks::claude::map(&other_payload, &env));

    // 6. Jetski PreToolUse and PostToolUse with edit_file and secret Target/Replacement
    let jetski_edit = serde_json::json!({
        "conversationId": "jetski-canary",
        "hook_event_name": "PreToolUse",
        "stepIdx": 10,
        "toolCall": {
            "name": "edit_file",
            "args": {
                "Path": "auth.rs",
                "Target": secret_edit_old,
                "Replacement": secret_edit_new,
            }
        }
    });
    all_produced_inputs.extend(tacho_ingest::hooks::jetski::map(&jetski_edit, &env));

    let jetski_post = serde_json::json!({
        "conversationId": "jetski-canary",
        "hook_event_name": "PostToolUse",
        "stepIdx": 10,
        "toolCall": {
            "name": "edit_file",
            "args": {
                "Path": "auth.rs",
            }
        },
        "result": secret_response_stdout,
        "error": secret_response_stderr,
    });
    all_produced_inputs.extend(tacho_ingest::hooks::jetski::map(&jetski_post, &env));

    // 7. Claude Stream feeding secrets
    let mut claude_parser = StreamParser::for_kind(StreamKind::Claude);
    let stream_user_line = serde_json::json!({
        "type": "user",
        "session_id": "sess-canary",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_stream_1",
                "content": secret_response_stdout,
            }]
        }
    })
    .to_string();
    all_produced_inputs.extend(claude_parser.feed_line(&stream_user_line, 50_000_000));

    // Assert that NONE of the secrets appear in any Debug representation!
    let all_secrets = [
        secret_prompt,
        secret_file_content,
        secret_edit_old,
        secret_edit_new,
        secret_response_stdout,
        secret_response_stderr,
        secret_other_tool,
    ];

    assert!(
        !all_produced_inputs.is_empty(),
        "Inputs must have been produced"
    );

    for (idx, input) in all_produced_inputs.iter().enumerate() {
        let debug_rep = format!("{:?}", input);

        for secret in all_secrets {
            assert!(
                !debug_rep.contains(secret),
                "Privacy violation at input #{}: secret '{}' leaked in Debug output: {}",
                idx,
                secret,
                debug_rep
            );
        }
    }
}
