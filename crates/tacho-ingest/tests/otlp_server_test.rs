use flate2::write::GzEncoder;
use flate2::Compression;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tacho_core::Input;
use tacho_ingest::otlp_server::{
    serve_otlp_with_config, OtlpServerConfig, DEFAULT_MAX_CONCURRENT_CONNECTIONS,
};

struct TestServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    shutdown_tx: i32,
    timeout_tx: i32,
    collected: Arc<(Mutex<Vec<Input>>, Condvar)>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn wait_collected(&self, min_len: usize) -> Vec<Input> {
        let (lock, cvar) = &*self.collected;
        // The server invokes `on_inputs` before writing its response, so by the
        // time a test has read a 200 this returns at once; no timeout needed.
        let inputs = cvar
            .wait_while(lock.lock().unwrap(), |v| v.len() < min_len)
            .unwrap();
        inputs.clone()
    }

    fn expire_io(&mut self) {
        if self.timeout_tx >= 0 {
            unsafe { libc::close(self.timeout_tx) };
            self.timeout_tx = -1;
        }
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if self.shutdown_tx >= 0 {
            unsafe { libc::close(self.shutdown_tx) };
            self.shutdown_tx = -1;
        }
        self.expire_io();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_test_server() -> TestServer {
    start_test_server_with(OtlpServerConfig::default())
}

fn start_test_server_with(base: OtlpServerConfig) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let collected = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let collected_clone = Arc::clone(&collected);

    let mut shutdown_pipe = [0i32; 2];
    let mut timeout_pipe = [0i32; 2];
    unsafe {
        libc::pipe2(
            shutdown_pipe.as_mut_ptr(),
            libc::O_CLOEXEC | libc::O_NONBLOCK,
        );
        libc::pipe2(
            timeout_pipe.as_mut_ptr(),
            libc::O_CLOEXEC | libc::O_NONBLOCK,
        );
    }
    let shutdown_rx = shutdown_pipe[0];
    let shutdown_tx = shutdown_pipe[1];
    let timeout_rx = timeout_pipe[0];
    let timeout_tx = timeout_pipe[1];

    let config = OtlpServerConfig {
        shutdown_fd: Some(shutdown_rx),
        timeout_fd: Some(timeout_rx),
        ..base
    };

    let handle = thread::spawn(move || {
        let _ = serve_otlp_with_config(listener, shutdown_clone, config, move |inputs| {
            let (lock, cvar) = &*collected_clone;
            let mut l = lock.lock().unwrap();
            l.extend(inputs);
            cvar.notify_all();
        });
        if shutdown_rx >= 0 {
            unsafe { libc::close(shutdown_rx) };
        }
        if timeout_rx >= 0 {
            unsafe { libc::close(timeout_rx) };
        }
    });

    // The listener is bound before the server thread starts, so connects queue
    // in the backlog immediately; no readiness wait is needed.
    TestServer {
        port,
        shutdown,
        shutdown_tx,
        timeout_tx,
        collected,
        handle: Some(handle),
    }
}

#[test]
fn test_otlp_server_json_logs() {
    let mut server = start_test_server();

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
                        {"key": "session_id", "value": {"stringValue": "sess-otlp-json"}}
                    ],
                    "body": {"stringValue": "Fix the bug in main.rs"}
                }]
            }]
        }]
    })
    .to_string();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json_body.len(),
        json_body
    );

    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("{}"));

    let inputs = server.wait_collected(1);
    assert_eq!(inputs.len(), 1);
    match &inputs[0] {
        Input::Prompt {
            session, text_len, ..
        } => {
            assert_eq!(session.session_id, "sess-otlp-json");
            assert_eq!(*text_len, "Fix the bug in main.rs".len() as u64);
        }
        other => panic!("expected Prompt, got {:?}", other),
    }

    server.stop();
}

