use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use prost::Message;
use std::collections::BTreeMap;
use tacho_core::{CustomOp, Input, SessionKey, ToolInput};

use crate::hooks::{blake3_digest_8, classify_tool};

/// Decoder for OTLP/HTTP telemetry (logs, metrics, traces) in Protobuf and JSON formats.
#[derive(Debug, Default)]
pub struct OtlpDecoder {
    pub unknown_count: u64,
}

impl OtlpDecoder {
    /// Creates a new OTLP decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes an OTLP ExportLogsServiceRequest protobuf payload.
    pub fn decode_logs_proto(&mut self, bytes: &[u8]) -> Vec<Input> {
        let Ok(req) = ExportLogsServiceRequest::decode(bytes) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for res_log in req.resource_logs {
            let service_name = res_log
                .resource
                .as_ref()
                .and_then(|r| get_attr_string(&r.attributes, "service.name"))
                .unwrap_or_else(|| "unknown".to_string());

            for scope_log in res_log.scope_logs {
                for record in scope_log.log_records {
                    let ts = if record.time_unix_nano > 0 {
                        Some(record.time_unix_nano)
                    } else if record.observed_time_unix_nano > 0 {
                        Some(record.observed_time_unix_nano)
                    } else {
                        None
                    };

                    let event_name = get_attr_string(&record.attributes, "event.name")
                        .or_else(|| get_attr_string(&record.attributes, "name"))
                        .or_else(|| {
                            if !record.event_name.is_empty() {
                                Some(record.event_name.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();

                    let session_id = get_attr_string(&record.attributes, "session_id")
                        .or_else(|| get_attr_string(&record.attributes, "session.id"))
                        .unwrap_or_else(|| "unknown".to_string());

                    let body_str = record
                        .body
                        .as_ref()
                        .map(any_value_to_string)
                        .unwrap_or_default();

                    match event_name.as_str() {
                        "claude_code.user_prompt" => {
                            out.push(Input::Prompt {
                                source_ts_ns: ts,
                                session: SessionKey::new("claude", session_id),
                                text_len: body_str.len() as u64,
                                digest: Some(blake3_digest_8(&body_str)),
                            });
                        }
                        "claude_code.tool_result" => {
                            let tool_use_id = get_attr_string(&record.attributes, "tool_use_id")
                                .unwrap_or_else(|| "unknown".to_string());
                            let is_error = get_attr_string(&record.attributes, "is_error")
                                .map(|s| s == "true")
                                .unwrap_or(false);
                            // Out-of-range codes are dropped rather than truncated: a
                            // wrapped `as i32` could turn 1<<32 into a fake success (0).
                            let exit_code = get_attr_i64(&record.attributes, "exit_code")
                                .and_then(|c| i32::try_from(c).ok());

                            out.push(Input::ToolEnd {
                                source_ts_ns: ts,
                                session: SessionKey::new("claude", session_id),
                                tool_use_id,
                                error: is_error,
                                result_bytes: Some(body_str.len() as u64),
                                exit_code,
                            });
                        }
                        "claude_code.api_error" => {
                            let error_code = get_attr_string(&record.attributes, "error_code");
                            out.push(Input::ProviderError {
                                source_ts_ns: ts,
                                provider: "claude".to_string(),
                                error_code,
                                message: Some(body_str),
                                rate_limit: false,
                                retry_after_ms: None,
                            });
                        }
                        "gemini_cli.tool_call" => {
                            let tool_name = get_attr_string(&record.attributes, "tool_name")
                                .unwrap_or_else(|| "unknown".to_string());
                            let tool_use_id = get_attr_string(&record.attributes, "tool_use_id")
                                .unwrap_or_else(|| tool_name.clone());

                            let val =
                                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                            let input = classify_tool(&tool_name, &val);

                            out.push(Input::ToolBegin {
                                source_ts_ns: ts,
                                session: SessionKey::new("gemini", session_id),
                                tool_use_id,
                                tool_name,
                                input,
                            });
                        }
                        "gemini_cli.chat_compression" => {
                            out.push(Input::Compacted {
                                source_ts_ns: ts,
                                session: SessionKey::new("gemini", session_id),
                                tokens_before: None,
                                tokens_after: None,
                                bytes_compacted: None,
                            });
                        }
                        _ => {
                            if !event_name.is_empty() {
                                let mut args = BTreeMap::new();
                                for kv in &record.attributes {
                                    if let Some(v) = &kv.value {
                                        args.insert(kv.key.clone(), any_value_to_string(v));
                                    }
                                }
                                out.push(Input::Custom {
                                    source_ts_ns: ts,
                                    track: format!("custom:{}", service_name),
                                    op: CustomOp::Instant,
                                    name: Some(event_name),
                                    id: None,
                                    args,
                                    value: None,
                                    flow_out: None,
                                    flow_in: None,
                                });
                            } else {
                                self.unknown_count += 1;
                            }
                        }
                    }
                }
            }
        }

        out
    }

    /// Decodes an OTLP ExportMetricsServiceRequest protobuf payload.
    pub fn decode_metrics_proto(&mut self, bytes: &[u8]) -> Vec<Input> {
        let Ok(req) = ExportMetricsServiceRequest::decode(bytes) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for res_metric in req.resource_metrics {
            for scope_metric in res_metric.scope_metrics {
                for metric in scope_metric.metrics {
                    let name = metric.name;
                    let unit = if metric.unit.is_empty() {
                        None
                    } else {
                        Some(metric.unit)
                    };

                    let points: Vec<(Option<u64>, i64)> = match metric.data {
                        Some(metric::Data::Gauge(g)) => g
                            .data_points
                            .into_iter()
                            .map(|p| {
                                let val = match p.value {
                                    Some(number_data_point::Value::AsInt(i)) => i,
                                    Some(number_data_point::Value::AsDouble(d)) => d as i64,
                                    None => 0,
                                };
                                (
                                    if p.time_unix_nano > 0 {
                                        Some(p.time_unix_nano)
                                    } else {
                                        None
                                    },
                                    val,
                                )
                            })
                            .collect(),
                        Some(metric::Data::Sum(s)) => s
                            .data_points
                            .into_iter()
                            .map(|p| {
                                let val = match p.value {
                                    Some(number_data_point::Value::AsInt(i)) => i,
                                    Some(number_data_point::Value::AsDouble(d)) => d as i64,
                                    None => 0,
                                };
                                (
                                    if p.time_unix_nano > 0 {
                                        Some(p.time_unix_nano)
                                    } else {
                                        None
                                    },
                                    val,
                                )
                            })
                            .collect(),
                        _ => {
                            self.unknown_count += 1;
                            Vec::new()
                        }
                    };

                    for (ts, val) in points {
                        out.push(Input::CounterSample {
                            source_ts_ns: ts,
                            track_key: name.clone(),
                            value: val,
                            unit: unit.clone(),
                        });
                    }
                }
            }
        }

        out
    }

    /// Decodes an OTLP ExportTraceServiceRequest protobuf payload.
    pub fn decode_traces_proto(&mut self, bytes: &[u8]) -> Vec<Input> {
        let Ok(req) = ExportTraceServiceRequest::decode(bytes) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for res_span in req.resource_spans {
            let service_name = res_span
                .resource
                .as_ref()
                .and_then(|r| get_attr_string(&r.attributes, "service.name"))
                .unwrap_or_else(|| "unknown".to_string());

            for scope_span in res_span.scope_spans {
                for span in scope_span.spans {
                    let start_ts = if span.start_time_unix_nano > 0 {
                        Some(span.start_time_unix_nano)
                    } else {
                        None
                    };
                    let end_ts = if span.end_time_unix_nano > 0 {
                        Some(span.end_time_unix_nano)
                    } else {
                        None
                    };

                    let span_id_hex = hex_encode(&span.span_id);
                    let mut args = BTreeMap::new();
                    for kv in &span.attributes {
                        if let Some(v) = &kv.value {
                            args.insert(kv.key.clone(), any_value_to_string(v));
                        }
                    }

                    let track = format!("custom:{}", service_name);

                    // Map specific Claude / Gemini tool spans if matching
                    if span.name == "claude_code.tool" {
                        let session_id = get_attr_string(&span.attributes, "session_id")
                            .unwrap_or_else(|| "unknown".to_string());
                        let tool_use_id = get_attr_string(&span.attributes, "tool_use_id")
                            .unwrap_or_else(|| span_id_hex.clone());
                        let tool_name = get_attr_string(&span.attributes, "tool_name")
                            .unwrap_or_else(|| "unknown".to_string());

                        out.push(Input::ToolBegin {
                            source_ts_ns: start_ts,
                            session: SessionKey::new("claude", &session_id),
                            tool_use_id: tool_use_id.clone(),
                            tool_name,
                            input: ToolInput::Other {
                                input_digest: None,
                                input_bytes: None,
                            },
                        });

                        if end_ts.is_some() {
                            out.push(Input::ToolEnd {
                                source_ts_ns: end_ts,
                                session: SessionKey::new("claude", session_id),
                                tool_use_id,
                                error: false,
                                result_bytes: None,
                                exit_code: None,
                            });
                        }
                    } else {
                        // Generic span -> Custom Begin & End on custom:<service.name>
                        out.push(Input::Custom {
                            source_ts_ns: start_ts,
                            track: track.clone(),
                            op: CustomOp::Begin,
                            name: Some(span.name.clone()),
                            id: Some(span_id_hex.clone()),
                            args,
                            value: None,
                            flow_out: None,
                            flow_in: None,
                        });

                        if end_ts.is_some() {
                            out.push(Input::Custom {
                                source_ts_ns: end_ts,
                                track,
                                op: CustomOp::End,
                                name: Some(span.name),
                                id: Some(span_id_hex),
                                args: BTreeMap::new(),
                                value: None,
                                flow_out: None,
                                flow_in: None,
                            });
                        }
                    }
                }
            }
        }

        out
    }

    /// Decodes an OTLP JSON logs payload.
    pub fn decode_logs_json(&mut self, value: &serde_json::Value) -> Vec<Input> {
        let Some(obj) = value.as_object() else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let Some(res_logs) = obj.get("resourceLogs").and_then(|v| v.as_array()) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for rl in res_logs {
            let service_name = rl
                .get("resource")
                .and_then(|r| r.get("attributes"))
                .and_then(get_json_attr_string("service.name"))
                .unwrap_or_else(|| "unknown".to_string());

            if let Some(scope_logs) = rl.get("scopeLogs").and_then(|v| v.as_array()) {
                for sl in scope_logs {
                    if let Some(log_records) = sl.get("logRecords").and_then(|v| v.as_array()) {
                        for rec in log_records {
                            let ts = rec
                                .get("timeUnixNano")
                                .or_else(|| rec.get("time_unix_nano"))
                                .and_then(parse_json_u64);

                            let attrs = rec.get("attributes");
                            let event_name = attrs
                                .and_then(get_json_attr_string("event.name"))
                                .or_else(|| attrs.and_then(get_json_attr_string("name")))
                                .or_else(|| {
                                    rec.get("eventName")
                                        .and_then(|v| v.as_str())
                                        .map(String::from)
                                })
                                .unwrap_or_default();

                            let session_id = attrs
                                .and_then(get_json_attr_string("session_id"))
                                .or_else(|| attrs.and_then(get_json_attr_string("session.id")))
                                .unwrap_or_else(|| "unknown".to_string());

                            let body_str = rec
                                .get("body")
                                .map(|b| {
                                    if let Some(s) = b.as_str() {
                                        s.to_string()
                                    } else if let Some(string_value) =
                                        b.get("stringValue").and_then(|v| v.as_str())
                                    {
                                        string_value.to_string()
                                    } else {
                                        b.to_string()
                                    }
                                })
                                .unwrap_or_default();

                            match event_name.as_str() {
                                "claude_code.user_prompt" => {
                                    out.push(Input::Prompt {
                                        source_ts_ns: ts,
                                        session: SessionKey::new("claude", session_id),
                                        text_len: body_str.len() as u64,
                                        digest: Some(blake3_digest_8(&body_str)),
                                    });
                                }
                                "claude_code.tool_result" => {
                                    let tool_use_id = attrs
                                        .and_then(get_json_attr_string("tool_use_id"))
                                        .unwrap_or_else(|| "unknown".to_string());
                                    let is_error = attrs
                                        .and_then(get_json_attr_string("is_error"))
                                        .map(|s| s == "true")
                                        .unwrap_or(false);

                                    out.push(Input::ToolEnd {
                                        source_ts_ns: ts,
                                        session: SessionKey::new("claude", session_id),
                                        tool_use_id,
                                        error: is_error,
                                        result_bytes: Some(body_str.len() as u64),
                                        exit_code: None,
                                    });
                                }
                                _ => {
                                    if !event_name.is_empty() {
                                        out.push(Input::Custom {
                                            source_ts_ns: ts,
                                            track: format!("custom:{}", service_name),
                                            op: CustomOp::Instant,
                                            name: Some(event_name),
                                            id: None,
                                            args: BTreeMap::new(),
                                            value: None,
                                            flow_out: None,
                                            flow_in: None,
                                        });
                                    } else {
                                        self.unknown_count += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        out
    }

    /// Decodes an OTLP JSON metrics payload.
    pub fn decode_metrics_json(&mut self, value: &serde_json::Value) -> Vec<Input> {
        let Some(obj) = value.as_object() else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let Some(res_metrics) = obj.get("resourceMetrics").and_then(|v| v.as_array()) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for rm in res_metrics {
            if let Some(scope_metrics) = rm.get("scopeMetrics").and_then(|v| v.as_array()) {
                for sm in scope_metrics {
                    if let Some(metrics) = sm.get("metrics").and_then(|v| v.as_array()) {
                        for m in metrics {
                            let name = m
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let unit = m.get("unit").and_then(|v| v.as_str()).map(String::from);

                            let dps = m
                                .get("sum")
                                .or_else(|| m.get("gauge"))
                                .and_then(|s| s.get("dataPoints"))
                                .and_then(|v| v.as_array());

                            if let Some(data_points) = dps {
                                for dp in data_points {
                                    let ts = dp
                                        .get("timeUnixNano")
                                        .or_else(|| dp.get("time_unix_nano"))
                                        .and_then(parse_json_u64);

                                    let val = dp
                                        .get("asInt")
                                        .or_else(|| dp.get("as_int"))
                                        .and_then(|v| {
                                            if let Some(i) = v.as_i64() {
                                                Some(i)
                                            } else if let Some(s) = v.as_str() {
                                                s.parse::<i64>().ok()
                                            } else {
                                                None
                                            }
                                        })
                                        .or_else(|| {
                                            dp.get("asDouble")
                                                .and_then(|v| v.as_f64())
                                                .map(|d| d as i64)
                                        })
                                        .unwrap_or(0);

                                    out.push(Input::CounterSample {
                                        source_ts_ns: ts,
                                        track_key: name.clone(),
                                        value: val,
                                        unit: unit.clone(),
                                    });
                                }
                            } else {
                                self.unknown_count += 1;
                            }
                        }
                    }
                }
            }
        }

        out
    }

    /// Decodes an OTLP JSON traces payload.
    pub fn decode_traces_json(&mut self, value: &serde_json::Value) -> Vec<Input> {
        let Some(obj) = value.as_object() else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let Some(res_spans) = obj.get("resourceSpans").and_then(|v| v.as_array()) else {
            self.unknown_count += 1;
            return Vec::new();
        };

        let mut out = Vec::new();
        for rs in res_spans {
            let service_name = rs
                .get("resource")
                .and_then(|r| r.get("attributes"))
                .and_then(get_json_attr_string("service.name"))
                .unwrap_or_else(|| "unknown".to_string());

            if let Some(scope_spans) = rs.get("scopeSpans").and_then(|v| v.as_array()) {
                for ss in scope_spans {
                    if let Some(spans) = ss.get("spans").and_then(|v| v.as_array()) {
                        for span in spans {
                            let name = span
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let span_id = span
                                .get("spanId")
                                .or_else(|| span.get("span_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();

                            let start_ts = span
                                .get("startTimeUnixNano")
                                .or_else(|| span.get("start_time_unix_nano"))
                                .and_then(parse_json_u64);

                            let end_ts = span
                                .get("endTimeUnixNano")
                                .or_else(|| span.get("end_time_unix_nano"))
                                .and_then(parse_json_u64);

                            let track = format!("custom:{}", service_name);

                            out.push(Input::Custom {
                                source_ts_ns: start_ts,
                                track: track.clone(),
                                op: CustomOp::Begin,
                                name: Some(name.clone()),
                                id: Some(span_id.clone()),
                                args: BTreeMap::new(),
                                value: None,
                                flow_out: None,
                                flow_in: None,
                            });

                            if end_ts.is_some() {
                                out.push(Input::Custom {
                                    source_ts_ns: end_ts,
                                    track,
                                    op: CustomOp::End,
                                    name: Some(name),
                                    id: Some(span_id),
                                    args: BTreeMap::new(),
                                    value: None,
                                    flow_out: None,
                                    flow_in: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        out
    }
}

fn get_attr_string(attrs: &[KeyValue], key: &str) -> Option<String> {
    for kv in attrs {
        if kv.key == key {
            if let Some(v) = &kv.value {
                return Some(any_value_to_string(v));
            }
        }
    }
    None
}

fn get_attr_i64(attrs: &[KeyValue], key: &str) -> Option<i64> {
    for kv in attrs {
        if kv.key == key {
            if let Some(v) = &kv.value {
                match &v.value {
                    Some(any_value::Value::IntValue(i)) => return Some(*i),
                    Some(any_value::Value::StringValue(s)) => return s.parse().ok(),
                    _ => {}
                }
            }
        }
    }
    None
}

fn any_value_to_string(val: &AnyValue) -> String {
    match &val.value {
        Some(any_value::Value::StringValue(s)) => s.clone(),
        Some(any_value::Value::BoolValue(b)) => b.to_string(),
        Some(any_value::Value::IntValue(i)) => i.to_string(),
        Some(any_value::Value::DoubleValue(d)) => d.to_string(),
        Some(any_value::Value::BytesValue(b)) => hex_encode(b),
        Some(any_value::Value::ArrayValue(arr)) => {
            let items: Vec<String> = arr.values.iter().map(any_value_to_string).collect();
            format!("[{}]", items.join(", "))
        }
        Some(any_value::Value::KvlistValue(kv)) => {
            let items: Vec<String> = kv
                .values
                .iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        k.key,
                        k.value
                            .as_ref()
                            .map(any_value_to_string)
                            .unwrap_or_default()
                    )
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        _ => String::new(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn get_json_attr_string(key: &'static str) -> impl Fn(&serde_json::Value) -> Option<String> {
    move |attrs| {
        if let Some(arr) = attrs.as_array() {
            for item in arr {
                if item.get("key").and_then(|v| v.as_str()) == Some(key) {
                    if let Some(v) = item.get("value") {
                        if let Some(s) = v.as_str() {
                            return Some(s.to_string());
                        }
                        if let Some(s) = v.get("stringValue").and_then(|x| x.as_str()) {
                            return Some(s.to_string());
                        }
                        // OTLP/JSON encodes booleans as `boolValue` and 64-bit ints as
                        // `intValue` (string or number); without these an `is_error`
                        // sent as a real bool was silently read as false.
                        if let Some(b) = v.get("boolValue").and_then(|x| x.as_bool()) {
                            return Some(b.to_string());
                        }
                        if let Some(i) = v.get("intValue") {
                            if let Some(s) = i.as_str() {
                                return Some(s.to_string());
                            }
                            if let Some(n) = i.as_i64() {
                                return Some(n.to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

fn parse_json_u64(val: &serde_json::Value) -> Option<u64> {
    if let Some(n) = val.as_u64() {
        Some(n)
    } else if let Some(s) = val.as_str() {
        s.parse::<u64>().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_otlp_json_logs_claude_prompt_and_tool() {
        let json = serde_json::json!({
            "resourceLogs": [{
                "resource": {
                    "attributes": [{ "key": "service.name", "value": { "stringValue": "claude-code" } }]
                },
                "scopeLogs": [{
                    "logRecords": [
                        {
                            "timeUnixNano": 1000000,
                            "attributes": [
                                { "key": "event.name", "value": { "stringValue": "claude_code.user_prompt" } },
                                { "key": "session_id", "value": { "stringValue": "sess-42" } }
                            ],
                            "body": { "stringValue": "Fix the bug in auth" }
                        },
                        {
                            "timeUnixNano": 2000000,
                            "attributes": [
                                { "key": "event.name", "value": { "stringValue": "claude_code.tool_result" } },
                                { "key": "session_id", "value": { "stringValue": "sess-42" } },
                                { "key": "tool_use_id", "value": { "stringValue": "tool_1" } },
                                { "key": "is_error", "value": { "stringValue": "false" } }
                            ],
                            "body": { "stringValue": "success" }
                        }
                    ]
                }]
            }]
        });

        let mut decoder = OtlpDecoder::new();
        let inputs = decoder.decode_logs_json(&json);

        assert_eq!(inputs.len(), 2);
        assert!(
            matches!(&inputs[0], Input::Prompt { session, text_len, .. } if session.session_id == "sess-42" && *text_len == 19)
        );
        assert!(
            matches!(&inputs[1], Input::ToolEnd { session, tool_use_id, error, .. } if session.session_id == "sess-42" && tool_use_id == "tool_1" && !error)
        );
    }

    #[test]
    fn test_otlp_json_metrics() {
        let json = serde_json::json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [
                        {
                            "name": "claude_code.cost.usage",
                            "unit": "USD",
                            "sum": {
                                "dataPoints": [{
                                    "timeUnixNano": 5000000,
                                    "asDouble": 12.5
                                }]
                            }
                        },
                        {
                            "name": "gemini_cli.cpu.usage",
                            "unit": "percent",
                            "gauge": {
                                "dataPoints": [{
                                    "timeUnixNano": 6000000,
                                    "asInt": 75
                                }]
                            }
                        }
                    ]
                }]
            }]
        });

        let mut decoder = OtlpDecoder::new();
        let inputs = decoder.decode_metrics_json(&json);

        assert_eq!(inputs.len(), 2);
        assert!(
            matches!(&inputs[0], Input::CounterSample { track_key, value, unit, .. } if track_key == "claude_code.cost.usage" && *value == 12 && unit.as_deref() == Some("USD"))
        );
        assert!(
            matches!(&inputs[1], Input::CounterSample { track_key, value, unit, .. } if track_key == "gemini_cli.cpu.usage" && *value == 75 && unit.as_deref() == Some("percent"))
        );
    }

    #[test]
    fn test_otlp_json_traces_custom_spans() {
        let json = serde_json::json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [{ "key": "service.name", "value": { "stringValue": "my-service" } }]
                },
                "scopeSpans": [{
                    "spans": [{
                        "name": "database-query",
                        "spanId": "span-123",
                        "startTimeUnixNano": 1000,
                        "endTimeUnixNano": 2000
                    }]
                }]
            }]
        });

        let mut decoder = OtlpDecoder::new();
        let inputs = decoder.decode_traces_json(&json);

        assert_eq!(inputs.len(), 2);
        assert!(
            matches!(&inputs[0], Input::Custom { track, op: CustomOp::Begin, name: Some(name), id: Some(id), .. } if track == "custom:my-service" && name == "database-query" && id == "span-123")
        );
        assert!(
            matches!(&inputs[1], Input::Custom { track, op: CustomOp::End, name: Some(name), id: Some(id), .. } if track == "custom:my-service" && name == "database-query" && id == "span-123")
        );
    }

    #[test]
    fn test_otlp_unknown_payloads_counted() {
        let mut decoder = OtlpDecoder::new();
        let _ = decoder.decode_logs_json(&serde_json::json!({ "invalid": true }));
        assert_eq!(decoder.unknown_count, 1);

        let _ = decoder.decode_metrics_json(&serde_json::json!([1, 2, 3]));
        assert_eq!(decoder.unknown_count, 2);

        let _ = decoder.decode_logs_proto(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(decoder.unknown_count, 3);
    }
}
