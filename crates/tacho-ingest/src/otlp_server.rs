use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tacho_core::Input;

pub const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 64;
pub const DEFAULT_MAX_BODY_SIZE: usize = 16 * 1024 * 1024; // 16 MiB
pub const DEFAULT_MAX_HEADER_SIZE: usize = 64 * 1024; // 64 KiB
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Configuration for the OTLP server.
#[derive(Debug, Clone)]
pub struct OtlpServerConfig {
    /// Maximum concurrent connections handled concurrently (default: 64).
    pub max_concurrent_connections: usize,
    /// Maximum body size in bytes (default: 16 MiB).
    pub max_body_size: usize,
    /// Maximum header size in bytes (default: 64 KiB).
    pub max_header_size: usize,
    /// Read/write I/O timeout (default: 2 seconds).
    pub io_timeout: Duration,
    /// Optional shutdown file descriptor (e.g. read end of a pipe).
    /// When provided, the accept loop blocks indefinitely on poll until a connection
    /// arrives or shutdown_fd is signaled/closed, eliminating timed poll loops.
    pub shutdown_fd: Option<std::os::unix::io::RawFd>,
}

impl Default for OtlpServerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_connections: DEFAULT_MAX_CONCURRENT_CONNECTIONS,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            io_timeout: DEFAULT_IO_TIMEOUT,
            shutdown_fd: None,
        }
    }
}

/// Runs a blocking HTTP/1.1 OTLP receiver on the given `TcpListener` with default configuration.
///
/// Listens for `POST /v1/logs`, `POST /v1/metrics`, and `POST /v1/traces`
/// payloads in Protobuf or JSON format, optionally gzip-compressed.
///
/// Runs until `shutdown` is set to `true`.
pub fn serve_otlp(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    on_inputs: impl Fn(Vec<Input>) + Send + Sync + 'static,
) -> io::Result<()> {
    serve_otlp_with_config(listener, shutdown, OtlpServerConfig::default(), on_inputs)
}

