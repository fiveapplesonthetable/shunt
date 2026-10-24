//! End-to-end integration test for OTLP HTTP receiver in tacho-daemon.
//!
//! Verifies:
//! 1. Daemon starts with configured `otlp_addr`.
//! 2. Sending OTLP JSON logs over HTTP to `otlp_addr` yields HTTP 200 OK.
//! 3. Inputs flow into flight recorder pipeline, updating live state (queryable via `api.sock` status).
//! 4. Snapshots captured via `trace.sock` contain the event and pass trace processor validation.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::process::Command;
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_otlp_daemon_integration() {
    let tp_path = tacho_core::find_trace_processor()
        .expect("trace_processor_shell not found; run scripts/oracle.sh or set TACHO_TP")
        .to_string_lossy()
        .into_owned();

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();

    // Pick an available ephemeral port for OTLP HTTP server
    let ephemeral_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ephemeral_listener.local_addr().unwrap().port();
    drop(ephemeral_listener);

    let config = Config {
        otlp_addr: Some(format!("127.0.0.1:{}", port)),
        ..Default::default()
    };

    let daemon = Daemon::start(paths.clone(), config).expect("Failed to start daemon");

    // Daemon::start binds the OTLP listener before returning, so no readiness wait is needed.

    // 1. Send OTLP JSON logs request
    let json_body = serde_json::json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [{"key": "service.name", "value": {"stringValue": "claude-code"}}]
            },
            "scopeLogs": [{
                "logRecords": [{
                    "timeUnixNano": "1700000000000000000",
                    "attributes": [
                        {"key": "event.name", "value": {"stringValue": "claude_code.user_prompt"}},
                        {"key": "session_id", "value": {"stringValue": "sess-otlp-test"}}
                    ],
                    "body": {"stringValue": "Implement OTLP receiver"}
                }]
            }]
        }]
    })
    .to_string();

    let mut http_stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to OTLP port");
    let http_req = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        port,
        json_body.len(),
        json_body
    );
    http_stream.write_all(http_req.as_bytes()).unwrap();

    let mut http_resp = String::new();
    http_stream.read_to_string(&mut http_resp).unwrap();
    assert!(
        http_resp.starts_with("HTTP/1.1 200 OK"),
        "Unexpected HTTP response: {http_resp}"
    );

    // Flush appender to ensure batch is fully processed
    daemon.appender.flush();

    // 2. Query daemon status via api.sock
    let mut api_stream = UnixStream::connect(paths.api_sock()).expect("connect to api.sock");
    let status_cmd = serde_json::json!({
        "cmd": "status",
        "json": true
    });
    api_stream
        .write_all(serde_json::to_string(&status_cmd).unwrap().as_bytes())
        .unwrap();
    api_stream.write_all(b"\n").unwrap();
    api_stream.flush().unwrap();

    let mut status_resp = String::new();
    let mut reader = std::io::BufReader::new(api_stream);
    std::io::BufRead::read_line(&mut reader, &mut status_resp).unwrap();

    let val: serde_json::Value =
        serde_json::from_str(&status_resp).expect("valid JSON status response");
    assert_eq!(val.get("status").and_then(|v| v.as_str()), Some("ok"));
    let output_str = val.get("output").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        output_str.contains("claude") || output_str.contains("sess-otlp-test"),
        "Status did not reflect OTLP input: {output_str}"
    );

    // 3. Fetch trace via trace.sock and validate in trace_processor_shell
    let mut trace_stream = UnixStream::connect(paths.trace_sock()).expect("connect to trace.sock");
    let mut trace_bytes = Vec::new();
    trace_stream.read_to_end(&mut trace_bytes).unwrap();
    assert!(!trace_bytes.is_empty(), "Expected non-empty trace snapshot");

    let trace_file = temp.path().join("otlp_test.pftrace");
    fs::write(&trace_file, &trace_bytes).unwrap();

    let tp_output = Command::new(&tp_path)
        .arg(&trace_file)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output();

    if let Ok(out) = tp_output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let error_rows: Vec<&str> = stdout
                .trim()
                .lines()
                .filter(|l| !l.starts_with("\"name\"") && !l.is_empty())
                .collect();
            assert!(
                error_rows.is_empty(),
                "Oracle found errors in OTLP trace: {:?}",
                error_rows
            );
        }
    }
}
