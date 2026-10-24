//! Minimal blocking HTTP/1.1 receiver for OTLP/HTTP (logs, metrics, traces).
//!
//! The listener faces local but untrusted clients, so every input is bounded:
//! header bytes, body bytes (before and after gzip), chunk-size lines, trailers,
//! per-read idle time and total time per request. Requests whose framing is
//! ambiguous (both `Content-Length` and `Transfer-Encoding`, conflicting
//! `Content-Length`s, unknown transfer codings) are refused and the connection
//! closed, because guessing would desynchronise keep-alive request boundaries.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tacho_core::Input;

pub const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 64;
pub const DEFAULT_MAX_BODY_SIZE: usize = 16 * 1024 * 1024; // 16 MiB
pub const DEFAULT_MAX_HEADER_SIZE: usize = 64 * 1024; // 64 KiB
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Total time a single request (head + body) may take. Bounds slow-drip
/// clients that send one byte just inside every `io_timeout`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest accepted chunk-size or trailer line in a chunked body.
const MAX_CHUNK_LINE: usize = 4096;

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
    /// Total time allowed for one request, head and body (default: 30 seconds).
    pub request_timeout: Duration,
    /// Optional shutdown file descriptor (e.g. read end of a pipe).
    /// When provided, the accept loop blocks indefinitely on poll until a connection
    /// arrives or shutdown_fd is signaled/closed, eliminating timed poll loops.
    pub shutdown_fd: Option<std::os::unix::io::RawFd>,
    /// Optional file descriptor that deterministically signals per-read I/O timeout expiration.
    pub timeout_fd: Option<std::os::unix::io::RawFd>,
}

impl Default for OtlpServerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_connections: DEFAULT_MAX_CONCURRENT_CONNECTIONS,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            io_timeout: DEFAULT_IO_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_fd: None,
            timeout_fd: None,
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
///
/// With `config.shutdown_fd` set, the loop sleeps in `poll` until a client
/// connects or the fd becomes readable/hung up. Without it the only stop signal
/// is the `shutdown` flag, which cannot wake `poll`, so the loop re-checks it
/// every 100 ms; callers that care about prompt, event-driven shutdown should
/// pass a `shutdown_fd`.
pub fn serve_otlp_with_config(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    config: OtlpServerConfig,
    on_inputs: impl Fn(Vec<Input>) + Send + Sync + 'static,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let on_inputs: Arc<dyn Fn(Vec<Input>) + Send + Sync> = Arc::new(on_inputs);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let config = Arc::new(config);

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                dispatch_connection(stream, &config, &active_connections, &on_inputs)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if wait_for_accept_or_shutdown(&listener, config.shutdown_fd)? {
                    break;
                }
            }
            // A client that resets between SYN and accept() must not take the
            // whole receiver down with it.
            Err(e) if is_transient_accept_error(&e) => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Decrements the active-connection counter when dropped, so a panicking
/// handler (or a panicking `on_inputs` callback) still frees its slot.
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn dispatch_connection(
    stream: TcpStream,
    config: &Arc<OtlpServerConfig>,
    active: &Arc<AtomicUsize>,
    on_inputs: &Arc<dyn Fn(Vec<Input>) + Send + Sync>,
) {
    let current = active.fetch_add(1, Ordering::SeqCst);
    let guard = SlotGuard(Arc::clone(active));
    if current >= config.max_concurrent_connections {
        drop(guard);
        reject_busy(stream);
        return;
    }

    let config = Arc::clone(config);
    let on_inputs = Arc::clone(on_inputs);
    // If the thread cannot be spawned the closure (and with it the guard and
    // the stream) is dropped, which frees the slot and closes the socket.
    let _ = std::thread::Builder::new()
        .name("tacho-otlp-conn".to_string())
        .spawn(move || {
            let _guard = guard;
            let _ = handle_client(stream, &config, &*on_inputs);
        });
}

/// Answers an over-capacity client without ever blocking the accept loop.
fn reject_busy(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(true);
    let _ = stream.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
}

fn is_transient_accept_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) || matches!(e.raw_os_error(), Some(libc::EPROTO) | Some(libc::EPERM))
}

/// Blocks until the listener is readable. Returns `Ok(true)` if the shutdown
/// fd fired instead.
fn wait_for_accept_or_shutdown(
    listener: &TcpListener,
    shutdown_fd: Option<std::os::unix::io::RawFd>,
) -> io::Result<bool> {
    let mut pfds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: shutdown_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let (nfds, timeout_ms) = match shutdown_fd {
        Some(_) => (2, -1),
        None => (1, 100),
    };
    // SAFETY: `pfds` is a valid array of at least `nfds` pollfd structs.
    let ret = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, timeout_ms) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(err);
    }
    let fired = libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    Ok(shutdown_fd.is_some() && (pfds[1].revents & fired) != 0)
}