/// Runs a blocking HTTP/1.1 OTLP receiver on the given `TcpListener` with custom configuration.
pub fn serve_otlp_with_config(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    config: OtlpServerConfig,
    on_inputs: impl Fn(Vec<Input>) + Send + Sync + 'static,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let on_inputs = Arc::new(on_inputs);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let config = Arc::new(config);

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let current = active_connections.fetch_add(1, Ordering::SeqCst);
                if current >= config.max_concurrent_connections {
                    active_connections.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = stream.flush();
                    continue;
                }

                let on_inputs_clone = Arc::clone(&on_inputs);
                let active_clone = Arc::clone(&active_connections);
                let config_clone = Arc::clone(&config);
                std::thread::spawn(move || {
                    let _ = handle_client(stream, &config_clone, on_inputs_clone);
                    active_clone.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                use std::os::unix::io::AsRawFd;
                if let Some(sfd) = config.shutdown_fd {
                    let mut pfds = [
                        libc::pollfd {
                            fd: listener.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: sfd,
                            events: libc::POLLIN | libc::POLLHUP,
                            revents: 0,
                        },
                    ];
                    let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
                    if ret < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(err);
                    }
                    if (pfds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                        break;
                    }
                } else {
                    let mut pfd = libc::pollfd {
                        fd: listener.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    unsafe {
                        let _ = libc::poll(&mut pfd, 1, 100);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    config: &OtlpServerConfig,
    on_inputs: Arc<dyn Fn(Vec<Input>) + Send + Sync>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(config.io_timeout))?;
    stream.set_write_timeout(Some(config.io_timeout))?;

    let mut read_buf = Vec::new();

    loop {
        let (header_bytes, initial_body) =
            match read_headers(&mut stream, &mut read_buf, config.max_header_size) {
                Ok(Some((hdr, rem))) => (hdr, rem),
                Ok(None) => break, // clean EOF
                Err(_) => {
                    let _ =
                        send_response(&mut stream, 400, "Bad Request", "malformed request", false);
                    break;
                }
            };

        let header_str = match std::str::from_utf8(&header_bytes) {
            Ok(s) => s,
            Err(_) => {
                let _ = send_response(&mut stream, 400, "Bad Request", "non-utf8 headers", false);
                break;
            }
        };

        let mut lines = header_str.split("\r\n");
        let request_line = match lines.next() {
            Some(line) => line,
            None => {
                let _ = send_response(&mut stream, 400, "Bad Request", "empty request line", false);
                break;
            }
        };

        let req_parts: Vec<&str> = request_line.split_whitespace().collect();
        if req_parts.len() < 3 {
            let _ = send_response(
                &mut stream,
                400,
                "Bad Request",
                "invalid request line",
                false,
            );
            break;
        }

        let method = req_parts[0];
        let path = req_parts[1];
        let version = req_parts[2];

        let mut headers = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }

        let keep_alive = if let Some(conn) = headers.get("connection") {
            conn.to_lowercase() != "close"
        } else {
            version.contains("1.1")
        };

        let clean_path = path.split('?').next().unwrap_or(path).trim_end_matches('/');

        if method != "POST" {
            let _ = send_response(
                &mut stream,
                405,
                "Method Not Allowed",
                "only POST is allowed",
                false,
            );
            break;
        }

        if clean_path != "/v1/logs" && clean_path != "/v1/metrics" && clean_path != "/v1/traces" {
            let _ = send_response(&mut stream, 404, "Not Found", "endpoint not found", false);
            break;
        }

        let content_type = headers
            .get("content-type")
            .cloned()
            .unwrap_or_default()
            .to_lowercase();
        let is_proto = content_type.contains("application/x-protobuf");
        let is_json = content_type.contains("application/json");

        if !is_proto && !is_json {
            let _ = send_response(
                &mut stream,
                415,
                "Unsupported Media Type",
                "content-type must be application/x-protobuf or application/json",
                keep_alive,
            );
            if !keep_alive {
                break;
            }
            continue;
        }

        let is_gzip = if let Some(enc) = headers.get("content-encoding") {
            let lower = enc.to_lowercase();
            if lower == "gzip" {
                true
            } else if lower.is_empty() || lower == "identity" {
                false
            } else {
                let _ = send_response(
                    &mut stream,
                    415,
                    "Unsupported Media Type",
                    "unsupported content-encoding",
                    keep_alive,
                );
                if !keep_alive {
                    break;
                }
                continue;
            }
        } else {
            false
        };

        let is_chunked = headers
            .get("transfer-encoding")
            .map(|v| v.to_lowercase().contains("chunked"))
            .unwrap_or(false);

        let content_length = if let Some(cl) = headers.get("content-length") {
            match cl.parse::<usize>() {
                Ok(len) => {
                    if len > config.max_body_size {
                        let _ = send_response(
                            &mut stream,
                            413,
                            "Payload Too Large",
                            "body exceeds max allowed size",
                            false,
                        );
                        break;
                    }
                    Some(len)
                }
                Err(_) => {
                    let _ = send_response(
                        &mut stream,
                        400,
                        "Bad Request",
                        "invalid content-length",
                        keep_alive,
                    );
                    if !keep_alive {
                        break;
                    }
                    continue;
                }
            }
        } else {
            None
        };

        // Handle Expect: 100-continue before reading body
        if headers
            .get("expect")
            .map(|v| v.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
            && (stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err()
                || stream.flush().is_err())
        {
            break;
        }

        let (body, remainder) = if is_chunked {
            match read_chunked_body(&mut stream, initial_body, config.max_body_size) {
                Ok(pair) => pair,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    let _ = send_response(
                        &mut stream,
                        413,
                        "Payload Too Large",
                        "chunked body exceeds max allowed size",
                        false,
                    );
                    break;
                }
                Err(_) => {
                    let _ = send_response(
                        &mut stream,
                        400,
                        "Bad Request",
                        "malformed chunked body",
                        false,
                    );
                    break;
                }
            }
        } else if let Some(len) = content_length {
            match read_fixed_body(&mut stream, initial_body, len) {
                Ok(pair) => pair,
                Err(_) => {
                    let _ = send_response(
                        &mut stream,
                        400,
                        "Bad Request",
                        "unexpected EOF reading body",
                        false,
                    );
                    break;
                }
            }
        } else {
            (Vec::new(), initial_body)
        };

        // Pipelined remainder saved for next request
        read_buf = remainder;

        let raw_payload = if is_gzip {
            let mut gz = flate2::read::GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            let mut limited = (&mut gz).take((config.max_body_size + 1) as u64);
            if limited.read_to_end(&mut decompressed).is_err() {
                let _ = send_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    "failed to decompress gzip body",
                    keep_alive,
                );
                if !keep_alive {
                    break;
                }
                continue;
            }
            if decompressed.len() > config.max_body_size {
                let _ = send_response(
                    &mut stream,
                    413,
                    "Payload Too Large",
                    "decompressed body exceeds max allowed size",
                    false,
                );
                break;
            }
            decompressed
        } else {
            body
        };

        let mut decoder = crate::otlp::OtlpDecoder::new();
        let inputs = match clean_path {
            "/v1/logs" => {
                if is_proto {
                    decoder.decode_logs_proto(&raw_payload)
                } else {
                    match serde_json::from_slice(&raw_payload) {
                        Ok(v) => decoder.decode_logs_json(&v),
                        Err(_) => {
                            let _ = send_response(
                                &mut stream,
                                400,
                                "Bad Request",
                                "invalid JSON body",
                                keep_alive,
                            );
                            if !keep_alive {
                                break;
                            }
                            continue;
                        }
                    }
                }
            }
            "/v1/metrics" => {
                if is_proto {
                    decoder.decode_metrics_proto(&raw_payload)
                } else {
                    match serde_json::from_slice(&raw_payload) {
                        Ok(v) => decoder.decode_metrics_json(&v),
                        Err(_) => {
                            let _ = send_response(
                                &mut stream,
                                400,
                                "Bad Request",
                                "invalid JSON body",
                                keep_alive,
                            );
                            if !keep_alive {
                                break;
                            }
                            continue;
                        }
                    }
                }
            }
            "/v1/traces" => {
                if is_proto {
                    decoder.decode_traces_proto(&raw_payload)
                } else {
                    match serde_json::from_slice(&raw_payload) {
                        Ok(v) => decoder.decode_traces_json(&v),
                        Err(_) => {
                            let _ = send_response(
                                &mut stream,
                                400,
                                "Bad Request",
                                "invalid JSON body",
                                keep_alive,
                            );
                            if !keep_alive {
                                break;
                            }
                            continue;
                        }
                    }
                }
            }
            _ => unreachable!(),
        };

        if !inputs.is_empty() {
            on_inputs(inputs);
        }

        let conn_hdr = if keep_alive { "keep-alive" } else { "close" };
        let resp_res = if is_proto {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: {}\r\n\r\n",
                conn_hdr
            )
        } else {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: {}\r\n\r\n{{}}",
                conn_hdr
            )
        };

        if resp_res.and_then(|_| stream.flush()).is_err() || !keep_alive {
            break;
        }
    }

    Ok(())
}

