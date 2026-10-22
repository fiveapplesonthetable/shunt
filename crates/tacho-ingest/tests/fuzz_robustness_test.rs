use proptest::prelude::*;
use tacho_ingest::emit::parse_ndjson_line;
use tacho_ingest::hooks::HookEnv;
use tacho_ingest::otlp::OtlpDecoder;
use tacho_ingest::streams::{detect, StreamKind, StreamParser};

#[test]
fn test_malformed_and_truncated_inputs_never_panic() {
    let env = HookEnv::new(100_000);

    let malformed_inputs = [
        "",
        "{",
        "}",
        "[]",
        "\"just a string\"",
        "42",
        "{\"type\": ",
        "{\"session_id\": \"test\", \"hook_event_name\": ",
        "{\"type\": \"assistant\", \"message\": {\"content\": [{\"type\": \"tool_use\"",
        "\0\0\0\0",
        "\r\n\t   \n",
        "{\"session_id\": 12345, \"tool_call\": true, \"tool_input\": [1,2,3]}",
        "{\"conversationId\": null, \"workspacePaths\": 42}",
        "{\"type\": \"custom_unknown_kind_404\"}",
    ];

    for input in malformed_inputs {
        // Hooks
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
            let _ = tacho_ingest::hooks::claude::map(&val, &env);
            let _ = tacho_ingest::hooks::gemini::map(&val, &env);
            let _ = tacho_ingest::hooks::jetski::map(&val, &env);
        }

        // Streams
        let _ = detect(&[input]);
        let mut claude_parser = StreamParser::for_kind(StreamKind::Claude);
        let _ = claude_parser.feed_line(input, 100);
        let mut gemini_parser = StreamParser::for_kind(StreamKind::Gemini);
        let _ = gemini_parser.feed_line(input, 100);
        let mut codex_parser = StreamParser::for_kind(StreamKind::Codex);
        let _ = codex_parser.feed_line(input, 100);

        // Emit
        let _ = parse_ndjson_line(input);

        // OTLP
        let mut otlp = OtlpDecoder::new();
        let _ = otlp.decode_logs_proto(input.as_bytes());
        let _ = otlp.decode_metrics_proto(input.as_bytes());
        let _ = otlp.decode_traces_proto(input.as_bytes());
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
            let _ = otlp.decode_logs_json(&val);
            let _ = otlp.decode_metrics_json(&val);
            let _ = otlp.decode_traces_json(&val);
        }
    }
}

#[test]
fn test_huge_10mb_payload_never_panics() {
    let env = HookEnv::new(200_000);

    // 10 MB payload
    let huge_content = "x".repeat(10 * 1024 * 1024);
    let huge_json = format!(
        r#"{{"session_id": "sess-huge", "hook_event_name": "UserPromptSubmit", "prompt": "{}"}}"#,
        huge_content
    );

    let val: serde_json::Value = serde_json::from_str(&huge_json).expect("parse huge json");
    let inputs = tacho_ingest::hooks::claude::map(&val, &env);
    assert_eq!(inputs.len(), 2);

    let mut parser = StreamParser::for_kind(StreamKind::Claude);
    let stream_line = format!(
        r#"{{"type": "assistant", "session_id": "sess-huge", "message": {{"content": [{{"type": "text", "text": "{}"}}]}}}}"#,
        "y".repeat(10 * 1024 * 1024)
    );
    let stream_inputs = parser.feed_line(&stream_line, 300_000);
    assert!(!stream_inputs.is_empty());
}

// ---------------------------------------------------------------------------
// Proptests: Arbitrary Bytes and JSON
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn proptest_emit_ndjson_never_panics(s in ".*") {
        let _ = parse_ndjson_line(&s);
    }

    #[test]
    fn proptest_streams_feed_line_never_panics(s in ".*", ts in any::<u64>()) {
        let _ = detect(&[&s]);
        let mut p1 = StreamParser::for_kind(StreamKind::Claude);
        let _ = p1.feed_line(&s, ts);
        let mut p2 = StreamParser::for_kind(StreamKind::Gemini);
        let _ = p2.feed_line(&s, ts);
        let mut p3 = StreamParser::for_kind(StreamKind::Codex);
        let _ = p3.feed_line(&s, ts);
    }

    #[test]
    fn proptest_otlp_proto_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut otlp = OtlpDecoder::new();
        let _ = otlp.decode_logs_proto(&bytes);
        let _ = otlp.decode_metrics_proto(&bytes);
        let _ = otlp.decode_traces_proto(&bytes);
    }
}