#[test]
fn test_otlp_server_proto_logs() {
    let mut server = start_test_server();

    let req_proto = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("claude-code".to_string())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: None,
                log_records: vec![LogRecord {
                    time_unix_nano: 1700000000000000000,
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(
                            "Edit main.rs to fix bug".to_string(),
                        )),
                    }),
                    attributes: vec![
                        KeyValue {
                            key: "event.name".to_string(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(
                                    "claude_code.user_prompt".to_string(),
                                )),
                            }),
                            ..Default::default()
                        },
                        KeyValue {
                            key: "session_id".to_string(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(
                                    "sess-proto".to_string(),
                                )),
                            }),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    let mut buf = Vec::new();
    req_proto.encode(&mut buf).unwrap();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    let req_header = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        buf.len()
    );

    stream.write_all(req_header.as_bytes()).unwrap();
    stream.write_all(&buf).unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    let inputs = server.wait_collected(1);
    assert_eq!(inputs.len(), 1);
    match &inputs[0] {
        Input::Prompt {
            session, text_len, ..
        } => {
            assert_eq!(session.session_id, "sess-proto");
            assert_eq!(*text_len, "Edit main.rs to fix bug".len() as u64);
        }
        other => panic!("expected Prompt, got {:?}", other),
    }

    server.stop();
}

#[test]
fn test_otlp_server_gzip() {
    let mut server = start_test_server();

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
                        {"key": "session_id", "value": {"stringValue": "sess-gzip"}}
                    ],
                    "body": {"stringValue": "Gzip compressed prompt"}
                }]
            }]
        }]
    })
    .to_string();

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(json_body.as_bytes()).unwrap();
    let gzip_bytes = encoder.finish().unwrap();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    let req_header = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        gzip_bytes.len()
    );

    stream.write_all(req_header.as_bytes()).unwrap();
    stream.write_all(&gzip_bytes).unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    let inputs = server.wait_collected(1);
    assert_eq!(inputs.len(), 1);
    match &inputs[0] {
        Input::Prompt { session, .. } => {
            assert_eq!(session.session_id, "sess-gzip");
        }
        other => panic!("expected Prompt, got {:?}", other),
    }

    server.stop();
}

#[test]
fn test_otlp_server_chunked() {
    let mut server = start_test_server();

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
                        {"key": "session_id", "value": {"stringValue": "sess-chunked"}}
                    ],
                    "body": {"stringValue": "Chunked message body"}
                }]
            }]
        }]
    })
    .to_string();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    let header = "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    stream.write_all(header.as_bytes()).unwrap();

    // Send body in chunks
    let bytes = json_body.as_bytes();
    let chunk1 = &bytes[..10];
    let chunk2 = &bytes[10..];

    write!(stream, "{:x}\r\n", chunk1.len()).unwrap();
    stream.write_all(chunk1).unwrap();
    stream.write_all(b"\r\n").unwrap();

    write!(stream, "{:x}\r\n", chunk2.len()).unwrap();
    stream.write_all(chunk2).unwrap();
    stream.write_all(b"\r\n").unwrap();

    // Final zero chunk
    stream.write_all(b"0\r\n\r\n").unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    let inputs = server.wait_collected(1);
    assert_eq!(inputs.len(), 1);
    match &inputs[0] {
        Input::Prompt { session, .. } => {
            assert_eq!(session.session_id, "sess-chunked");
        }
        other => panic!("expected Prompt, got {:?}", other),
    }

    server.stop();
}

#[test]
fn test_otlp_server_malformed_http() {
    let mut server = start_test_server();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream
        .write_all(b"NOT_A_VALID_HTTP_REQUEST\r\n\r\n")
        .unwrap();

    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 400 Bad Request"));

    server.stop();
}

#[test]
fn test_otlp_server_unknown_path() {
    let mut server = start_test_server();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream
        .write_all(b"POST /v1/unknown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 404 Not Found"));

    server.stop();
}

#[test]
fn test_otlp_server_oversized_body() {
    let mut server = start_test_server();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    // 17 MiB > 16 MiB max
    let header = "POST /v1/logs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 17825792\r\nConnection: close\r\n\r\n";
    stream.write_all(header.as_bytes()).unwrap();

    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large"));

    server.stop();
}