/// An HTTP error response, plus whether the connection can be reused after it.
/// Only errors raised after the body has been fully consumed may keep the
/// connection alive; anything earlier leaves unread bytes on the wire.
#[derive(Debug)]
struct Reject {
    status: u16,
    reason: &'static str,
    message: &'static str,
    keep_alive: bool,
}

impl Reject {
    fn close(status: u16, reason: &'static str, message: &'static str) -> Self {
        Self {
            status,
            reason,
            message,
            keep_alive: false,
        }
    }

    fn bad_request(message: &'static str) -> Self {
        Self::close(400, "Bad Request", message)
    }

    fn too_large(message: &'static str) -> Self {
        Self::close(413, "Payload Too Large", message)
    }

    /// Maps a read error to a response; `None` means the peer is gone and
    /// there is nobody to answer.
    fn from_io(e: &io::Error, what: &'static str) -> Option<Self> {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                Some(Self::close(408, "Request Timeout", "request timed out"))
            }
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                Some(Self::bad_request(what))
            }
            _ => None,
        }
    }
}

/// A connection plus its read-ahead buffer. Bytes past the current request
/// (pipelining) stay in `buf` for the next one.
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    io_timeout: Duration,
    timeout_fd: Option<std::os::unix::io::RawFd>,
    deadline: Instant,
}

