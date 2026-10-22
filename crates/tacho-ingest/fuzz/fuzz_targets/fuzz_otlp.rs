#![no_main]
use libfuzzer_sys::fuzz_target;
use tacho_ingest::otlp::OtlpDecoder;

fuzz_target!(|data: &[u8]| {
    let mut otlp = OtlpDecoder::new();
    let _ = otlp.decode_logs_proto(data);
    let _ = otlp.decode_metrics_proto(data);
    let _ = otlp.decode_traces_proto(data);

    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
            let _ = otlp.decode_logs_json(&val);
            let _ = otlp.decode_metrics_json(&val);
            let _ = otlp.decode_traces_json(&val);
        }
    }
});
