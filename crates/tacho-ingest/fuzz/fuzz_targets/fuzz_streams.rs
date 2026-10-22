#![no_main]
use libfuzzer_sys::fuzz_target;
use tacho_ingest::streams::{detect, StreamKind, StreamParser};

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = detect(&[s]);
        let mut p1 = StreamParser::for_kind(StreamKind::Claude);
        let _ = p1.feed_line(s, 1000);
        let mut p2 = StreamParser::for_kind(StreamKind::Gemini);
        let _ = p2.feed_line(s, 1000);
        let mut p3 = StreamParser::for_kind(StreamKind::Codex);
        let _ = p3.feed_line(s, 1000);
    }
});