impl Conn {
    /// Reads more bytes into `buf`. Returns 0 on EOF. Fails with `TimedOut`
    /// once the per-request deadline has passed.
    fn fill(&mut self) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let timeout = remaining.min(self.io_timeout);
        if let Some(tfd) = self.timeout_fd {
            let mut pfds = [
                libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: tfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            loop {
                let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 2, ms) };
                if rc < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if rc == 0 {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if (pfds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                    break;
                }
                if (pfds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
        }
        self.stream.set_read_timeout(Some(timeout))?;
        let mut temp = [0u8; 8192];
        let n = loop {
            match self.stream.read(&mut temp) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        self.buf.extend_from_slice(&temp[..n]);
        Ok(n)
    }

    /// Reads until `buf` holds at least `len` bytes; EOF first is an error.
    fn fill_to(&mut self, len: usize) -> io::Result<()> {
        while self.buf.len() < len {
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(())
    }

    /// Removes and returns one line (without CRLF/LF) of at most `max` bytes.
    fn take_line(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut scanned = 0;
        loop {
            if let Some(pos) = self.buf[scanned..].iter().position(|&b| b == b'\n') {
                let end = scanned + pos;
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            scanned = self.buf.len();
            if scanned > max {
                return Err(invalid("line too long"));
            }
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    /// Reads one request head (through the blank line). `Ok(None)` is a clean
    /// EOF between requests.
    fn read_head(&mut self, max_header_size: usize) -> io::Result<Option<Vec<u8>>> {
        let mut scanned: usize = 0;
        loop {
            // Resume the search 3 bytes back so a terminator split across reads
            // is still found, without rescanning the whole buffer (quadratic).
            let from = scanned.saturating_sub(3);
            if let Some(pos) = find_subsequence(&self.buf[from..], b"\r\n\r\n") {
                let end = from + pos;
                if end > max_header_size {
                    break;
                }
                let head = self.buf[..end].to_vec();
                self.buf.drain(..end + 4);
                return Ok(Some(head));
            }
            scanned = self.buf.len();
            if scanned > max_header_size {
                break;
            }
            if self.fill()? == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Err(invalid("headers exceed max size"))
    }

    fn take(&mut self, len: usize) -> Vec<u8> {
        self.buf.drain(..len).collect()
    }
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The supported OTLP signals, by request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Logs,
    Metrics,
    Traces,
}

impl Signal {
    fn from_path(path: &str) -> Option<Self> {
        let clean = path.split('?').next().unwrap_or(path).trim_end_matches('/');
        match clean {
            "/v1/logs" => Some(Self::Logs),
            "/v1/metrics" => Some(Self::Metrics),
            "/v1/traces" => Some(Self::Traces),
            _ => None,
        }
    }
}

/// How the request body is delimited on the wire.
#[derive(Debug, PartialEq, Eq)]
enum Framing {
    Empty,
    Length(usize),
    Chunked,
}

/// A validated request head: everything needed to read and decode the body.
#[derive(Debug)]
struct RequestHead {
    signal: Signal,
    is_proto: bool,
    is_gzip: bool,
    framing: Framing,
    expect_continue: bool,
    keep_alive: bool,
}

/// Parses and validates a request head. All failures close the connection,
/// since the body (if any) has not been consumed.
fn parse_head(head: &[u8], max_body_size: usize) -> Result<RequestHead, Reject> {
    let text = std::str::from_utf8(head).map_err(|_| Reject::bad_request("non-utf8 headers"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ').filter(|p| !p.is_empty());
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Reject::bad_request("invalid request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Reject::bad_request("unsupported HTTP version"));
    }

    let mut connection = None;
    let mut content_type = String::new();
    let mut content_encoding = None;
    let mut transfer_encoding = None;
    let mut content_length: Option<usize> = None;
    let mut expect_continue = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Reject::bad_request("malformed header line"));
        };
        // Whitespace before the colon is a classic smuggling vector (RFC 9112 5.1).
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(Reject::bad_request("malformed header name"));
        }
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "connection" => connection = Some(value.to_ascii_lowercase()),
            "content-type" => content_type = value.to_ascii_lowercase(),
            "content-encoding" => content_encoding = Some(value.to_ascii_lowercase()),
            "transfer-encoding" => {
                if transfer_encoding.is_some() {
                    return Err(Reject::bad_request("duplicate transfer-encoding"));
                }
                transfer_encoding = Some(value.to_ascii_lowercase());
            }
            "content-length" => {
                let len = parse_content_length(value)?;
                if content_length.is_some_and(|prev| prev != len) {
                    return Err(Reject::bad_request("conflicting content-length"));
                }
                content_length = Some(len);
            }
            "expect" => expect_continue = value.eq_ignore_ascii_case("100-continue"),
            _ => {}
        }
    }

    let keep_alive = match connection.as_deref() {
        Some(c) => !c.split(',').any(|t| t.trim() == "close"),
        None => version == "HTTP/1.1",
    };

    if method != "POST" {
        return Err(Reject::close(
            405,
            "Method Not Allowed",
            "only POST is allowed",
        ));
    }
    let signal =
        Signal::from_path(path).ok_or(Reject::close(404, "Not Found", "endpoint not found"))?;

    let is_proto = content_type.contains("application/x-protobuf");
    if !is_proto && !content_type.contains("application/json") {
        return Err(Reject::close(
            415,
            "Unsupported Media Type",
            "content-type must be application/x-protobuf or application/json",
        ));
    }

    let is_gzip = match content_encoding.as_deref() {
        None | Some("") | Some("identity") => false,
        Some("gzip") => true,
        Some(_) => {
            return Err(Reject::close(
                415,
                "Unsupported Media Type",
                "unsupported content-encoding",
            ))
        }
    };

    let framing = match (transfer_encoding.as_deref(), content_length) {
        (Some(_), Some(_)) => {
            return Err(Reject::bad_request(
                "both transfer-encoding and content-length",
            ))
        }
        (Some("chunked"), None) => Framing::Chunked,
        (Some(_), None) => {
            return Err(Reject::close(
                501,
                "Not Implemented",
                "unsupported transfer-encoding",
            ))
        }
        (None, Some(len)) if len > max_body_size => {
            return Err(Reject::too_large("body exceeds max allowed size"))
        }
        (None, Some(len)) => Framing::Length(len),
        (None, None) => Framing::Empty,
    };

    Ok(RequestHead {
        signal,
        is_proto,
        is_gzip,
        framing,
        expect_continue,
        keep_alive,
    })
}

/// Strict decimal parse: `usize::from_str` would also accept a leading `+`.
fn parse_content_length(value: &str) -> Result<usize, Reject> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Reject::bad_request("invalid content-length"));
    }
    // All digits, so the only failure left is overflow: certainly too large.
    value
        .parse()
        .map_err(|_| Reject::too_large("body exceeds max allowed size"))
}

fn read_body(conn: &mut Conn, framing: &Framing, max_body_size: usize) -> Result<Vec<u8>, Reject> {
    match framing {
        Framing::Empty => Ok(Vec::new()),
        Framing::Length(len) => match conn.fill_to(*len) {
            Ok(()) => Ok(conn.take(*len)),
            Err(e) => Err(Reject::from_io(&e, "unexpected EOF reading body")
                .unwrap_or(Reject::bad_request("unexpected EOF reading body"))),
        },
        Framing::Chunked => read_chunked_body(conn, max_body_size),
    }
}

fn read_chunked_body(conn: &mut Conn, max_body_size: usize) -> Result<Vec<u8>, Reject> {
    let io_err = |e: io::Error| {
        Reject::from_io(&e, "malformed chunked body")
            .unwrap_or(Reject::bad_request("malformed chunked body"))
    };
    let mut body = Vec::new();
    loop {
        let line = conn.take_line(MAX_CHUNK_LINE).map_err(io_err)?;
        let chunk_size = parse_chunk_size(&line)?;
        if chunk_size == 0 {
            break;
        }
        if body.len().saturating_add(chunk_size) > max_body_size {
            return Err(Reject::too_large("chunked body exceeds max allowed size"));
        }
        // chunk_size <= max_body_size here, so +2 cannot overflow.
        conn.fill_to(chunk_size + 2).map_err(io_err)?;
        body.extend_from_slice(&conn.buf[..chunk_size]);
        conn.buf.drain(..chunk_size);
        if conn.buf.starts_with(b"\r\n") {
            conn.buf.drain(..2);
        } else {
            return Err(Reject::bad_request("chunk data not followed by CRLF"));
        }
    }

    // Trailers: discarded, but bounded in count and size.
    let mut trailer_bytes = 0usize;
    loop {
        let line = conn.take_line(MAX_CHUNK_LINE).map_err(io_err)?;
        if line.is_empty() {
            return Ok(body);
        }
        trailer_bytes += line.len();
        if trailer_bytes > MAX_CHUNK_LINE * 4 {
            return Err(Reject::bad_request("chunked trailers too large"));
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<usize, Reject> {
    let size_part = line.split(|&b| b == b';').next().unwrap_or_default();
    let hex = std::str::from_utf8(size_part)
        .map_err(|_| Reject::bad_request("malformed chunk size"))?
        .trim();
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Reject::bad_request("malformed chunk size"));
    }
    usize::from_str_radix(hex, 16)
        .map_err(|_| Reject::too_large("chunked body exceeds max allowed size"))
}

/// Gunzips with an output cap so a small bomb cannot exhaust memory.
fn gunzip_bounded(body: &[u8], max: usize, keep_alive: bool) -> Result<Vec<u8>, Reject> {
    let gz = flate2::read::GzDecoder::new(body);
    let mut out = Vec::new();
    if gz.take(max as u64 + 1).read_to_end(&mut out).is_err() {
        return Err(Reject {
            keep_alive,
            ..Reject::bad_request("failed to decompress gzip body")
        });
    }
    if out.len() > max {
        return Err(Reject::too_large(
            "decompressed body exceeds max allowed size",
        ));
    }
    Ok(out)
}

fn decode_payload(head: &RequestHead, payload: &[u8]) -> Result<Vec<Input>, Reject> {
    let mut decoder = crate::otlp::OtlpDecoder::new();
    if head.is_proto {
        return Ok(match head.signal {
            Signal::Logs => decoder.decode_logs_proto(payload),
            Signal::Metrics => decoder.decode_metrics_proto(payload),
            Signal::Traces => decoder.decode_traces_proto(payload),
        });
    }
    let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| Reject {
        keep_alive: head.keep_alive,
        ..Reject::bad_request("invalid JSON body")
    })?;
    Ok(match head.signal {
        Signal::Logs => decoder.decode_logs_json(&value),
        Signal::Metrics => decoder.decode_metrics_json(&value),
        Signal::Traces => decoder.decode_traces_json(&value),
    })
}

/// Reads, validates and decodes one request. `Ok(None)` is a clean EOF.
fn serve_one(
    conn: &mut Conn,
    config: &OtlpServerConfig,
    on_inputs: &(dyn Fn(Vec<Input>) + Send + Sync),
) -> Result<Option<RequestHead>, Option<Reject>> {
    let head_bytes = match conn.read_head(config.max_header_size) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return Err(Reject::from_io(&e, "malformed request")),
    };
    let head = parse_head(&head_bytes, config.max_body_size).map_err(Some)?;