#[test]
fn test_otlp_server_slow_loris_timeout() {
    let mut server = start_test_server();

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    // Send partial header and stop, then deterministically expire the server's read wait.
    stream
        .write_all(b"POST /v1/logs HTTP/1.1\r\nHost: localhost\r\n")
        .unwrap();
    server.expire_io();

    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 408 Request Timeout"), "{resp}");

    server.stop();
}

#[test]
fn test_otlp_server_default_config_cap() {
    assert_eq!(DEFAULT_MAX_CONCURRENT_CONNECTIONS, 64);
    assert_eq!(OtlpServerConfig::default().max_concurrent_connections, 64);
}

#[test]
fn test_otlp_server_connection_cap_503_retry_after() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let mut shutdown_pipe = [0i32; 2];
    unsafe {
        libc::pipe2(
            shutdown_pipe.as_mut_ptr(),
            libc::O_CLOEXEC | libc::O_NONBLOCK,
        );
    }
    let shutdown_rx = shutdown_pipe[0];
    let shutdown_tx = shutdown_pipe[1];

    let config = OtlpServerConfig {
        max_concurrent_connections: 2,
        shutdown_fd: Some(shutdown_rx),
        ..Default::default()
    };

    let handle = thread::spawn(move || {
        let _ = serve_otlp_with_config(listener, shutdown_clone, config, |_| {});
        if shutdown_rx >= 0 {
            unsafe { libc::close(shutdown_rx) };
        }
    });

    // Connect 2 clients with HTTP keep-alive to hold both connection slots active
    let req_keepalive = "POST /v1/logs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n{}";

    let mut stream1 = TcpStream::connect(("127.0.0.1", port)).expect("connect 1");
    stream1.write_all(req_keepalive.as_bytes()).unwrap();
    let mut resp1_buf = [0u8; 128];
    let n1 = stream1.read(&mut resp1_buf).unwrap();
    assert!(std::str::from_utf8(&resp1_buf[..n1])
        .unwrap()
        .starts_with("HTTP/1.1 200 OK"));

    let mut stream2 = TcpStream::connect(("127.0.0.1", port)).expect("connect 2");
    stream2.write_all(req_keepalive.as_bytes()).unwrap();
    let mut resp2_buf = [0u8; 128];
    let n2 = stream2.read(&mut resp2_buf).unwrap();
    assert!(std::str::from_utf8(&resp2_buf[..n2])
        .unwrap()
        .starts_with("HTTP/1.1 200 OK"));

    // 3rd client exceeds capacity (max 2) -> server immediately responds 503 Retry-After: 1 and closes
    let mut stream3 = TcpStream::connect(("127.0.0.1", port)).expect("connect 3");
    let mut resp3 = String::new();
    stream3.read_to_string(&mut resp3).unwrap();

    assert!(
        resp3.starts_with("HTTP/1.1 503 Service Unavailable"),
        "got: {}",
        resp3
    );
    assert!(resp3.contains("Retry-After: 1"), "got: {}", resp3);
    assert!(resp3.contains("Connection: close"), "got: {}", resp3);

    // Now close stream1 so active connection count drops back below 2
    drop(stream1);

    // 4th client: the slot frees when stream1's handler observes EOF, which
    // races with this connect. Each attempt ends in a real response (503 while
    // the slot is held, then 200), so retry on 503 only; any other reply fails.
    let req4 = "POST /v1/logs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    loop {
        let mut stream4 = TcpStream::connect(("127.0.0.1", port)).expect("connect 4");
        let _ = stream4.write_all(req4.as_bytes());
        let mut resp4 = String::new();
        let _ = stream4.read_to_string(&mut resp4);
        if resp4.starts_with("HTTP/1.1 200 OK") {
            break;
        }
        // An empty read means the 503 was lost to an RST (we wrote a request the
        // rejecting server never read), which is still a rejection event.
        assert!(
            resp4.is_empty() || resp4.starts_with("HTTP/1.1 503"),
            "unexpected reply: {resp4}"
        );
        thread::yield_now();
    }

    drop(stream2);
    shutdown.store(true, Ordering::Relaxed);
    if shutdown_tx >= 0 {
        unsafe { libc::close(shutdown_tx) };
    }
    let _ = handle.join();
}

