use std::collections::BTreeMap;
use tacho_core::{AgentRole, Input, SessionKey};

use crate::hooks::{blake3_digest_8, classify_tool};

/// Parses an ISO-8601 timestamp string (e.g. `"2026-10-22T14:03:12.456Z"`) or numeric
/// timestamp into nanoseconds since the UNIX epoch.
pub fn parse_timestamp_ns(val: &serde_json::Value) -> Option<u64> {
    if let Some(n) = val.as_u64() {
        return Some(if n < 10_000_000_000 {
            n.saturating_mul(1_000_000_000)
        } else if n < 10_000_000_000_000 {
            n.saturating_mul(1_000_000)
        } else if n < 10_000_000_000_000_000 {
            n.saturating_mul(1_000)
        } else {
            n
        });
    }
    if let Some(f) = val.as_f64() {
        if f > 0.0 {
            return Some(if f < 10_000_000_000.0 {
                (f * 1_000_000_000.0) as u64
            } else {
                (f * 1_000_000.0) as u64
            });
        }
    }
    let s = val.as_str()?.trim();
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u64 = s.get(11..13)?.parse().ok()?;
    let min: u64 = s.get(14..16)?.parse().ok()?;
    let sec: u64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut nanos: u64 = 0;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        let digits = &after_dot[..frac_len.min(9)];
        if let Ok(frac_val) = digits.parse::<u64>() {
            nanos = frac_val * 10u64.pow((9 - digits.len()) as u32);
        }
        rest = &after_dot[frac_len..];
    }

    let mut tz_offset_secs: i64 = 0;
    let rest = rest.trim();
    if !rest.is_empty() && rest != "Z" && rest != "z" {
        let sign: i64 = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let tz = &rest[1..];
        let tz_h: i64 = tz.get(0..2)?.parse().ok()?;
        let tz_m: i64 = if tz.len() >= 5 && tz.as_bytes().get(2) == Some(&b':') {
            tz.get(3..5)?.parse().ok()?
        } else if tz.len() >= 4 {
            tz.get(2..4)?.parse().ok()?
        } else {
            0
        };
        tz_offset_secs = sign * (tz_h * 3600 + tz_m * 60);
    }

    // Howard Hinnant's civil-to-days algorithm
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = if month > 2 {
        (month - 3) as u64
    } else {
        (month + 9) as u64
    };
    let doy = (153 * m_adj + 2) / 5 + (day as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + (doe as i64) - 719468;

    let epoch_secs =
        days * 86_400 + (hour as i64) * 3_600 + (min as i64) * 60 + (sec as i64) - tz_offset_secs;
    if epoch_secs < 0 {
        return None;
    }
    Some(
        (epoch_secs as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nanos),
    )
}

/// Stateful parser for Claude Code `stream-json` streams and on-disk `.jsonl` transcripts.
#[derive(Debug, Default)]
pub struct StreamParser {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub pid: Option<u32>,
    pub role: Option<AgentRole>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub run_started: bool,
    pub emit_run_start: bool,
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

    /// Creates a parser configured for tailing an on-disk Claude transcript file.
    pub fn for_transcript(session_id: Option<String>, agent: Option<String>) -> Self {
        Self {
            session_id,
            agent,
            emit_run_start: true,
            ..Self::default()
        }
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

        let ts_ns = obj
            .get("timestamp")
            .or_else(|| obj.get("ts"))
            .and_then(parse_timestamp_ns)
            .unwrap_or(now_ns);

        if let Some(sid) = obj
            .get("session_id")
            .or_else(|| obj.get("sessionId"))
            .and_then(|v| v.as_str())
        {
            if self.session_id.is_none() {
                self.session_id = Some(sid.to_string());
            }
        }
        if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
            if self.cwd.is_none() {
                self.cwd = Some(cwd.to_string());
            }
        }
        if let Some(model) = obj
            .get("model")
            .or_else(|| obj.get("message").and_then(|m| m.get("model")))
            .and_then(|v| v.as_str())
        {
            self.model = Some(model.to_string());
        }

        let msg_type = obj.get("type").and_then(|v| v.as_str());
        let mut out = Vec::new();

