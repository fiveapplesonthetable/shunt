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
use std::time::{Duration, Instant};
use tacho_core::Input;
use tacho_ingest::otlp_server::{
    serve_otlp_with_config, OtlpServerConfig, DEFAULT_MAX_CONCURRENT_CONNECTIONS,
};

struct TestServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    shutdown_tx: i32,
    collected: Arc<(Mutex<Vec<Input>>, Condvar)>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn wait_collected(&self, min_len: usize) -> Vec<Input> {
        let (lock, cvar) = &*self.collected;
        let mut inputs = lock.lock().unwrap();
        let cap = Instant::now() + Duration::from_secs(5);
        while inputs.len() < min_len && Instant::now() < cap {
            let (next, _) = cvar
                .wait_timeout(inputs, Duration::from_millis(50))
                .unwrap();
            inputs = next;
        }
        inputs.clone()
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if self.shutdown_tx >= 0 {
            unsafe { libc::close(self.shutdown_tx) };
            self.shutdown_tx = -1;
        }
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let collected = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let collected_clone = Arc::clone(&collected);

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
        shutdown_fd: Some(shutdown_rx),
        ..Default::default()
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
    });

    let cap = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_err() && Instant::now() < cap {
        thread::yield_now();
    }

    TestServer {
        port,
        shutdown,
        shutdown_tx,
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
    // Send partial header and stop
    stream
        .write_all(b"POST /v1/logs HTTP/1.1\r\nHost: localhost\r\n")
        .unwrap();

    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 400 Bad Request"));

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

    // 4th client connects now that a slot is free -> should succeed
    let cap = Instant::now() + Duration::from_secs(5);
    let mut stream4_succeeded = false;
    let req4 = "POST /v1/logs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    while Instant::now() < cap {
        if let Ok(mut stream4) = TcpStream::connect(("127.0.0.1", port)) {
            if stream4.write_all(req4.as_bytes()).is_ok() {
                let mut resp4 = String::new();
                if stream4.read_to_string(&mut resp4).is_ok()
                    && resp4.starts_with("HTTP/1.1 200 OK")
                {
                    stream4_succeeded = true;
                    break;
                }
            }
        }
        thread::yield_now();
    }
    assert!(
        stream4_succeeded,
        "stream4 must succeed once stream1 is dropped"
    );

    drop(stream2);
    shutdown.store(true, Ordering::Relaxed);
    if shutdown_tx >= 0 {
        unsafe { libc::close(shutdown_tx) };
    }
    let _ = handle.join();
}