    if head.expect_continue && head.framing != Framing::Empty {
        conn.stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|_| None)?;
    }

    let body = read_body(conn, &head.framing, config.max_body_size).map_err(Some)?;
    let payload = if head.is_gzip {
        gunzip_bounded(&body, config.max_body_size, head.keep_alive).map_err(Some)?
    } else {
        body
    };
    let inputs = decode_payload(&head, &payload).map_err(Some)?;
    if !inputs.is_empty() {
        on_inputs(inputs);
    }
    Ok(Some(head))
}

fn handle_client(
    stream: TcpStream,
    config: &OtlpServerConfig,
    on_inputs: &(dyn Fn(Vec<Input>) + Send + Sync),
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(config.io_timeout))?;
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        io_timeout: config.io_timeout,
        timeout_fd: config.timeout_fd,
        deadline: Instant::now(),
    };

    loop {
        conn.deadline = Instant::now() + config.request_timeout;
        match serve_one(&mut conn, config, on_inputs) {
            Ok(None) | Err(None) => return Ok(()),
            Ok(Some(head)) => {
                send_ok(&mut conn.stream, head.is_proto, head.keep_alive)?;
                if !head.keep_alive {
                    return Ok(());
                }
            }
            Err(Some(reject)) => {
                send_response(
                    &mut conn.stream,
                    reject.status,
                    reject.reason,
                    reject.message,
                    reject.keep_alive,
                )?;
                if !reject.keep_alive {
                    lingering_close(&mut conn);
                    return Ok(());
                }
            }
        }
    }
}