/// Sends raw bytes on a fresh connection and returns everything the server
/// writes before closing.
fn raw_exchange(port: u16, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let _ = stream.write_all(bytes);
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    String::from_utf8_lossy(&resp).into_owned()
}

fn prompt_json(session: &str) -> String {
    serde_json::json!({
        "resourceLogs": [{"scopeLogs": [{"logRecords": [{
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "claude_code.user_prompt"}},
                {"key": "session_id", "value": {"stringValue": session}}
            ],
            "body": {"stringValue": "hi"}
        }]}]}]
    })
    .to_string()
}

#[test]
fn test_otlp_server_rejects_request_smuggling() {
    let server = start_test_server();
    let body = prompt_json("smuggled");
    // CL+TE: framing is ambiguous; must refuse and close, not guess.
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nTransfer-Encoding: chunked\r\n\r\n{}",
        body.len(),
        body
    );
    let resp = raw_exchange(server.port, req.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(resp.contains("Connection: close"), "{resp}");
    assert_eq!(resp.matches("HTTP/1.1").count(), 1, "{resp}");

    // Conflicting Content-Length values.
    let req = "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n{}";
    assert!(raw_exchange(server.port, req.as_bytes()).starts_with("HTTP/1.1 400"));
}

#[test]
fn test_otlp_server_error_before_body_closes_instead_of_desyncing() {
    // Before the fix a 415 kept the connection alive without consuming the
    // body, so the body bytes were parsed as the next request.
    let server = start_test_server();
    let inner =
        "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        inner.len(),
        inner
    );
    let resp = raw_exchange(server.port, req.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 415"), "{resp}");
    assert_eq!(resp.matches("HTTP/1.1").count(), 1, "{resp}");
}

