use std::collections::BTreeMap;
use tacho_core::{Input, SessionKey};

use crate::hooks::{blake3_digest_8, classify_tool};

/// Stateful parser for Claude Code `stream-json` streams.
#[derive(Debug, Default)]
pub struct StreamParser {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
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
    /// Creates a new Claude stream parser.
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
            Some("system") => {
                let subtype = obj.get("subtype").and_then(|v| v.as_str());
                if subtype == Some("init") {
                    let sid = obj
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    self.session_id = Some(sid.clone());

                    self.cwd = obj.get("cwd").and_then(|v| v.as_str()).map(String::from);
                    self.model = obj.get("model").and_then(|v| v.as_str()).map(String::from);

                    out.push(Input::RunStart {
                        source_ts_ns: Some(now_ns),
                        provider: "claude".to_string(),
                        session_id: sid,
                        agent: None,
                        task: None,
                        pid: None,
                        cwd: self.cwd.clone(),
                        model: self.model.clone(),
                        parent_session: None,
                        git_tree: None,
                    });
                }
            }
            Some("assistant") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                if !self.in_turn {
                    self.in_turn = true;
                    self.accumulated_text_bytes = 0;
                    out.push(Input::TurnStart {
                        source_ts_ns: Some(now_ns),
                        session: SessionKey::new("claude", &session_id),
                        model: self.model.clone(),
                    });
                }

                if let Some(msg) = obj.get("message").and_then(|m| m.as_object()) {
                    if let Some(usage) = msg.get("usage").and_then(|u| u.as_object()) {
                        if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            self.pending_input_tokens = Some(inp);
                        }
                        if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            self.pending_output_tokens = Some(out);
                        }
                        let cache_read = usage
                            .get("cache_read_input_tokens")
                            .or_else(|| usage.get("cache_read_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_create = usage
                            .get("cache_creation_input_tokens")
                            .or_else(|| usage.get("cache_creation_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        if cache_read + cache_create > 0 {
                            self.pending_cached_tokens = Some(cache_read + cache_create);
                        }
                    }

                    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                        for item in content {
                            let item_type = item.get("type").and_then(|t| t.as_str());
                            match item_type {
                                Some("text") => {
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        self.accumulated_text_bytes += text.len() as u64;
                                    }
                                }
                                Some("tool_use") => {
                                    let id = item
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let name = item
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let input_val =
                                        item.get("input").unwrap_or(&serde_json::Value::Null);

                                    let tool_input = classify_tool(&name, input_val);
                                    self.open_tools.insert(id.clone(), name.clone());

                                    out.push(Input::ToolBegin {
                                        source_ts_ns: Some(now_ns),
                                        session: SessionKey::new("claude", &session_id),
                                        tool_use_id: id,
                                        tool_name: name,
                                        input: tool_input,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            Some("user") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                if let Some(msg) = obj.get("message").and_then(|m| m.as_object()) {
                    let mut text_prompts = Vec::new();
                    let mut tool_results = Vec::new();

                    if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
                        for item in content_arr {
                            let item_type = item.get("type").and_then(|t| t.as_str());
                            if item_type == Some("tool_result") {
                                tool_results.push(item);
                            } else if item_type == Some("text") {
                                if let Some(t) = item.get("text").and_then(|s| s.as_str()) {
                                    text_prompts.push(t.to_string());
                                }
                            }
                        }
                    } else if let Some(content_str) = msg.get("content").and_then(|c| c.as_str()) {
                        text_prompts.push(content_str.to_string());
                    }

                    for item in tool_results {
                        let tool_use_id = item
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();

                        self.open_tools.remove(&tool_use_id);

                        let is_error = item
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        let content_str = item.get("content").and_then(|v| v.as_str());
                        let result_bytes = content_str.map(|s| s.len() as u64);

                        out.push(Input::ToolEnd {
                            source_ts_ns: Some(now_ns),
                            session: SessionKey::new("claude", &session_id),
                            tool_use_id,
                            error: is_error,
                            result_bytes,
                            exit_code: None,
                        });
                    }

                    for text in text_prompts {
                        if self.in_turn {
                            out.push(Input::TurnEnd {
                                source_ts_ns: Some(now_ns),
                                session: SessionKey::new("claude", &session_id),
                                input_tokens: self.pending_input_tokens.take(),
                                output_tokens: self.pending_output_tokens.take(),
                                cached_tokens: self.pending_cached_tokens.take(),
                                text_bytes: Some(self.accumulated_text_bytes),
                                cost_micros: None,
                            });
                            self.in_turn = false;
                        }

                        let text_len = text.len() as u64;
                        let digest = blake3_digest_8(&text);

                        out.push(Input::Prompt {
                            source_ts_ns: Some(now_ns),
                            session: SessionKey::new("claude", &session_id),
                            text_len,
                            digest: Some(digest),
                        });

                        self.in_turn = true;
                        self.accumulated_text_bytes = 0;
                        out.push(Input::TurnStart {
                            source_ts_ns: Some(now_ns),
                            session: SessionKey::new("claude", &session_id),
                            model: self.model.clone(),
                        });
                    }
                }
            }
            Some("result") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                let usage = obj.get("usage");
                let input_tokens = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .or(self.pending_input_tokens.take());
                let output_tokens = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .or(self.pending_output_tokens.take());

                let cache_read = usage
                    .and_then(|u| {
                        u.get("cache_read_tokens")
                            .or_else(|| u.get("cache_read_input_tokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_create = usage
                    .and_then(|u| {
                        u.get("cache_creation_tokens")
                            .or_else(|| u.get("cache_creation_input_tokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cached_tokens = if cache_read + cache_create > 0 {
                    Some(cache_read + cache_create)
                } else {
                    self.pending_cached_tokens.take()
                };

                let cost_micros = obj
                    .get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .map(|usd| (usd * 1_000_000.0).round() as u64);

                let text_bytes = obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .map(|r| r.len() as u64)
                    .or(Some(self.accumulated_text_bytes));

                if self.in_turn {
                    self.in_turn = false;

                    out.push(Input::TurnEnd {
                        source_ts_ns: Some(now_ns),
                        session: SessionKey::new("claude", &session_id),
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        text_bytes,
                        cost_micros,
                    });
                }

                let subtype = obj
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                out.push(Input::RunEnd {
                    source_ts_ns: Some(now_ns),
                    session: SessionKey::new("claude", &session_id),
                    exit_code: Some(0),
                    reason: subtype,
                });
            }
            _ => {}
        }

        out
    }
}