/// Closing a socket that still has unread request bytes makes the kernel send
/// RST, which can destroy the error response before the client reads it. Send
/// FIN first, then discard a bounded amount of input until the client closes.
fn lingering_close(conn: &mut Conn) {
    const MAX_DRAIN: usize = 1024 * 1024;
    let _ = conn.stream.shutdown(std::net::Shutdown::Write);
    if conn.timeout_fd.is_some() {
        return;
    }
    let _ = conn.stream.set_read_timeout(Some(conn.io_timeout));
    let mut sink = [0u8; 8192];
    let mut drained = 0;
    while drained < MAX_DRAIN {
        match conn.stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn send_ok(stream: &mut TcpStream, is_proto: bool, keep_alive: bool) -> io::Result<()> {
    let conn_hdr = if keep_alive { "keep-alive" } else { "close" };
    let (content_type, body) = if is_proto {
        ("application/x-protobuf", "")
    } else {
        ("application/json", "{}")
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{}",
        content_type,
        body.len(),
        conn_hdr,
        body
    )?;
    stream.flush()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &str) -> Result<RequestHead, Reject> {
        parse_head(s.as_bytes(), 1024)
    }

    const OK_PREFIX: &str = "POST /v1/logs HTTP/1.1\r\nContent-Type: application/json";

    #[test]
    fn rejects_te_and_cl_together() {
        let r = head(&format!(
            "{OK_PREFIX}\r\nContent-Length: 5\r\nTransfer-Encoding: chunked"
        ))
        .unwrap_err();
        assert_eq!(r.status, 400);
        assert!(!r.keep_alive);
    }

    #[test]
    fn rejects_conflicting_content_length_but_allows_repeats() {
        assert_eq!(
            head(&format!(
                "{OK_PREFIX}\r\nContent-Length: 5\r\nContent-Length: 6"
            ))
            .unwrap_err()
            .status,
            400
        );
        let ok = head(&format!(
            "{OK_PREFIX}\r\nContent-Length: 5\r\nContent-Length: 5"
        ))
        .unwrap();
        assert_eq!(ok.framing, Framing::Length(5));
    }

    #[test]
    fn rejects_signed_or_overflowing_content_length() {
        assert_eq!(
            head(&format!("{OK_PREFIX}\r\nContent-Length: +5"))
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            head(&format!(
                "{OK_PREFIX}\r\nContent-Length: 99999999999999999999999999"
            ))
            .unwrap_err()
            .status,
            413
        );
    }

    #[test]
    fn rejects_space_before_colon_and_unknown_te() {
        assert_eq!(
            head(&format!("{OK_PREFIX}\r\nContent-Length : 5"))
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            head(&format!("{OK_PREFIX}\r\nTransfer-Encoding: gzip, chunked"))
                .unwrap_err()
                .status,
            501
        );
    }

    #[test]
    fn chunk_size_overflow_is_too_large_not_panic() {
        assert_eq!(
            parse_chunk_size(b"ffffffffffffffffffffffff")
                .unwrap_err()
                .status,
            413
        );
        assert_eq!(parse_chunk_size(b"-1").unwrap_err().status, 400);
        assert_eq!(parse_chunk_size(b"1a;ext=1").unwrap(), 26);
    }

    #[test]
    fn connection_header_token_list() {
        let h = head(&format!("{OK_PREFIX}\r\nConnection: keep-alive, Close")).unwrap();
        assert!(!h.keep_alive);
        let h = parse_head(
            b"POST /v1/logs HTTP/1.0\r\nContent-Type: application/json",
            1024,
        )
        .unwrap();
        assert!(!h.keep_alive);
    }

    #[test]
    fn unicode_headers_do_not_panic() {
        let r = head("POST /v1/l\u{f6}gs HTTP/1.1\r\nX-\u{1f600}: \u{1f600}");
        assert_eq!(r.unwrap_err().status, 404);
    }
}