        match msg_type {
            Some("system") => {
                let subtype = obj.get("subtype").and_then(|v| v.as_str());
                if subtype == Some("init") {
                    let sid = obj
                        .get("session_id")
                        .or_else(|| obj.get("sessionId"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    self.session_id = Some(sid.clone());

                    if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                        self.cwd = Some(cwd.to_string());
                    }
                    if let Some(model) = obj.get("model").and_then(|v| v.as_str()) {
                        self.model = Some(model.to_string());
                    }
                    self.run_started = true;

                    out.push(Input::RunStart {
                        source_ts_ns: Some(ts_ns),
                        provider: "claude".to_string(),
                        session_id: sid,
                        agent: self.agent.clone(),
                        task: self.task.clone(),
                        pid: self.pid,
                        cwd: self.cwd.clone(),
                        model: self.model.clone(),
                        parent_session: None,
                        git_tree: None,
                        role: self.role,
                    });
                }
            }
            Some("assistant") => {
                let session_id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                if !self.run_started && (self.emit_run_start || obj.contains_key("sessionId")) {
                    self.run_started = true;
                    out.push(Input::RunStart {
                        source_ts_ns: Some(ts_ns),
                        provider: "claude".to_string(),
                        session_id: session_id.clone(),
                        agent: self.agent.clone(),
                        task: self.task.clone(),
                        pid: self.pid,
                        cwd: self.cwd.clone(),
                        model: self.model.clone(),
                        parent_session: None,
                        git_tree: None,
                        role: self.role,
                    });
                }

                if !self.in_turn {
                    self.in_turn = true;
                    self.accumulated_text_bytes = 0;
                    out.push(Input::TurnStart {
                        source_ts_ns: Some(ts_ns),
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
                                        source_ts_ns: Some(ts_ns),
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

                if !self.run_started && (self.emit_run_start || obj.contains_key("sessionId")) {
                    self.run_started = true;
                    out.push(Input::RunStart {
                        source_ts_ns: Some(ts_ns),
                        provider: "claude".to_string(),
                        session_id: session_id.clone(),
                        agent: self.agent.clone(),
                        task: self.task.clone(),
                        pid: self.pid,
                        cwd: self.cwd.clone(),
                        model: self.model.clone(),
                        parent_session: None,
                        git_tree: None,
                        role: self.role,
                    });
                }

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

                        let result_bytes = match item.get("content") {
                            Some(serde_json::Value::String(s)) => Some(s.len() as u64),
                            Some(serde_json::Value::Array(arr)) => {
                                let sum: u64 = arr
                                    .iter()
                                    .filter_map(|blk| blk.get("text").and_then(|t| t.as_str()))
                                    .map(|s| s.len() as u64)
                                    .sum();
                                Some(sum)
                            }
                            _ => None,
                        };

                        out.push(Input::ToolEnd {
                            source_ts_ns: Some(ts_ns),
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
                                source_ts_ns: Some(ts_ns),
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
                            source_ts_ns: Some(ts_ns),
                            session: SessionKey::new("claude", &session_id),
                            text_len,
                            digest: Some(digest),
                        });

                        self.in_turn = true;
                        self.accumulated_text_bytes = 0;
                        out.push(Input::TurnStart {
                            source_ts_ns: Some(ts_ns),
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
                        source_ts_ns: Some(ts_ns),
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
                    source_ts_ns: Some(ts_ns),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso_timestamp_and_ondisk_transcript() {
        let mut parser = StreamParser::for_transcript(None, Some("worker-1".to_string()));
        let l1 = r#"{"type":"user","sessionId":"abc-123","cwd":"/work/repo","timestamp":"2026-10-22T12:00:00.500Z","message":{"role":"user","content":"Fix the bug"}}"#;
        let l2 = r#"{"type":"assistant","sessionId":"abc-123","timestamp":"2026-10-22T12:00:05.000Z","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":100,"output_tokens":25}}}"#;
        let l3 = r#"{"type":"user","sessionId":"abc-123","timestamp":"2026-10-22T12:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"test result: ok"}]}]}}"#;

        let i1 = parser.feed_line(&format!("{}\n", l1), 999);
        assert_eq!(i1.len(), 3); // RunStart + Prompt + TurnStart
        assert!(matches!(
            &i1[0],
            Input::RunStart {
                source_ts_ns: Some(1_792_670_400_500_000_000),
                session_id,
                agent: Some(agent),
                cwd: Some(cwd),
                ..
            } if session_id == "abc-123" && agent == "worker-1" && cwd == "/work/repo"
        ));

        let i2 = parser.feed_line(&format!("{}\n", l2), 999);
        assert_eq!(i2.len(), 1); // ToolBegin
        assert!(matches!(
            &i2[0],
            Input::ToolBegin {
                source_ts_ns: Some(1_792_670_405_000_000_000),
                tool_use_id,
                ..
            } if tool_use_id == "toolu_1"
        ));

        let i3 = parser.feed_line(&format!("{}\n", l3), 999);
        assert_eq!(i3.len(), 1); // ToolEnd with array content length 15
        assert!(matches!(
            &i3[0],
            Input::ToolEnd {
                source_ts_ns: Some(1_792_670_410_000_000_000),
                result_bytes: Some(15),
                ..
            }
        ));
    }
}
