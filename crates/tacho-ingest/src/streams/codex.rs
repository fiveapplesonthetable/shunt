use tacho_core::{Input, SessionKey, ToolInput};

/// Stateful parser for OpenAI Codex `codex exec --json` streams.
#[derive(Debug, Default)]
pub struct StreamParser {
    pub thread_id: Option<String>,
    pub in_turn: bool,
    pub accumulated_text_bytes: u64,
    pub buffer: String,
}

impl StreamParser {
    /// Creates a new Codex stream parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk or line of NDJSON and returns zero or more parsed `Input` events.
    pub fn feed_line(&mut self, line: &str, now_ns: u64) -> Vec<Input> {
        self.buffer.push_str(line);
        let mut out = Vec::new();

        while let Some(pos) = self.buffer.find('\n') {
            let full_line = self.buffer[..pos].trim().to_string();
            self.buffer.drain(..=pos);
            if !full_line.is_empty() {
                out.extend(self.process_json_line(&full_line, now_ns));
            }
        }

        let trimmed = self.buffer.trim();
        if !trimmed.is_empty() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if val.is_object() {
                    let full_line = trimmed.to_string();
                    self.buffer.clear();
                    out.extend(self.process_json_line(&full_line, now_ns));
                }
            }
        }

        out
    }

    fn process_json_line(&mut self, line: &str, now_ns: u64) -> Vec<Input> {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };

        let Some(obj) = val.as_object() else {
            return Vec::new();
        };

        let msg_type = obj.get("type").and_then(|v| v.as_str());
        let mut out = Vec::new();

        match msg_type {
            Some("thread.started") => {
                let tid = obj
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                self.thread_id = Some(tid.clone());

                out.push(Input::RunStart {
                    source_ts_ns: Some(now_ns),
                    provider: "codex".to_string(),
                    session_id: tid,
                    agent: None,
                    task: None,
                    pid: None,
                    cwd: None,
                    model: None,
                    parent_session: None,
                    git_tree: None,
                });
            }
            Some("turn.started") => {
                self.in_turn = true;
                self.accumulated_text_bytes = 0;
                let session_id = self
                    .thread_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                out.push(Input::TurnStart {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("codex", session_id),
                    model: None,
                });
            }
            Some("item.completed") => {
                let session_id = self
                    .thread_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                if let Some(item) = obj.get("item").and_then(|v| v.as_object()) {
                    let item_type = item.get("type").and_then(|v| v.as_str());
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("item")
                        .to_string();

                    match item_type {
                        Some("agent_message") => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                self.accumulated_text_bytes += text.len() as u64;
                            }
                        }
                        Some("command_execution") => {
                            let cmd = item
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();

                            let exit_code = item
                                .get("exit_code")
                                .and_then(|v| v.as_i64())
                                .map(|c| c as i32);

                            let output = item.get("output").and_then(|v| v.as_str());

                            let result_bytes = output.map(|s| s.len() as u64);
                            let error = exit_code.map(|c| c != 0).unwrap_or(false);

                            out.push(Input::ToolBegin {
                                source_ts_ns: Some(now_ns),
                                session: SessionKey::new("codex", &session_id),
                                tool_use_id: id.clone(),
                                tool_name: "command_execution".to_string(),
                                input: ToolInput::Shell {
                                    argv: cmd,
                                    cwd: None,
                                    git_tree: None,
                                },
                            });

                            out.push(Input::ToolEnd {
                                source_ts_ns: Some(now_ns),
                                session: SessionKey::new("codex", &session_id),
                                tool_use_id: id,
                                error,
                                result_bytes,
                                exit_code,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Some("turn.completed") => {
                let session_id = self
                    .thread_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                self.in_turn = false;

                let usage = obj.get("usage");
                let input_tokens = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64());
                let output_tokens = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64());
                let cached_tokens = usage
                    .and_then(|u| u.get("cached_input_tokens"))
                    .and_then(|v| v.as_u64());

                out.push(Input::TurnEnd {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("codex", session_id),
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    text_bytes: Some(self.accumulated_text_bytes),
                    cost_micros: None,
                });
            }
            _ => {}
        }

        out
    }
}