#[test]
fn test_otlp_server_pipelined_keep_alive_with_bad_json_in_middle() {
    let server = start_test_server();
    let a = prompt_json("pipe-a");
    let b = prompt_json("pipe-b");
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}\
         POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n{{{{x\
         POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        a.len(),
        a,
        b.len(),
        b
    );
    let resp = raw_exchange(server.port, req.as_bytes());
    let statuses: Vec<&str> = resp
        .match_indices("HTTP/1.1 ")
        .map(|(i, _)| &resp[i + 9..i + 12])
        .collect();
    assert_eq!(statuses, ["200", "400", "200"], "{resp}");
    let ids: Vec<String> = server
        .wait_collected(2)
        .iter()
        .filter_map(|i| match i {
            Input::Prompt { session, .. } => Some(session.session_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["pipe-a", "pipe-b"]);
}

#[test]
fn test_otlp_server_gzip_bomb_is_413() {
    let server = start_test_server_with(OtlpServerConfig {
        max_body_size: 64 * 1024,
        ..Default::default()
    });
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    enc.write_all(&vec![b' '; 4 * 1024 * 1024]).unwrap();
    let gz = enc.finish().unwrap();
    assert!(gz.len() < 64 * 1024);
    let mut req = format!(
        "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        gz.len()
    )
    .into_bytes();
    req.extend_from_slice(&gz);
    let resp = raw_exchange(server.port, &req);
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
}

#[test]
fn test_otlp_server_hostile_chunked_bodies() {
    let server = start_test_server();
    let head = "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
    let cases: &[(&str, &str)] = &[
        // usize overflow in the size line used to panic (debug) / wrap.
        ("ffffffffffffffffffff\r\n", "413"),
        ("ffffffffffffffff\r\n", "413"),
        // Negative / garbage sizes.
        ("-1\r\n", "400"),
        ("zz\r\n\r\n", "400"),
        // Chunk data not followed by CRLF.
        ("2\r\n{}XX0\r\n\r\n", "400"),
        // Endless size line with no newline.
        (&"1".repeat(10_000), "400"),
        // Truncated: EOF in chunk data (we shut down the write side).
        ("10\r\nabc", "400"),
    ];
    for (body, want) in cases {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut resp = String::new();
        let _ = stream.read_to_string(&mut resp);
        assert!(
            resp.starts_with(&format!("HTTP/1.1 {want}")),
            "body {:?}: {resp}",
            &body[..body.len().min(40)]
        );
    }
}

#[test]
fn test_otlp_server_oversized_and_garbage_heads() {
    let server = start_test_server();
    let mut huge = b"POST /v1/logs HTTP/1.1\r\nX-Pad: ".to_vec();
    huge.extend(std::iter::repeat_n(b'a', 128 * 1024));
    let resp = raw_exchange(server.port, &huge);
    assert!(
        resp.starts_with("HTTP/1.1 400"),
        "{}",
        &resp[..resp.len().min(80)]
    );

    for garbage in [
        &b"\xff\xfe\r\n\r\n"[..],
        b"POST /v1/logs\r\n\r\n",
        b"POST /v1/logs HTTP/1.1 extra\r\n\r\n",
        b"POST /v1/logs SPDY/3\r\n\r\n",
        b"POST /v1/logs HTTP/1.1\r\nno-colon-here\r\n\r\n",
        b"\r\n\r\n",
    ] {
        let resp = raw_exchange(server.port, garbage);
        assert!(resp.starts_with("HTTP/1.1 400"), "{garbage:?}: {resp}");
    }
}

#[test]
fn test_otlp_server_panicking_callback_frees_slot() {
    // With one slot, a handler panic that leaked the slot would make every
    // later client get 503.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let config = OtlpServerConfig {
        max_concurrent_connections: 1,
        shutdown_fd: Some(fds[0]),
        ..Default::default()
    };
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls2 = Arc::clone(&calls);
    let sd = Arc::clone(&shutdown);
    let handle = thread::spawn(move || {
        serve_otlp_with_config(listener, sd, config, move |_| {
            if calls2.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("callback panic (expected in test)");
            }
        })
    });

    let body = prompt_json("boom");
    let req = format!(
        "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    // First request: handler panics, socket closes with no response.
    assert_eq!(raw_exchange(port, req.as_bytes()), "");
    // The slot is freed by the guard's Drop during unwinding, which may race
    // with this connect; retry only on 503 / RST.
    loop {
        let resp = raw_exchange(port, req.as_bytes());
        if resp.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(
            resp.is_empty() || resp.starts_with("HTTP/1.1 503"),
            "{resp}"
        );
        thread::yield_now();
    }

    unsafe { libc::close(fds[1]) };
    handle.join().unwrap().unwrap();
    unsafe { libc::close(fds[0]) };
}

#[test]
fn test_otlp_json_bool_is_error_and_huge_exit_code() {
    use tacho_ingest::otlp::OtlpDecoder;
    let v = serde_json::json!({
        "resourceLogs": [{"scopeLogs": [{"logRecords": [{
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "claude_code.tool_result"}},
                {"key": "is_error", "value": {"boolValue": true}}
            ]
        }]}]}]
    });
    let inputs = OtlpDecoder::new().decode_logs_json(&v);
    assert!(
        matches!(&inputs[..], [Input::ToolEnd { error: true, .. }]),
        "{inputs:?}"
    );

    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    attributes: vec![
                        KeyValue {
                            key: "event.name".into(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(
                                    "claude_code.tool_result".into(),
                                )),
                            }),
                            ..Default::default()
                        },
                        KeyValue {
                            key: "exit_code".into(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::IntValue(1 << 32)),
                            }),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let inputs = OtlpDecoder::new().decode_logs_proto(&req.encode_to_vec());
    // 1<<32 used to wrap to exit code 0 (a fake success).
    assert!(
        matches!(
            &inputs[..],
            [Input::ToolEnd {
                exit_code: None,
                ..
            }]
        ),
        "{inputs:?}"
    );
}