fn read_headers(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    max_header_size: usize,
) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut temp = [0u8; 1024];
    loop {
        if let Some(pos) = find_subsequence(buffer, b"\r\n\r\n") {
            let header_bytes = buffer[..pos].to_vec();
            let initial_body = buffer[pos + 4..].to_vec();
            buffer.clear();
            return Ok(Some((header_bytes, initial_body)));
        }

        if buffer.len() > max_header_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers exceed max size",
            ));
        }

        let n = stream.read(&mut temp)?;
        if n == 0 {
            if buffer.is_empty() {
                return Ok(None);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF in headers",
                ));
            }
        }
        buffer.extend_from_slice(&temp[..n]);
    }
}

fn read_fixed_body(
    stream: &mut TcpStream,
    mut initial: Vec<u8>,
    target_len: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut temp = [0u8; 8192];
    while initial.len() < target_len {
        let to_read = (target_len - initial.len()).min(temp.len());
        let n = stream.read(&mut temp[..to_read])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete body",
            ));
        }
        initial.extend_from_slice(&temp[..n]);
    }

    let remainder = initial.drain(target_len..).collect();
    Ok((initial, remainder))
}

fn read_chunked_body(
    stream: &mut TcpStream,
    mut buf: Vec<u8>,
    max_body_size: usize,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut body = Vec::new();
    let mut temp = [0u8; 1024];

    loop {
        while !buf.contains(&b'\n') {
            let n = stream.read(&mut temp)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF in chunked header",
                ));
            }
            buf.extend_from_slice(&temp[..n]);
            if buf.len() > max_body_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunked body too large",
                ));
            }
        }

        let newline_pos = buf.iter().position(|&b| b == b'\n').unwrap();
        let size_line = String::from_utf8_lossy(&buf[..newline_pos]);
        let size_hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        buf.drain(..=newline_pos);

        if chunk_size == 0 {
            // Read optional trailers until empty line
            loop {
                while !buf.contains(&b'\n') {
                    let n = stream.read(&mut temp)?;
                    if n == 0 {
                        return Ok((body, Vec::new()));
                    }
                    buf.extend_from_slice(&temp[..n]);
                }
                let line_end = buf.iter().position(|&b| b == b'\n').unwrap();
                let line = &buf[..line_end];
                let is_empty = line == b"\r" || line.is_empty();
                buf.drain(..=line_end);
                if is_empty {
                    break;
                }
            }
            return Ok((body, buf));
        }

        if body.len() + chunk_size > max_body_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked body exceeds max allowed size",
            ));
        }

        while buf.len() < chunk_size + 2 {
            let n = stream.read(&mut temp)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF in chunk data",
                ));
            }
            buf.extend_from_slice(&temp[..n]);
        }

        body.extend_from_slice(&buf[..chunk_size]);
        buf.drain(..chunk_size);
        if buf.starts_with(b"\r\n") {
            buf.drain(..2);
        } else if buf.starts_with(b"\n") {
            buf.drain(..1);
        }
    }
}

fn send_response(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    body: &str,
    keep_alive: bool,
) -> io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{}",
        status_code,
        reason,
        body.len(),
        conn,
        body
    )?;
    stream.flush()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
