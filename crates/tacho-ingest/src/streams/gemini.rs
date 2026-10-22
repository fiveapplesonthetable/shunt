use std::collections::BTreeMap;
use tacho_core::{Input, SessionKey};

use crate::hooks::{blake3_digest_8, classify_tool, parse_exit_status};

/// Stateful parser for Gemini CLI stream lines.
#[derive(Debug, Default)]
pub struct StreamParser {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub in_turn: bool,
    pub accumulated_text_bytes: u64,
    pub open_tools: BTreeMap<String, String>,
    pub pending_input_tokens: Option<u64>,
    pub pending_output_tokens: Option<u64>,
    pub pending_cached_tokens: Option<u64>,
    pub buffer: String,
}

impl StreamParser {
    /// Creates a new Gemini stream parser.
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

        if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
            if self.session_id.is_none() {
                self.session_id = Some(sid.to_string());
            }
        }

        let msg_type = obj.get("type").and_then(|v| v.as_str());
        let mut out = Vec::new();

        match msg_type {
            Some("init") => {
                let sid = obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                self.session_id = Some(sid.clone());

                self.model = obj.get("model").and_then(|v| v.as_str()).map(String::from);

                out.push(Input::RunStart {
                    source_ts_ns: Some(now_ns),
                    provider: "gemini".to_string(),
                    session_id: sid,
                    agent: None,
                    task: None,
                    pid: None,
                    cwd: None,
                    model: self.model.clone(),
                    parent_session: None,
                });
            }
            Some("message") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let role = obj.get("role").and_then(|v| v.as_str());
                let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");

                // Extract usage metadata if present
                if let Some(usage) = obj
                    .get("usageMetadata")
                    .or_else(|| obj.get("usage_metadata"))
                    .or_else(|| obj.get("usage"))
                {
                    if let Some(inp) = usage
                        .get("promptTokenCount")
                        .or_else(|| usage.get("prompt_token_count"))
                        .or_else(|| usage.get("input_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        self.pending_input_tokens = Some(inp);
                    }
                    if let Some(out) = usage
                        .get("candidatesTokenCount")
                        .or_else(|| usage.get("candidates_token_count"))
                        .or_else(|| usage.get("output_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        self.pending_output_tokens = Some(out);
                    }
                    if let Some(cached) = usage
                        .get("cachedContentTokenCount")
                        .or_else(|| usage.get("cached_content_token_count"))
                        .or_else(|| usage.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        self.pending_cached_tokens = Some(cached);
                    }
                }

                match role {
                    Some("user") => {
                        self.in_turn = true;
                        self.accumulated_text_bytes = 0;

                        out.push(Input::Prompt {
                            source_ts_ns: Some(now_ns),
                            session: SessionKey::new("gemini", &session_id),
                            text_len: content.len() as u64,
                            digest: Some(blake3_digest_8(content)),
                        });

                        out.push(Input::TurnStart {
                            source_ts_ns: Some(now_ns),
                            session: SessionKey::new("gemini", &session_id),
                            model: self.model.clone(),
                        });
                    }
                    Some("assistant") => {
                        self.accumulated_text_bytes += content.len() as u64;
                    }
                    _ => {}
                }
            }
            Some("tool_use") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                if !self.in_turn {
                    self.in_turn = true;
                    out.push(Input::TurnStart {
                        source_ts_ns: Some(now_ns),
                        session: SessionKey::new("gemini", &session_id),
                        model: self.model.clone(),
                    });
                }

                let tool_name = obj
                    .get("tool_name")
                    .or_else(|| obj.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let tool_id = obj
                    .get("tool_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tool_name)
                    .to_string();

                let args = obj.get("args").unwrap_or(&serde_json::Value::Null);
                let input = classify_tool(&tool_name, args);

                self.open_tools.insert(tool_id.clone(), tool_name.clone());

                out.push(Input::ToolBegin {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("gemini", &session_id),
                    tool_use_id: tool_id,
                    tool_name,
                    input,
                });
            }
            Some("tool_result") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                let tool_name = obj
                    .get("tool_name")
                    .or_else(|| obj.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let tool_id = obj
                    .get("tool_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tool_name)
                    .to_string();

                self.open_tools.remove(&tool_id);

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

                out.push(Input::ToolEnd {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("gemini", &session_id),
                    tool_use_id: tool_id,
                    error,
                    result_bytes,
                    exit_code,
                });
            }
            Some("result") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                let usage = obj
                    .get("usageMetadata")
                    .or_else(|| obj.get("usage_metadata"))
                    .or_else(|| obj.get("usage"));

                let input_tokens = usage
                    .and_then(|u| {
                        u.get("promptTokenCount")
                            .or_else(|| u.get("prompt_token_count"))
                            .or_else(|| u.get("input_tokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .or_else(|| obj.get("input_tokens").and_then(|v| v.as_u64()))
                    .or(self.pending_input_tokens.take());

                let output_tokens = usage
                    .and_then(|u| {
                        u.get("candidatesTokenCount")
                            .or_else(|| u.get("candidates_token_count"))
                            .or_else(|| u.get("output_tokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .or_else(|| obj.get("output_tokens").and_then(|v| v.as_u64()))
                    .or(self.pending_output_tokens.take());

                let cached_tokens = usage
                    .and_then(|u| {
                        u.get("cachedContentTokenCount")
                            .or_else(|| u.get("cached_content_token_count"))
                            .or_else(|| u.get("cached_tokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .or(self.pending_cached_tokens.take());

                if self.in_turn {
                    self.in_turn = false;

                    out.push(Input::TurnEnd {
                        source_ts_ns: Some(now_ns),
                        session: SessionKey::new("gemini", &session_id),
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        text_bytes: Some(self.accumulated_text_bytes),
                        cost_micros: None,
                    });
                }

                let status = obj.get("status").and_then(|v| v.as_str()).map(String::from);

                out.push(Input::RunEnd {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("gemini", &session_id),
                    exit_code: Some(0),
                    reason: status,
                });
            }
            _ => {}
        }

        out
    }
}
