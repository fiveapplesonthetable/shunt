use tacho_core::Input;

/// Error returned when parsing an NDJSON line fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub field: Option<String>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(field) = &self.field {
            write!(f, "field `{}`: {}", field, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for Error {}

/// Parses a single line of NDJSON into an `Input` event with clear, field-aware error messages.
pub fn parse_ndjson_line(line: &str) -> Result<Input, Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(Error {
            message: "empty input line".to_string(),
            field: None,
        });
    }

    let val: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| Error {
        message: format!("invalid JSON syntax: {}", e),
        field: None,
    })?;

    let obj = val.as_object().ok_or_else(|| Error {
        message: "expected a JSON object representing an Input event".to_string(),
        field: None,
    })?;

    let type_val = obj.get("type").ok_or_else(|| Error {
        message: "missing required discriminator field `type`".to_string(),
        field: Some("type".to_string()),
    })?;

    let type_str = type_val.as_str().ok_or_else(|| Error {
        message: "field `type` must be a string".to_string(),
        field: Some("type".to_string()),
    })?;

    // Validate type discriminator against known variants
    match type_str {
        "run_start" | "run_end" | "prompt" | "turn_start" | "turn_end" | "tool_begin"
        | "tool_end" | "wait" | "wait_end" | "message" | "compacted" | "provider_error"
        | "task" | "task_state" | "finding" | "custom" | "counter_sample" => {}
        unknown => {
            return Err(Error {
                message: format!("unknown Input type `{}`", unknown),
                field: Some("type".to_string()),
            });
        }
    }

    // Deserialize into Input
    serde_json::from_value::<Input>(val).map_err(|err| {
        let err_str = err.to_string();
        let field = extract_field_name(&err_str);
        Error {
            message: err_str,
            field,
        }
    })
}

fn extract_field_name(err_str: &str) -> Option<String> {
    if let Some(pos) = err_str.find("missing field `") {
        let start = pos + "missing field `".len();
        if let Some(end) = err_str[start..].find('`') {
            return Some(err_str[start..start + end].to_string());
        }
    }
    if let Some(pos) = err_str.find("unknown field `") {
        let start = pos + "unknown field `".len();
        if let Some(end) = err_str[start..].find('`') {
            return Some(err_str[start..start + end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_ndjson_lines() {
        let lines = [
            r#"{"type":"run_start","provider":"claude","session_id":"s1"}"#,
            r#"{"type":"run_end","session":{"provider":"claude","session_id":"s1"}}"#,
            r#"{"type":"prompt","session":{"provider":"claude","session_id":"s1"},"text_len":42}"#,
            r#"{"type":"turn_start","session":{"provider":"claude","session_id":"s1"}}"#,
            r#"{"type":"turn_end","session":{"provider":"claude","session_id":"s1"},"input_tokens":100}"#,
            r#"{"type":"tool_begin","session":{"provider":"claude","session_id":"s1"},"tool_use_id":"t1","tool_name":"Bash","input":{"tool":"shell","argv":"ls"}}"#,
            r#"{"type":"tool_end","session":{"provider":"claude","session_id":"s1"},"tool_use_id":"t1","error":false}"#,
            r#"{"type":"wait","session":{"provider":"claude","session_id":"s1"}}"#,
            r#"{"type":"wait_end","session":{"provider":"claude","session_id":"s1"},"timed_out":false}"#,
            r#"{"type":"message","from":"a","to":"b"}"#,
            r#"{"type":"compacted","session":{"provider":"claude","session_id":"s1"}}"#,
            r#"{"type":"provider_error","provider":"claude"}"#,
            r#"{"type":"task","slug":"t1","paths":["a.rs"]}"#,
            r#"{"type":"task_state","slug":"t1","state":"in_progress"}"#,
            r#"{"type":"finding","task":"t1","severity":"major","title":"bug"}"#,
            r#"{"type":"custom","track":"custom:my","op":"begin"}"#,
            r#"{"type":"counter_sample","track_key":"cpu","value":100}"#,
        ];

        for line in lines {
            let res = parse_ndjson_line(line);
            assert!(res.is_ok(), "Failed to parse: {}: {:?}", line, res.err());
        }
    }

    #[test]
    fn test_parse_errors_with_bad_field_named() {
        // Missing required discriminator `type`
        let err1 = parse_ndjson_line(r#"{"provider":"claude"}"#).unwrap_err();
        assert_eq!(err1.field.as_deref(), Some("type"));

        // Unknown type
        let err2 = parse_ndjson_line(r#"{"type":"totally_unknown_kind"}"#).unwrap_err();
        assert_eq!(err2.field.as_deref(), Some("type"));
        assert!(err2.to_string().contains("totally_unknown_kind"));

        // Missing field in variant (run_start requires session_id)
        let err3 = parse_ndjson_line(r#"{"type":"run_start","provider":"claude"}"#).unwrap_err();
        assert_eq!(err3.field.as_deref(), Some("session_id"));
        assert!(err3.to_string().contains("session_id"));

        // Invalid JSON
        let err4 = parse_ndjson_line(r#"not json"#).unwrap_err();
        assert!(err4.to_string().contains("invalid JSON"));
    }
}
