pub mod claude;
pub mod codex;
pub mod gemini;

use tacho_core::Input;

/// Known stream formats detected from initial lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Claude,
    Gemini,
    Codex,
}

/// Detects the stream kind by inspecting initial lines of output.
/// Detection is purely content-based, never relying on program name.
pub fn detect(first_lines: &[&str]) -> Option<StreamKind> {
    for line in first_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Fast string checks before JSON parse
        if trimmed.contains("\"thread.started\"") || trimmed.contains("\"turn.started\"") {
            return Some(StreamKind::Codex);
        }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(obj) = val.as_object() {
                let msg_type = obj.get("type").and_then(|v| v.as_str());

                // Codex checks
                if matches!(
                    msg_type,
                    Some("thread.started") | Some("turn.started") | Some("turn.completed")
                ) || obj.contains_key("thread_id")
                {
                    return Some(StreamKind::Codex);
                }

                // Claude checks
                if let Some(subtype) = obj.get("subtype").and_then(|v| v.as_str()) {
                    if subtype == "init" {
                        return Some(StreamKind::Claude);
                    }
                }
                if let Some(model) = obj.get("model").and_then(|v| v.as_str()) {
                    if model.contains("claude") {
                        return Some(StreamKind::Claude);
                    }
                    if model.contains("gemini") {
                        return Some(StreamKind::Gemini);
                    }
                }
                if msg_type == Some("system") {
                    return Some(StreamKind::Claude);
                }
                if (msg_type == Some("assistant") || msg_type == Some("user"))
                    && obj.contains_key("message")
                {
                    return Some(StreamKind::Claude);
                }

                // Gemini checks
                if matches!(msg_type, Some("tool_use") | Some("tool_result")) {
                    return Some(StreamKind::Gemini);
                }
                if msg_type == Some("init") && !obj.contains_key("subtype") {
                    return Some(StreamKind::Gemini);
                }
            }
        }
    }

    None
}

/// A unified stream parser that delegates to provider-specific parsers.
#[derive(Debug)]
pub enum StreamParser {
    Claude(claude::StreamParser),
    Gemini(gemini::StreamParser),
    Codex(codex::StreamParser),
}

impl StreamParser {
    /// Creates a new stream parser for the given stream kind.
    pub fn for_kind(kind: StreamKind) -> Self {
        match kind {
            StreamKind::Claude => Self::Claude(claude::StreamParser::new()),
            StreamKind::Gemini => Self::Gemini(gemini::StreamParser::new()),
            StreamKind::Codex => Self::Codex(codex::StreamParser::new()),
        }
    }

    /// Feeds one line of stream output and returns parsed inputs.
    pub fn feed_line(&mut self, line: &str, now_ns: u64) -> Vec<Input> {
        match self {
            Self::Claude(p) => p.feed_line(line, now_ns),
            Self::Gemini(p) => p.feed_line(line, now_ns),
            Self::Codex(p) => p.feed_line(line, now_ns),
        }
    }
}
