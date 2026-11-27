//! High-performance Peer Federation HTTP/1.1 client:
//! - [`PeerKeepAlivePool`]: per-peer HTTP/1.1 Keep-Alive TCP connection pool (max 2 idle sockets,
//!   30s idle TTL, stale-EOF automatic retry for idempotent GETs, chunked & `Content-Length`
//!   decoding, pure TCP/HTTP RTT measurement via `/api/health`, plus `https://` `curl` fallback).
//! - [`PeerSseSubscriber`]: persistent `GET /api/events` SSE stream reader (`Accept: text/event-stream`,
//!   `Last-Event-Id` replay header on reconnect, `1s -> 2s -> 5s -> 15s` exponential backoff,
//!   reusable zero-per-line-allocation line buffering, chunked & identity HTTP/1.1 decoding).
//! - [`SshLoopbackTunnel`]: 1-script atomic SSH bootstrap of remote `starfaned` + OpenSSH
//!   `ControlMaster` dynamic loopback forward (`ssh -O forward -L 127.0.0.1:0:127.0.0.1:<port>`)
//!   with clean `ssh -O cancel` teardown on close/Drop.

use serde_json::Value;
use starfane_proto::{ApiError, DaemonEvent, ErrorCode};
use starfane_tmux::{ExecTransport, SshExec};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Default per-request HTTP timeout for peer JSON calls.
pub const DEFAULT_PEER_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
/// Default maximum idle Keep-Alive TCP sockets retained per peer pool.
pub const DEFAULT_KEEPALIVE_MAX_IDLE: usize = 2;
/// Default idle TTL for Keep-Alive TCP sockets before eviction.
pub const DEFAULT_KEEPALIVE_IDLE_TTL: Duration = Duration::from_secs(30);
/// Default idle read timeout for persistent SSE streams (axum keep-alive fires every 15s).
pub const DEFAULT_SSE_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Hard cap on any single peer JSON response payload (32 MiB).
pub const MAX_PEER_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
/// Default SSE reconnect backoff schedule (`1s -> 2s -> 5s -> 15s`).
pub const DEFAULT_SSE_BACKOFF_SECS: &[u64] = &[1, 2, 5, 15];

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_SSE_LINE_BYTES: usize = 4 * 1024 * 1024;
const RTT_UNSET: u64 = u64::MAX;
const BOOT_BEGIN_MARKER: &str = "__SF_PEER_BOOT_BEGIN__";
const BOOT_END_MARKER: &str = "__SF_PEER_BOOT_END__";
const TUNNEL_OP_TIMEOUT: Duration = Duration::from_secs(10);
const TUNNEL_CANCEL_TIMEOUT: Duration = Duration::from_millis(800);

// ---------------------------------------------------------------------------
// Shared error type & URL parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerHttpError {
    Transport(String),
    Http { status: u16, body: String },
}

impl PeerHttpError {
    pub fn with_prefix(self, base: &str) -> Self {
        match self {
            PeerHttpError::Transport(m) => PeerHttpError::Transport(format!("{base}: {m}")),
            PeerHttpError::Http { status, body } => PeerHttpError::Http {
                status,
                body: format!("{base}: {body}"),
            },
        }
    }

    pub fn into_api_error(self, ctx: &str) -> ApiError {
        match self {
            PeerHttpError::Transport(m) => {
                ApiError::new(ErrorCode::Unavailable, format!("{ctx}: {m}"))
            }
            PeerHttpError::Http { status, body } => {
                let msg = format!("{ctx}: HTTP {status}: {body}");
                let code = match status {
                    400 => ErrorCode::BadRequest,
                    404 => ErrorCode::NotFound,
                    409 => ErrorCode::Conflict,
                    422 => ErrorCode::GateRejected,
                    503 => ErrorCode::Unavailable,
                    _ => ErrorCode::Internal,
                };
                ApiError::new(code, msg)
            }
        }
    }
}

impl fmt::Display for PeerHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerHttpError::Transport(m) => f.write_str(m),
            PeerHttpError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
        }
    }
}

impl std::error::Error for PeerHttpError {}

struct ParsedHttpUrl<'a> {
    host_port: &'a str,
    connect_addr: String,
    path: &'a str,
}

fn parse_http_url(without_scheme: &str) -> Result<ParsedHttpUrl<'_>, PeerHttpError> {
    if without_scheme.contains(['\r', '\n']) {
        return Err(PeerHttpError::Transport(
            "invalid newline in peer URL".into(),
        ));
    }
    let (host_port, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    if host_port.is_empty() {
        return Err(PeerHttpError::Transport("empty peer host".into()));
    }
    let connect_addr = if host_port.starts_with('[') {
        if host_port.contains("]:") {
            host_port.to_string()
        } else {
            format!("{host_port}:80")
        }
    } else if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };
    Ok(ParsedHttpUrl {
        host_port,
        connect_addr,
        path,
    })
}

fn validate_header_value(label: &str, v: &str) -> Result<(), PeerHttpError> {
    if v.contains(['\r', '\n', '\0']) {
        return Err(PeerHttpError::Transport(format!(
            "invalid control character in {label}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. PeerKeepAlivePool (HTTP/1.1 Keep-Alive TCP pool + pure RTT tracking)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct IdleConn {
    addr: String,
    stream: TcpStream,
    last_used: Instant,
}

/// Small per-peer HTTP/1.1 Keep-Alive TCP connection pool.
///
/// Retains up to `max_idle` (default `2`) warm TCP connections for `idle_ttl` (default `30s`),
/// retries stale keep-alive EOF once automatically for idempotent `GET`/`HEAD` requests,
/// supports both `Transfer-Encoding: chunked` and `Content-Length` response framing, and tracks
/// pure transport RTT EWMA (`rtt_ms`) via [`PeerKeepAlivePool::probe_rtt`].
#[derive(Debug)]
pub struct PeerKeepAlivePool {
    idle: Mutex<Vec<IdleConn>>,
    max_idle: usize,
    idle_ttl: Duration,
    timeout: Duration,
    rtt_ewma_ms: AtomicU64,
}

impl Default for PeerKeepAlivePool {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerKeepAlivePool {
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_KEEPALIVE_MAX_IDLE,
            DEFAULT_KEEPALIVE_IDLE_TTL,
            DEFAULT_PEER_HTTP_TIMEOUT,
        )
    }

    pub fn with_config(max_idle: usize, idle_ttl: Duration, timeout: Duration) -> Self {
        Self {
            idle: Mutex::new(Vec::with_capacity(max_idle)),
            max_idle,
            idle_ttl,
            timeout: timeout.max(Duration::from_millis(100)),
            rtt_ewma_ms: AtomicU64::new(RTT_UNSET),
        }
    }

    /// Exponentially weighted moving average pure transport round-trip latency in milliseconds.
    pub fn rtt_ms(&self) -> Option<u64> {
        let v = self.rtt_ewma_ms.load(Ordering::Acquire);
        (v != RTT_UNSET).then_some(v)
    }

    /// Record a pure transport RTT sample (in ms) into the EWMA (`alpha = 0.25`).
    pub fn record_rtt_sample(&self, sample_ms: u64) {
        let sample = sample_ms.min(RTT_UNSET - 1);
        let mut prev = self.rtt_ewma_ms.load(Ordering::Acquire);
        loop {
            let next = if prev == RTT_UNSET {
                sample
            } else {
                prev.saturating_mul(3)
                    .saturating_add(sample)
                    .saturating_add(2)
                    / 4
            };
            match self.rtt_ewma_ms.compare_exchange_weak(
                prev,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }
    }

    /// Number of currently pooled (non-expired) idle Keep-Alive sockets.
    pub fn idle_count(&self) -> usize {
        let now = Instant::now();
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|c| now.saturating_duration_since(c.last_used) < self.idle_ttl);
        guard.len()
    }

    /// Drop all idle pooled sockets (e.g. after tunnel reset or endpoint failover).
    pub fn clear_idle(&self) {
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        guard.clear();
    }

    /// Lightweight `GET /api/health` pure transport RTT probe using the Keep-Alive pool.
    pub async fn probe_rtt(
        &self,
        base_url: &str,
        token: Option<&str>,
    ) -> Result<u64, PeerHttpError> {
        self.probe_rtt_with_host(base_url, None, token).await
    }

    /// Variant of [`PeerKeepAlivePool::probe_rtt`] with an explicit HTTP `Host` header override
    /// (required when forwarding through [`SshLoopbackTunnel`] to a different remote port).
    pub async fn probe_rtt_with_host(
        &self,
        base_url: &str,
        host_override: Option<&str>,
        token: Option<&str>,
    ) -> Result<u64, PeerHttpError> {
        let url = format!("{}/api/health", base_url.trim_end_matches('/'));
        let started = Instant::now();
        let _ = self
            .request_json_with_host("GET", &url, host_override, token, None)
            .await?;
        let ms = started.elapsed().as_millis().min((RTT_UNSET - 1) as u128) as u64;
        self.record_rtt_sample(ms);
        Ok(ms)
    }

    pub async fn get_json(&self, url: &str, token: Option<&str>) -> Result<Value, PeerHttpError> {
        self.request_json("GET", url, token, None).await
    }

    pub async fn post_json(
        &self,
        url: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, PeerHttpError> {
        self.request_json("POST", url, token, body).await
    }

    pub async fn request_json(
        &self,
        method: &str,
        url: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, PeerHttpError> {
        self.request_json_with_host(method, url, None, token, body)
            .await
    }

    /// Execute an HTTP JSON request over Keep-Alive (`http://`) or `curl` (`https://`),
    /// optionally overriding the HTTP `Host` / `Origin` authority (for loopback port forwards).
    pub async fn request_json_with_host(
        &self,
        method: &str,
        url: &str,
        host_override: Option<&str>,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, PeerHttpError> {
        if let Some(rest) = url.strip_prefix("http://") {
            tokio::time::timeout(
                self.timeout,
                self.http1_keepalive_json(method, rest, host_override, token, body),
            )
            .await
            .map_err(|_| {
                PeerHttpError::Transport(format!("timeout after {}s", self.timeout.as_secs()))
            })?
        } else if url.starts_with("https://") {
            curl_http_json(method, url, host_override, token, body, self.timeout).await
        } else {
            Err(PeerHttpError::Transport(format!(
                "unsupported peer endpoint scheme: {url}"
            )))
        }
    }

    fn take_idle(&self, connect_addr: &str) -> Option<TcpStream> {
        let now = Instant::now();
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|c| now.saturating_duration_since(c.last_used) < self.idle_ttl);

        // Pop most-recently-used matching socket (LIFO maximizes server-side keep-alive hits).
        while let Some(pos) = guard.iter().rposition(|c| c.addr == connect_addr) {
            let conn = guard.remove(pos);
            let mut probe = [0u8; 1];
            match conn.stream.try_read(&mut probe) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Some(conn.stream);
                }
                // Ok(0) = peer sent FIN; Ok(n) = unexpected buffered garbage; Err(_) = dead socket.
                _ => continue,
            }
        }
        None
    }

    fn put_idle(&self, connect_addr: String, stream: TcpStream) {
        if self.max_idle == 0 {
            return;
        }
        let now = Instant::now();
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|c| now.saturating_duration_since(c.last_used) < self.idle_ttl);
        while guard.len() >= self.max_idle {
            guard.remove(0);
        }
        guard.push(IdleConn {
            addr: connect_addr,
            stream,
            last_used: now,
        });
    }

    async fn http1_keepalive_json(
        &self,
        method: &str,
        without_scheme: &str,
        host_override: Option<&str>,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, PeerHttpError> {
        let parsed = parse_http_url(without_scheme)?;
        let host_header = host_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(parsed.host_port);
        validate_header_value("Host header", host_header)?;
        if let Some(tok) = token {
            validate_header_value("bearer token", tok)?;
        }

        let body_bytes = match body {
            Some(v) => serde_json::to_vec(v)
                .map_err(|e| PeerHttpError::Transport(format!("encode body: {e}")))?,
            None => Vec::new(),
        };

        let mut req = format!(
            "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: keep-alive\r\nAccept: application/json\r\n",
            parsed.path
        );
        if let Some(tok) = token {
            req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
        }
        let is_non_get =
            !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD");
        if body.is_some() {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
            req.push_str(&format!("Origin: http://{host_header}\r\n"));
        } else if is_non_get {
            req.push_str("Content-Length: 0\r\n");
            req.push_str(&format!("Origin: http://{host_header}\r\n"));
        }
        req.push_str("\r\n");
        let mut wire = req.into_bytes();
        wire.extend_from_slice(&body_bytes);

        let idempotent = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");

        // Attempt 1: prefer pooled warm socket; on stale EOF during idempotent request, retry fresh.
        let mut pooled = self.take_idle(&parsed.connect_addr);
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let (stream, reused) = match pooled.take() {
                Some(s) => (s, true),
                None => {
                    let s = TcpStream::connect(&parsed.connect_addr)
                        .await
                        .map_err(|e| {
                            PeerHttpError::Transport(format!(
                                "connect {}: {e}",
                                parsed.connect_addr
                            ))
                        })?;
                    let _ = s.set_nodelay(true);
                    (s, false)
                }
            };

            match exchange_http1_response(stream, &wire, method).await {
                Ok(ex) => {
                    if ex.can_reuse {
                        self.put_idle(parsed.connect_addr.clone(), ex.stream);
                    }
                    if !(200..300).contains(&ex.status) {
                        return Err(PeerHttpError::Http {
                            status: ex.status,
                            body: String::from_utf8_lossy(&ex.body).trim().to_string(),
                        });
                    }
                    return serde_json::from_slice(&ex.body)
                        .map_err(|e| PeerHttpError::Transport(format!("parse JSON: {e}")));
                }
                Err(ExchangeError::StaleBeforeResponse(msg))
                    if reused && idempotent && attempt == 1 =>
                {
                    log::debug!(
                        "peer_client: stale keep-alive socket for {} ({msg}); retrying fresh",
                        parsed.connect_addr
                    );
                    continue;
                }
                Err(ExchangeError::StaleBeforeResponse(msg))
                | Err(ExchangeError::Transport(msg)) => {
                    return Err(PeerHttpError::Transport(msg));
                }
            }
        }
    }
}

struct Http1Exchange {
    status: u16,
    body: Vec<u8>,
    stream: TcpStream,
    can_reuse: bool,
}

enum ExchangeError {
    StaleBeforeResponse(String),
    Transport(String),
}

async fn exchange_http1_response(
    mut stream: TcpStream,
    wire: &[u8],
    method: &str,
) -> Result<Http1Exchange, ExchangeError> {
    if let Err(e) = stream.write_all(wire).await {
        return Err(ExchangeError::StaleBeforeResponse(format!(
            "write request: {e}"
        )));
    }

    let mut reader = BufReader::with_capacity(16 * 1024, stream);
    let head = read_http_head(&mut reader).await?;

    let mut can_reuse = !head.connection_close
        && (head.is_http11 || (head.is_http10 && head.connection_keep_alive));

    let body = if method.eq_ignore_ascii_case("HEAD") || head.status == 204 || head.status == 304 {
        Vec::new()
    } else if head.is_chunked {
        read_chunked_body(&mut reader, MAX_PEER_RESPONSE_BYTES)
            .await
            .map_err(ExchangeError::Transport)?
    } else if let Some(len) = head.content_length {
        if len > MAX_PEER_RESPONSE_BYTES {
            return Err(ExchangeError::Transport(format!(
                "peer response exceeded {} MiB limit",
                MAX_PEER_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        let mut buf = vec![0u8; len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| ExchangeError::Transport(format!("read response body: {e}")))?;
        buf
    } else {
        can_reuse = false;
        let mut buf = Vec::new();
        (&mut reader)
            .take(MAX_PEER_RESPONSE_BYTES + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| ExchangeError::Transport(format!("read response body: {e}")))?;
        if (buf.len() as u64) > MAX_PEER_RESPONSE_BYTES {
            return Err(ExchangeError::Transport(format!(
                "peer response exceeded {} MiB limit",
                MAX_PEER_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        buf
    };

    if !reader.buffer().is_empty() {
        // Unexpected trailing bytes in buffer: do not reuse this socket.
        can_reuse = false;
    }
    let stream = reader.into_inner();
    Ok(Http1Exchange {
        status: head.status,
        body,
        stream,
        can_reuse,
    })
}

struct ParsedHttpHead {
    status: u16,
    is_http11: bool,
    is_http10: bool,
    is_chunked: bool,
    content_length: Option<u64>,
    connection_close: bool,
    connection_keep_alive: bool,
}

async fn read_http_head<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<ParsedHttpHead, ExchangeError> {
    let mut status_line = Vec::with_capacity(64);
    let n = read_crlf_line_bounded(reader, &mut status_line, 8192)
        .await
        .map_err(|e| ExchangeError::StaleBeforeResponse(format!("read status line: {e}")))?;
    if n == 0 {
        return Err(ExchangeError::StaleBeforeResponse(
            "unexpected EOF before HTTP status line".into(),
        ));
    }
    let status_str = String::from_utf8_lossy(&status_line);
    let mut parts = status_str.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| {
            ExchangeError::Transport(format!("malformed HTTP status line: {status_str:?}"))
        })?;
    let is_http11 = version.eq_ignore_ascii_case("HTTP/1.1");
    let is_http10 = version.eq_ignore_ascii_case("HTTP/1.0");

    let mut is_chunked = false;
    let mut content_length: Option<u64> = None;
    let mut connection_close = false;
    let mut connection_keep_alive = false;
    let mut total_bytes = n;
    let mut line_buf = Vec::with_capacity(256);

    loop {
        line_buf.clear();
        let read = read_crlf_line_bounded(reader, &mut line_buf, MAX_HEADER_BYTES)
            .await
            .map_err(|e| ExchangeError::Transport(format!("read response header: {e}")))?;
        if read == 0 {
            return Err(ExchangeError::Transport(
                "unexpected EOF inside HTTP headers".into(),
            ));
        }
        total_bytes = total_bytes.saturating_add(read);
        if total_bytes > MAX_HEADER_BYTES {
            return Err(ExchangeError::Transport(
                "HTTP response headers exceeded 64 KiB limit".into(),
            ));
        }
        if line_buf.is_empty() {
            break;
        }
        if let Ok(line) = std::str::from_utf8(&line_buf) {
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim();
                let value = value.trim();
                if name.eq_ignore_ascii_case("transfer-encoding") {
                    if value
                        .split(',')
                        .any(|t| t.trim().eq_ignore_ascii_case("chunked"))
                    {
                        is_chunked = true;
                    }
                } else if name.eq_ignore_ascii_case("content-length") {
                    if let Ok(cl) = value.parse::<u64>() {
                        content_length = Some(cl);
                    }
                } else if name.eq_ignore_ascii_case("connection") {
                    for tok in value.split(',') {
                        let t = tok.trim();
                        if t.eq_ignore_ascii_case("close") {
                            connection_close = true;
                        } else if t.eq_ignore_ascii_case("keep-alive") {
                            connection_keep_alive = true;
                        }
                    }
                }
            }
        }
    }

    Ok(ParsedHttpHead {
        status,
        is_http11,
        is_http10,
        is_chunked,
        content_length,
        connection_close,
        connection_keep_alive,
    })
}

async fn read_crlf_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_len: usize,
) -> io::Result<usize> {
    out.clear();
    let mut consumed = 0usize;
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            if out.ends_with(b"\r") {
                out.pop();
            }
            return Ok(consumed);
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            if out.len() + pos > max_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP line exceeded limit",
                ));
            }
            out.extend_from_slice(&avail[..pos]);
            if out.ends_with(b"\r") {
                out.pop();
            }
            let take = pos + 1;
            reader.consume(take);
            consumed += take;
            return Ok(consumed);
        }
        let len = avail.len();
        if out.len() + len > max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP line exceeded limit",
            ));
        }
        out.extend_from_slice(avail);
        reader.consume(len);
        consumed += len;
    }
}

async fn read_chunked_body<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut line_buf = Vec::with_capacity(64);
    loop {
        let n = read_crlf_line_bounded(reader, &mut line_buf, 1024)
            .await
            .map_err(|e| format!("read chunk size: {e}"))?;
        if n == 0 {
            return Err("incomplete chunked response (missing 0 terminator)".into());
        }
        let size_str = std::str::from_utf8(&line_buf)
            .map_err(|e| format!("chunk size utf8: {e}"))?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|e| format!("chunk size hex {size_str:?}: {e}"))?;
        if size == 0 {
            // Drain optional trailer headers until terminating blank CRLF line.
            loop {
                let tn = read_crlf_line_bounded(reader, &mut line_buf, 8192)
                    .await
                    .map_err(|e| format!("read chunk trailer: {e}"))?;
                if tn == 0 || line_buf.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        if (out.len() as u64).saturating_add(size as u64) > max_bytes {
            return Err(format!(
                "peer response exceeded {} MiB limit",
                max_bytes / (1024 * 1024)
            ));
        }
        let start = out.len();
        out.resize(start + size, 0);
        reader
            .read_exact(&mut out[start..start + size])
            .await
            .map_err(|e| format!("read chunk payload: {e}"))?;
        // Consume trailing CRLF after chunk payload.
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .await
            .map_err(|e| format!("read chunk CRLF: {e}"))?;
        if &crlf != b"\r\n" {
            return Err("malformed chunked body (expected CRLF after chunk)".into());
        }
    }
}

fn curl_escape_config_val(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

async fn curl_http_json(
    method: &str,
    url: &str,
    host_override: Option<&str>,
    token: Option<&str>,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, PeerHttpError> {
    validate_header_value("peer URL", url)?;
    if let Some(tok) = token {
        validate_header_value("peer token", tok)?;
    }
    if let Some(h) = host_override {
        validate_header_value("Host header", h)?;
    }

    let mut cfg_stdin = format!("url = \"{}\"\n", curl_escape_config_val(url));
    if let Some(h) = host_override.map(str::trim).filter(|s| !s.is_empty()) {
        cfg_stdin.push_str(&format!(
            "header = \"Host: {}\"\n",
            curl_escape_config_val(h)
        ));
    }
    if let Some(tok) = token {
        cfg_stdin.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_escape_config_val(tok)
        ));
    }
    if let Some(val) = body {
        let body_str = serde_json::to_string(val)
            .map_err(|e| PeerHttpError::Transport(format!("encode body: {e}")))?;
        let origin_host = host_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                url.strip_prefix("https://")
                    .and_then(|rest| rest.split('/').next())
                    .filter(|h| !h.is_empty())
            });
        if let Some(oh) = origin_host {
            cfg_stdin.push_str(&format!(
                "header = \"Origin: https://{}\"\n",
                curl_escape_config_val(oh)
            ));
        }
        cfg_stdin.push_str("header = \"Content-Type: application/json\"\n");
        cfg_stdin.push_str(&format!(
            "data-raw = \"{}\"\n",
            curl_escape_config_val(&body_str)
        ));
    }

    let max_time = timeout.as_secs().max(1).to_string();
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        &max_time,
        "-X",
        method,
        "-w",
        "\n%{http_code}",
        "-K",
        "-",
    ])
    .kill_on_drop(true);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PeerHttpError::Transport(format!("spawn curl: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg_stdin.as_bytes()).await;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| PeerHttpError::Transport(format!("wait curl: {e}")))?;
    if !out.status.success() {
        return Err(PeerHttpError::Transport(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let full = String::from_utf8_lossy(&out.stdout);
    let (body_part, code_part) = full.rsplit_once('\n').unwrap_or(("", full.as_ref()));
    let status = code_part.trim().parse::<u16>().unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(PeerHttpError::Http {
            status,
            body: body_part.trim().to_string(),
        });
    }
    serde_json::from_str(body_part)
        .map_err(|e| PeerHttpError::Transport(format!("parse curl JSON: {e}")))
}

// ---------------------------------------------------------------------------
// 2. PeerSseSubscriber (Persistent /api/events SSE stream + Last-Event-Id)
// ---------------------------------------------------------------------------

/// A single decoded Server-Sent Event frame from a peer `GET /api/events` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSseFrame {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

impl PeerSseFrame {
    /// Decode `data` into a [`DaemonEvent`] (matches `starfaned` `/api/events` JSON payload).
    pub fn parse_daemon_event(&self) -> Result<DaemonEvent, serde_json::Error> {
        serde_json::from_str(&self.data)
    }

    /// True when the peer emitted a `resync` frame indicating the subscriber lagged.
    pub fn is_resync(&self) -> bool {
        self.event == "resync"
    }

    /// True when the peer emitted an SSE keep-alive `ping` heartbeat frame.
    pub fn is_ping(&self) -> bool {
        self.event == "ping"
    }
}

/// Lifecycle + data signal emitted by [`PeerSseSubscriber::run_signals`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSseSignal {
    /// SSE HTTP 200 stream established (`reconnected == true` on any reconnect after the first).
    Connected { endpoint: String, reconnected: bool },
    /// Parsed SSE frame from the peer.
    Frame(PeerSseFrame),
    /// Stream dropped or failed; subscriber will retry after `retry_in`.
    Disconnected {
        endpoint: String,
        error: String,
        retry_in: Duration,
    },
}

/// Endpoint descriptor for [`PeerSseSubscriber`] (supports loopback `Host` header override).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSseEndpoint {
    pub base_url: String,
    pub host_override: Option<String>,
    pub token: Option<String>,
}

impl PeerSseEndpoint {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_string(),
            host_override: None,
            token,
        }
    }

    pub fn with_host_override(mut self, host_override: Option<String>) -> Self {
        self.host_override = host_override;
        self
    }
}

/// Persistent SSE subscriber for remote peer `starfaned` `/api/events` streams.
///
/// Supports `Last-Event-Id` replay across reconnects, exponential backoff (`1s -> 2s -> 5s -> 15s`),
/// chunked and identity HTTP/1.1 framing, multi-endpoint failover, and reusable line buffers.
#[derive(Debug, Clone)]
pub struct PeerSseSubscriber {
    endpoints: Vec<PeerSseEndpoint>,
    connect_timeout: Duration,
    read_idle_timeout: Duration,
    backoff_schedule: Vec<Duration>,
    last_event_id: Arc<Mutex<Option<String>>>,
}

impl PeerSseSubscriber {
    pub fn new(endpoint: impl Into<String>, token: Option<String>) -> Self {
        Self::with_endpoints(vec![PeerSseEndpoint::new(endpoint, token)])
    }

    pub fn with_endpoints(endpoints: Vec<PeerSseEndpoint>) -> Self {
        let cleaned = endpoints
            .into_iter()
            .filter(|e| !e.base_url.trim().is_empty())
            .map(|mut e| {
                e.base_url = e.base_url.trim().trim_end_matches('/').to_string();
                e
            })
            .collect();
        Self {
            endpoints: cleaned,
            connect_timeout: DEFAULT_PEER_HTTP_TIMEOUT,
            read_idle_timeout: DEFAULT_SSE_READ_IDLE_TIMEOUT,
            backoff_schedule: DEFAULT_SSE_BACKOFF_SECS
                .iter()
                .map(|&s| Duration::from_secs(s))
                .collect(),
            last_event_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_host_override(mut self, host_override: Option<String>) -> Self {
        for ep in &mut self.endpoints {
            ep.host_override = host_override.clone();
        }
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.max(Duration::from_millis(100));
        self
    }

    pub fn with_read_idle_timeout(mut self, timeout: Duration) -> Self {
        self.read_idle_timeout = timeout.max(Duration::from_millis(100));
        self
    }

    pub fn with_backoff_schedule(mut self, schedule: Vec<Duration>) -> Self {
        if !schedule.is_empty() {
            self.backoff_schedule = schedule;
        }
        self
    }

    pub fn with_initial_last_event_id(self, id: Option<String>) -> Self {
        *self.last_event_id.lock().unwrap_or_else(|p| p.into_inner()) = id;
        self
    }

    pub fn last_event_id(&self) -> Option<String> {
        self.last_event_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Spawn a background task forwarding parsed [`PeerSseFrame`]s into `tx` until `tx` closes.
    pub fn spawn(self, tx: mpsc::UnboundedSender<PeerSseFrame>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run(tx).await;
        })
    }

    /// Spawn a background task forwarding [`PeerSseSignal`]s (connect/frame/disconnect) into `tx`.
    pub fn spawn_signals(
        self,
        tx: mpsc::UnboundedSender<PeerSseSignal>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_signals(tx).await;
        })
    }

    /// Run persistently emitting parsed [`PeerSseFrame`]s until `tx` is closed.
    pub async fn run(self, tx: mpsc::UnboundedSender<PeerSseFrame>) {
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
        let runner = tokio::spawn(async move {
            self.run_signals(sig_tx).await;
        });
        while let Some(sig) = sig_rx.recv().await {
            if let PeerSseSignal::Frame(frame) = sig {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        }
        drop(sig_rx);
        runner.abort();
    }

    /// Run persistently emitting [`PeerSseSignal`]s until `tx` is closed.
    pub async fn run_signals(self, tx: mpsc::UnboundedSender<PeerSseSignal>) {
        if self.endpoints.is_empty() {
            return;
        }
        let mut active_idx = 0usize;
        let mut backoff_idx = 0usize;
        let mut ever_connected = false;

        while !tx.is_closed() {
            let ep = &self.endpoints[active_idx];
            let last_id = self.last_event_id();
            let mut connected_this_round = false;

            let res = self
                .stream_once(ep, last_id.as_deref(), &tx, ever_connected, || {
                    connected_this_round = true;
                })
                .await;

            if tx.is_closed() {
                return;
            }
            if connected_this_round {
                ever_connected = true;
                backoff_idx = 0;
            }
            let err_msg = match res {
                Ok(()) => "SSE stream ended (EOF)".to_string(),
                Err(e) => e.to_string(),
            };
            let retry_in = self
                .backoff_schedule
                .get(backoff_idx)
                .copied()
                .unwrap_or_else(|| {
                    self.backoff_schedule
                        .last()
                        .copied()
                        .unwrap_or(Duration::from_secs(15))
                });
            if backoff_idx + 1 < self.backoff_schedule.len() {
                backoff_idx += 1;
            }
            if tx
                .send(PeerSseSignal::Disconnected {
                    endpoint: ep.base_url.clone(),
                    error: err_msg,
                    retry_in,
                })
                .is_err()
            {
                return;
            }
            if !connected_this_round && self.endpoints.len() > 1 {
                active_idx = (active_idx + 1) % self.endpoints.len();
            }

            tokio::select! {
                _ = tokio::time::sleep(retry_in) => {}
                _ = tx.closed() => return,
            }
        }
    }

    async fn stream_once(
        &self,
        ep: &PeerSseEndpoint,
        last_event_id: Option<&str>,
        tx: &mpsc::UnboundedSender<PeerSseSignal>,
        ever_connected: bool,
        mut on_connected: impl FnMut(),
    ) -> Result<(), PeerHttpError> {
        if let Some(rest) = ep.base_url.strip_prefix("http://") {
            let full = format!("{rest}/api/events");
            let parsed = parse_http_url(&full)?;
            let host_header = ep
                .host_override
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(parsed.host_port);
            validate_header_value("Host header", host_header)?;
            if let Some(tok) = ep.token.as_deref() {
                validate_header_value("bearer token", tok)?;
            }
            if let Some(lid) = last_event_id {
                validate_header_value("Last-Event-Id", lid)?;
            }

            let mut stream = tokio::time::timeout(
                self.connect_timeout,
                TcpStream::connect(&parsed.connect_addr),
            )
            .await
            .map_err(|_| {
                PeerHttpError::Transport(format!(
                    "SSE connect {} timeout after {}s",
                    parsed.connect_addr,
                    self.connect_timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                PeerHttpError::Transport(format!("SSE connect {}: {e}", parsed.connect_addr))
            })?;
            let _ = stream.set_nodelay(true);

            let mut req = format!(
                "GET {} HTTP/1.1\r\nHost: {host_header}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n",
                parsed.path
            );
            if let Some(tok) = ep.token.as_deref() {
                req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
            }
            if let Some(lid) = last_event_id {
                req.push_str(&format!("Last-Event-Id: {lid}\r\n"));
            }
            req.push_str("\r\n");

            tokio::time::timeout(self.connect_timeout, stream.write_all(req.as_bytes()))
                .await
                .map_err(|_| PeerHttpError::Transport("SSE request write timeout".into()))?
                .map_err(|e| PeerHttpError::Transport(format!("SSE write request: {e}")))?;

            let mut reader = BufReader::with_capacity(16 * 1024, stream);
            let head = tokio::time::timeout(self.connect_timeout, read_http_head(&mut reader))
                .await
                .map_err(|_| PeerHttpError::Transport("SSE response header timeout".into()))?
                .map_err(|e| match e {
                    ExchangeError::StaleBeforeResponse(m) | ExchangeError::Transport(m) => {
                        PeerHttpError::Transport(m)
                    }
                })?;

            if head.status != 200 {
                return Err(PeerHttpError::Http {
                    status: head.status,
                    body: format!("unexpected SSE status {}", head.status),
                });
            }

            on_connected();
            if tx
                .send(PeerSseSignal::Connected {
                    endpoint: ep.base_url.clone(),
                    reconnected: ever_connected,
                })
                .is_err()
            {
                return Ok(());
            }

            let mut body_reader = SseBodyLineReader::new(reader, head.is_chunked);
            self.pump_sse_lines(&mut body_reader, tx).await
        } else if ep.base_url.starts_with("https://") {
            self.stream_https_curl(ep, last_event_id, tx, ever_connected, on_connected)
                .await
        } else {
            Err(PeerHttpError::Transport(format!(
                "unsupported SSE endpoint scheme: {}",
                ep.base_url
            )))
        }
    }

    async fn stream_https_curl(
        &self,
        ep: &PeerSseEndpoint,
        last_event_id: Option<&str>,
        tx: &mpsc::UnboundedSender<PeerSseSignal>,
        ever_connected: bool,
        mut on_connected: impl FnMut(),
    ) -> Result<(), PeerHttpError> {
        let url = format!("{}/api/events", ep.base_url);
        validate_header_value("peer URL", &url)?;
        let mut cfg_stdin = format!("url = \"{}\"\n", curl_escape_config_val(&url));
        cfg_stdin.push_str("header = \"Accept: text/event-stream\"\n");
        cfg_stdin.push_str("header = \"Cache-Control: no-cache\"\n");
        if let Some(h) = ep
            .host_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            validate_header_value("Host header", h)?;
            cfg_stdin.push_str(&format!(
                "header = \"Host: {}\"\n",
                curl_escape_config_val(h)
            ));
        }
        if let Some(tok) = ep.token.as_deref() {
            validate_header_value("peer token", tok)?;
            cfg_stdin.push_str(&format!(
                "header = \"Authorization: Bearer {}\"\n",
                curl_escape_config_val(tok)
            ));
        }
        if let Some(lid) = last_event_id {
            validate_header_value("Last-Event-Id", lid)?;
            cfg_stdin.push_str(&format!(
                "header = \"Last-Event-Id: {}\"\n",
                curl_escape_config_val(lid)
            ));
        }

        let connect_secs = self.connect_timeout.as_secs().max(1).to_string();
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args([
            "-sS",
            "-N",
            "--no-buffer",
            "--fail-with-body",
            "--connect-timeout",
            &connect_secs,
            "-K",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| PeerHttpError::Transport(format!("spawn curl SSE: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(cfg_stdin.as_bytes()).await;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PeerHttpError::Transport("curl SSE missing stdout".into()))?;
        let mut body_reader =
            SseBodyLineReader::new(BufReader::with_capacity(16 * 1024, stdout), false);

        on_connected();
        if tx
            .send(PeerSseSignal::Connected {
                endpoint: ep.base_url.clone(),
                reconnected: ever_connected,
            })
            .is_err()
        {
            return Ok(());
        }
        self.pump_sse_lines(&mut body_reader, tx).await
    }

    async fn pump_sse_lines<R: AsyncBufRead + Unpin>(
        &self,
        body_reader: &mut SseBodyLineReader<R>,
        tx: &mpsc::UnboundedSender<PeerSseSignal>,
    ) -> Result<(), PeerHttpError> {
        let mut line_buf = Vec::with_capacity(4096);
        let mut parser = SseFrameBuilder::default();

        loop {
            let read_res = tokio::select! {
                _ = tx.closed() => return Ok(()),
                res = tokio::time::timeout(
                    self.read_idle_timeout,
                    body_reader.read_line_into(&mut line_buf),
                ) => res,
            };
            let got_line = read_res
                .map_err(|_| {
                    PeerHttpError::Transport(format!(
                        "SSE stream idle timeout after {}s",
                        self.read_idle_timeout.as_secs()
                    ))
                })?
                .map_err(|e| PeerHttpError::Transport(format!("SSE read error: {e}")))?;
            if !got_line {
                return Ok(());
            }

            let line = String::from_utf8_lossy(&line_buf);
            if let Some(frame) = parser.feed_line(&line) {
                if let Some(ref id) = frame.id {
                    *self.last_event_id.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(id.clone());
                }
                if frame.is_ping() {
                    continue;
                }
                if tx.send(PeerSseSignal::Frame(frame)).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Reusable-buffer SSE frame accumulator (zero per-line allocations across frames).
#[derive(Debug, Default)]
struct SseFrameBuilder {
    event: String,
    data: String,
    id: Option<String>,
    has_data: bool,
    has_event: bool,
}

impl SseFrameBuilder {
    fn feed_line(&mut self, line: &str) -> Option<PeerSseFrame> {
        if line.is_empty() {
            if !self.has_data && !self.has_event {
                return None;
            }
            let event = if self.has_event && !self.event.is_empty() {
                self.event.clone()
            } else {
                "message".to_string()
            };
            let frame = PeerSseFrame {
                id: self.id.clone(),
                event,
                data: self.data.clone(),
            };
            self.event.clear();
            self.data.clear();
            self.has_data = false;
            self.has_event = false;
            return Some(frame);
        }
        if line.starts_with(':') {
            // Keep-alive / SSE comment line.
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => {
                self.event.clear();
                self.event.push_str(value);
                self.has_event = true;
            }
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" if !value.contains('\0') => {
                self.id = Some(value.to_string());
            }
            _ => {}
        }
        None
    }
}

/// Incremental line reader supporting both `Transfer-Encoding: chunked` and raw identity streams
/// while reusing a single pre-allocated byte buffer across lines.
struct SseBodyLineReader<R> {
    reader: R,
    chunked: bool,
    chunk_rem: usize,
    eof: bool,
    scratch: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> SseBodyLineReader<R> {
    fn new(reader: R, chunked: bool) -> Self {
        Self {
            reader,
            chunked,
            chunk_rem: 0,
            eof: false,
            scratch: Vec::with_capacity(64),
        }
    }

    /// Read the next `\n`- or `\r\n`-delimited line into `out` (cleared first, newline stripped).
    /// Returns `Ok(true)` if a line was read, `Ok(false)` on clean EOF.
    async fn read_line_into(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        if self.eof {
            return Ok(false);
        }
        if !self.chunked {
            let n = read_crlf_line_bounded(&mut self.reader, out, MAX_SSE_LINE_BYTES).await?;
            if n == 0 {
                self.eof = true;
                return Ok(false);
            }
            return Ok(true);
        }

        let mut saw_any_byte = false;
        loop {
            if self.chunk_rem == 0 {
                let n = read_crlf_line_bounded(&mut self.reader, &mut self.scratch, 1024).await?;
                if n == 0 {
                    self.eof = true;
                    return Ok(saw_any_byte);
                }
                let size_str = std::str::from_utf8(&self.scratch)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim();
                let size = usize::from_str_radix(size_str, 16).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid SSE chunk size {size_str:?}: {e}"),
                    )
                })?;
                if size == 0 {
                    // Drain optional chunk trailer until empty line.
                    loop {
                        let tn = read_crlf_line_bounded(&mut self.reader, &mut self.scratch, 4096)
                            .await?;
                        if tn == 0 || self.scratch.is_empty() {
                            break;
                        }
                    }
                    self.eof = true;
                    return Ok(saw_any_byte);
                }
                self.chunk_rem = size;
            }

            let avail = self.reader.fill_buf().await?;
            if avail.is_empty() {
                self.eof = true;
                return Ok(saw_any_byte);
            }
            let slice_len = avail.len().min(self.chunk_rem);
            let slice = &avail[..slice_len];
            saw_any_byte = true;

            if let Some(nl_pos) = slice.iter().position(|&b| b == b'\n') {
                if out.len() + nl_pos > MAX_SSE_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SSE line exceeded limit",
                    ));
                }
                out.extend_from_slice(&slice[..nl_pos]);
                if out.ends_with(b"\r") {
                    out.pop();
                }
                let take = nl_pos + 1;
                self.reader.consume(take);
                self.chunk_rem -= take;
                if self.chunk_rem == 0 {
                    consume_chunk_crlf(&mut self.reader).await?;
                }
                return Ok(true);
            }

            if out.len() + slice_len > MAX_SSE_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SSE line exceeded limit",
                ));
            }
            out.extend_from_slice(slice);
            self.reader.consume(slice_len);
            self.chunk_rem -= slice_len;
            if self.chunk_rem == 0 {
                consume_chunk_crlf(&mut self.reader).await?;
            }
        }
    }
}

async fn consume_chunk_crlf<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<()> {
    let mut crlf = [0u8; 2];
    reader.read_exact(&mut crlf).await?;
    if &crlf != b"\r\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed SSE chunk terminator (expected CRLF)",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. SshLoopbackTunnel (Atomic 1-script SSH bootstrap + ControlMaster forward)
// ---------------------------------------------------------------------------

/// Parsed output of the atomic remote `starfane daemon` bootstrap script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBootstrapInfo {
    pub remote_bin: String,
    pub remote_port: u16,
    pub auth_enabled: bool,
    pub token: Option<String>,
}

/// Active loopback tunnel endpoint returned by [`SshLoopbackTunnel::ensure_endpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelEndpoint {
    /// Local loopback base URL (e.g. `"http://127.0.0.1:54321"`).
    pub local_endpoint: String,
    pub local_port: u16,
    pub remote_port: u16,
    /// Authoritative remote `Host` header (`"127.0.0.1:<remote_port>"`) satisfying remote
    /// `starfaned`'s DNS-rebinding `AccessGate` over a dynamically forwarded local port.
    pub host_header: String,
    /// Optional remote bearer token read from `~/.local/state/starfane/web.token` when auth is enabled.
    pub token: Option<String>,
    pub remote_bin: String,
}

#[derive(Debug, Clone)]
struct ActiveTunnel {
    endpoint: TunnelEndpoint,
}

/// Zero-open-port SSH ControlMaster loopback tunnel to a remote `starfaned` peer daemon.
///
/// 1. Runs 1 atomic POSIX `/bin/sh` bootstrap script over `SshExec` (warming `ControlMaster`):
///    locates `starfane` (`~/.local/bin/starfane`, `/opt/homebrew/bin/starfane`, `command -v starfane`),
///    starts `starfane daemon --detach` if not running, and reads remote `PORT` + `web.token`.
/// 2. Opens a dynamic loopback forward over the existing `ControlMaster` socket:
///    `ssh -O forward -o ControlPath=<sock> -L 127.0.0.1:0:127.0.0.1:<remote_port> <target>`.
/// 3. Cancels the forward cleanly via `ssh -O cancel` on [`SshLoopbackTunnel::teardown`] or `Drop`.
#[derive(Debug)]
pub struct SshLoopbackTunnel {
    ssh: SshExec,
    requested_remote_port: Option<u16>,
    active: Mutex<Option<ActiveTunnel>>,
    connect_lock: tokio::sync::Mutex<()>,
}

impl SshLoopbackTunnel {
    pub fn new(ssh: SshExec) -> Self {
        Self {
            ssh,
            requested_remote_port: None,
            active: Mutex::new(None),
            connect_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Optional explicit remote port override (when `None`, reads `[web].port` from remote config
    /// or defaults to `27418`).
    pub fn with_remote_port(mut self, remote_port: Option<u16>) -> Self {
        self.requested_remote_port = remote_port.filter(|&p| p > 0);
        self
    }

    pub fn ssh(&self) -> &SshExec {
        &self.ssh
    }

    /// Non-blocking snapshot of the currently established loopback tunnel, if any.
    pub fn current_endpoint(&self) -> Option<TunnelEndpoint> {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|a| a.endpoint.clone())
    }

    /// Ensure the remote `starfaned` is running and the local `ControlMaster` loopback forward
    /// is established, returning the cached [`TunnelEndpoint`] when already active.
    pub async fn ensure_endpoint(&self) -> Result<TunnelEndpoint, PeerHttpError> {
        if let Some(ep) = self.current_endpoint() {
            return Ok(ep);
        }
        let _guard = self.connect_lock.lock().await;
        if let Some(ep) = self.current_endpoint() {
            return Ok(ep);
        }
        self.establish_locked().await
    }

    /// Tear down any existing forward, re-bootstrap remote `starfaned`, and open a fresh forward.
    pub async fn reconnect(&self) -> Result<TunnelEndpoint, PeerHttpError> {
        let _guard = self.connect_lock.lock().await;
        self.cancel_active_async().await;
        self.establish_locked().await
    }

    /// Cancel the active loopback forward cleanly (`ssh -O cancel`).
    pub async fn teardown(&self) {
        let _guard = self.connect_lock.lock().await;
        self.cancel_active_async().await;
    }

    /// Synchronous best-effort teardown (used by `Drop`).
    pub fn close_sync(&self) {
        let prev = self.active.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(active) = prev {
            cancel_forward_sync(
                self.ssh.ssh_bin(),
                self.ssh.control_path(),
                self.ssh.target(),
                active.endpoint.local_port,
                active.endpoint.remote_port,
            );
        }
    }

    async fn cancel_active_async(&self) {
        let prev = self.active.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(active) = prev {
            let ssh_bin = self.ssh.ssh_bin().to_path_buf();
            let control_path = self.ssh.control_path().to_path_buf();
            let target = self.ssh.target().to_string();
            let local_port = active.endpoint.local_port;
            let remote_port = active.endpoint.remote_port;
            let _ = tokio::task::spawn_blocking(move || {
                cancel_forward_sync(&ssh_bin, &control_path, &target, local_port, remote_port);
            })
            .await;
        }
    }

    async fn establish_locked(&self) -> Result<TunnelEndpoint, PeerHttpError> {
        let script = build_bootstrap_script(self.requested_remote_port);
        let ssh = self.ssh.clone();
        let boot_out = tokio::task::spawn_blocking(move || ssh.run_shell(&script))
            .await
            .map_err(|e| PeerHttpError::Transport(format!("ssh bootstrap join: {e}")))?
            .map_err(|e| PeerHttpError::Transport(format!("ssh bootstrap failed: {e}")))?;

        if !boot_out.success {
            let detail = if !boot_out.stderr.trim().is_empty() {
                boot_out.stderr.trim().to_string()
            } else {
                boot_out.stdout.trim().to_string()
            };
            return Err(PeerHttpError::Transport(format!(
                "remote starfane bootstrap failed (exit {:?}): {detail}",
                boot_out.code
            )));
        }
        let info = parse_bootstrap_output(&boot_out.stdout)?;

        // Open dynamic loopback forward over the warm ControlMaster socket.
        let local_port = match self.open_forward_once(info.remote_port).await {
            Ok(p) => p,
            Err(first_err) => {
                // Retry once after clearing any stale mux socket and re-warming ControlMaster.
                let ssh_retry = self.ssh.clone();
                let script_retry = build_bootstrap_script(Some(info.remote_port));
                let _ = tokio::task::spawn_blocking(move || {
                    ssh_retry.clear_stale_control_socket();
                    let _ = ssh_retry.run_shell(&script_retry);
                })
                .await;
                self.open_forward_once(info.remote_port)
                    .await
                    .map_err(|e| {
                        PeerHttpError::Transport(format!(
                            "ssh -O forward failed: {e} (initial: {first_err})"
                        ))
                    })?
            }
        };

        let endpoint = TunnelEndpoint {
            local_endpoint: format!("http://127.0.0.1:{local_port}"),
            local_port,
            remote_port: info.remote_port,
            host_header: format!("127.0.0.1:{}", info.remote_port),
            token: if info.auth_enabled { info.token } else { None },
            remote_bin: info.remote_bin,
        };
        *self.active.lock().unwrap_or_else(|p| p.into_inner()) = Some(ActiveTunnel {
            endpoint: endpoint.clone(),
        });
        Ok(endpoint)
    }

    async fn open_forward_once(&self, remote_port: u16) -> Result<u16, PeerHttpError> {
        let forward_spec = format!("127.0.0.1:0:127.0.0.1:{remote_port}");
        let mut cmd = tokio::process::Command::new(self.ssh.ssh_bin());
        cmd.args([
            "-O",
            "forward",
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ControlPath={}", self.ssh.control_path().display()),
            "-L",
            &forward_spec,
            self.ssh.target(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

        let out = tokio::time::timeout(TUNNEL_OP_TIMEOUT, cmd.output())
            .await
            .map_err(|_| {
                PeerHttpError::Transport(format!(
                    "ssh -O forward timeout after {}s",
                    TUNNEL_OP_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| PeerHttpError::Transport(format!("spawn ssh -O forward: {e}")))?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            return Err(PeerHttpError::Transport(format!(
                "ssh -O forward exit {:?}: {}",
                out.status.code(),
                stderr.trim()
            )));
        }
        parse_forward_port(&stdout, &stderr)
    }
}

impl Drop for SshLoopbackTunnel {
    fn drop(&mut self) {
        self.close_sync();
    }
}

fn cancel_forward_sync(
    ssh_bin: &Path,
    control_path: &Path,
    target: &str,
    local_port: u16,
    remote_port: u16,
) {
    let forward_spec = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
    let Ok(mut child) = StdCommand::new(ssh_bin)
        .args([
            "-O",
            "cancel",
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ControlPath={}", control_path.display()),
            "-L",
            &forward_spec,
            target,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = Instant::now() + TUNNEL_CANCEL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                return;
            }
        }
    }
}

/// Build the 1-script atomic POSIX `/bin/sh` remote bootstrap script used by [`SshLoopbackTunnel`].
pub fn build_bootstrap_script(requested_port: Option<u16>) -> String {
    let default_port = crate::config::DEFAULT_WEB_PORT;
    let req_port_line = match requested_port.filter(|&p| p > 0) {
        Some(p) => format!("SF_REQ_PORT='{p}'\n"),
        None => "SF_REQ_PORT=''\n".to_string(),
    };
    format!(
        r#"for _d in "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin "$HOME/.cargo/bin" "$HOME/bin"; do
  [ -d "$_d" ] && case ":$PATH:" in *":$_d:"*) ;; *) PATH="$_d:$PATH" ;; esac
done
export PATH
SF_BIN=""
for _cand in "$HOME/.local/bin/starfane" /opt/homebrew/bin/starfane /usr/local/bin/starfane "$HOME/.cargo/bin/starfane"; do
  if [ -x "$_cand" ]; then
    SF_BIN="$_cand"
    break
  fi
done
if [ -z "$SF_BIN" ]; then
  SF_BIN="$(command -v starfane 2>/dev/null || true)"
fi
if [ -z "$SF_BIN" ]; then
  printf '%s\nERR=starfane binary not found\n%s\n' '{BOOT_BEGIN_MARKER}' '{BOOT_END_MARKER}'
  exit 127
fi
SF_STATE="${{STARFANE_STATE_DIR:-$HOME/.local/state/starfane}}"
SF_SOCK="${{STARFANE_SOCKET:-$SF_STATE/starfaned.sock}}"
SF_CFG="${{STARFANE_CONFIG:-${{XDG_CONFIG_HOME:-$HOME/.config}}/starfane/config.toml}}"
{req_port_line}SF_PORT="{default_port}"
SF_AUTH="1"
if [ -r "$SF_CFG" ]; then
  _cfg_vals="$(awk '
    /^[[:space:]]*\[/ {{ in_web = ($0 ~ /^[[:space:]]*\[web\][[:space:]]*(#.*)?$/); next }}
    in_web && /^[[:space:]]*port[[:space:]]*=/ {{
      v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]*(#.*)?$/, "", v);
      if (v ~ /^[0-9]+$/) port = v;
    }}
    in_web && /^[[:space:]]*auth_enabled[[:space:]]*=/ {{
      v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]*(#.*)?$/, "", v);
      if (v == "false") auth = "0"; else if (v == "true") auth = "1";
    }}
    END {{ printf "%s %s\n", port, auth }}
  ' "$SF_CFG" 2>/dev/null || true)"
  _cp="$(printf '%s' "$_cfg_vals" | awk '{{print $1}}')"
  _ca="$(printf '%s' "$_cfg_vals" | awk '{{print $2}}')"
  [ -n "$_cp" ] && SF_PORT="$_cp"
  [ -n "$_ca" ] && SF_AUTH="$_ca"
fi
if [ -n "$SF_REQ_PORT" ]; then
  SF_PORT="$SF_REQ_PORT"
fi
SF_RUNNING="0"
if [ -S "$SF_SOCK" ] && [ -r "$SF_STATE/starfaned.pid" ]; then
  _pid="$(tr -d '[:space:]' < "$SF_STATE/starfaned.pid" 2>/dev/null || true)"
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null; then
    SF_RUNNING="1"
  fi
fi
if [ "$SF_RUNNING" != "1" ]; then
  if [ -n "$SF_REQ_PORT" ]; then
    "$SF_BIN" daemon --detach --port "$SF_PORT" >/dev/null 2>&1 || true
  else
    "$SF_BIN" daemon --detach >/dev/null 2>&1 || true
  fi
  _i=0
  while [ "$_i" -lt 30 ]; do
    if [ -S "$SF_SOCK" ] && [ -s "$SF_STATE/web.token" ]; then
      break
    fi
    sleep 0.1
    _i=$((_i + 1))
  done
fi
SF_TOKEN=""
if [ "$SF_AUTH" != "0" ] && [ -r "$SF_STATE/web.token" ]; then
  SF_TOKEN="$(tr -d '\r\n ' < "$SF_STATE/web.token" 2>/dev/null || true)"
fi
printf '%s\nBIN=%s\nPORT=%s\nAUTH=%s\nTOKEN=%s\n%s\n' \
  '{BOOT_BEGIN_MARKER}' "$SF_BIN" "$SF_PORT" "$SF_AUTH" "$SF_TOKEN" '{BOOT_END_MARKER}'
"#
    )
}

/// Parse stdout emitted by [`build_bootstrap_script`].
pub fn parse_bootstrap_output(stdout: &str) -> Result<RemoteBootstrapInfo, PeerHttpError> {
    let mut in_block = false;
    let mut saw_end = false;
    let mut remote_bin: Option<String> = None;
    let mut remote_port: Option<u16> = None;
    let mut auth_enabled = true;
    let mut token: Option<String> = None;
    let mut err_msg: Option<String> = None;

    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line == BOOT_BEGIN_MARKER {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if line == BOOT_END_MARKER {
            saw_end = true;
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "ERR" => err_msg = Some(v.trim().to_string()),
                "BIN" => {
                    let b = v.trim();
                    if !b.is_empty() {
                        remote_bin = Some(b.to_string());
                    }
                }
                "PORT" => {
                    let p = v.trim().parse::<u16>().map_err(|e| {
                        PeerHttpError::Transport(format!("invalid bootstrap PORT {v:?}: {e}"))
                    })?;
                    if p == 0 {
                        return Err(PeerHttpError::Transport(
                            "bootstrap PORT cannot be 0".into(),
                        ));
                    }
                    remote_port = Some(p);
                }
                "AUTH" => {
                    auth_enabled = !matches!(v.trim(), "0" | "false" | "no");
                }
                "TOKEN" => {
                    let t = v.trim();
                    if !t.is_empty() {
                        token = Some(t.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(err) = err_msg {
        return Err(PeerHttpError::Transport(err));
    }
    if !in_block || !saw_end {
        return Err(PeerHttpError::Transport(format!(
            "incomplete bootstrap output (missing markers): {}",
            stdout.trim()
        )));
    }
    let remote_port = remote_port
        .ok_or_else(|| PeerHttpError::Transport("bootstrap output missing PORT".into()))?;
    let remote_bin = remote_bin.unwrap_or_else(|| "starfane".to_string());
    Ok(RemoteBootstrapInfo {
        remote_bin,
        remote_port,
        auth_enabled,
        token: if auth_enabled { token } else { None },
    })
}

/// Parse the dynamically allocated local port printed by `ssh -O forward -L 127.0.0.1:0:...`.
pub fn parse_forward_port(stdout: &str, stderr: &str) -> Result<u16, PeerHttpError> {
    let extract = |text: &str| -> Option<u16> {
        for line in text.lines() {
            let trimmed = line.trim();
            if let Ok(p) = trimmed.parse::<u16>() {
                if p > 0 {
                    return Some(p);
                }
            }
            for tok in trimmed.split_whitespace() {
                if let Ok(p) = tok.parse::<u16>() {
                    if p > 0 {
                        return Some(p);
                    }
                }
            }
        }
        None
    };
    extract(stdout).or_else(|| extract(stderr)).ok_or_else(|| {
        PeerHttpError::Transport(format!(
            "could not parse forwarded local port from ssh -O forward (stdout={:?}, stderr={:?})",
            stdout.trim(),
            stderr.trim()
        ))
    })
}

// ---------------------------------------------------------------------------
// Tests (in-process TcpListener / axum mock servers + fake-ssh tunnel test)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::response::sse::{Event, Sse};
    use axum::routing::{get, post};
    use axum::Router;
    use std::convert::Infallible;
    use std::os::unix::fs::PermissionsExt;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};
    use tokio::net::TcpListener;

    struct VecSseStream {
        items: std::collections::VecDeque<Event>,
    }

    impl futures_core::Stream for VecSseStream {
        type Item = Result<Event, Infallible>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.items.pop_front().map(Ok))
        }
    }

    #[tokio::test]
    async fn keepalive_pool_reuses_single_tcp_connection_and_measures_rtt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let accepted_srv = accepted.clone();
        let requests_srv = requests.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                accepted_srv.fetch_add(1, Ordering::SeqCst);
                let req_cnt = requests_srv.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut req_line = Vec::new();
                        let Ok(n) = read_crlf_line_bounded(&mut reader, &mut req_line, 4096).await
                        else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        let req_str = String::from_utf8_lossy(&req_line).into_owned();
                        let mut content_len = 0usize;
                        let mut auth_ok = false;
                        let mut host_hdr = String::new();
                        let mut hdr = Vec::new();
                        loop {
                            let Ok(hn) = read_crlf_line_bounded(&mut reader, &mut hdr, 4096).await
                            else {
                                return;
                            };
                            if hn == 0 || hdr.is_empty() {
                                break;
                            }
                            let line = String::from_utf8_lossy(&hdr);
                            if let Some((k, v)) = line.split_once(':') {
                                if k.eq_ignore_ascii_case("content-length") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                } else if k.eq_ignore_ascii_case("authorization")
                                    && v.trim() == "Bearer secret-tok"
                                {
                                    auth_ok = true;
                                } else if k.eq_ignore_ascii_case("host") {
                                    host_hdr = v.trim().to_string();
                                }
                            }
                        }
                        if content_len > 0 {
                            let mut body = vec![0u8; content_len];
                            if reader.read_exact(&mut body).await.is_err() {
                                break;
                            }
                        }
                        let seq = req_cnt.fetch_add(1, Ordering::SeqCst) + 1;
                        let payload = serde_json::json!({
                            "ok": true,
                            "seq": seq,
                            "auth": auth_ok,
                            "host": host_hdr,
                            "chunked": req_str.contains("/chunked"),
                        })
                        .to_string();
                        let resp = if req_str.contains("/chunked") {
                            format!(
                                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                                payload.len(),
                                payload
                            )
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                                payload.len(),
                                payload
                            )
                        };
                        if reader.get_mut().write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let pool = PeerKeepAlivePool::new();
        let base = format!("http://{addr}");

        // 1. Health probe measures pure RTT and warms connection #1.
        let rtt = pool.probe_rtt(&base, Some("secret-tok")).await.unwrap();
        assert!(rtt < 2_000);
        assert!(pool.rtt_ms().is_some());
        assert_eq!(pool.idle_count(), 1);

        // 2. Sequential GET (Content-Length), chunked GET, and POST all reuse the SAME socket!
        let r1 = pool
            .get_json(&format!("{base}/api/state"), Some("secret-tok"))
            .await
            .unwrap();
        assert_eq!(r1["seq"], 2);
        assert_eq!(r1["auth"], true);

        let r2 = pool
            .get_json(&format!("{base}/chunked"), Some("secret-tok"))
            .await
            .unwrap();
        assert_eq!(r2["seq"], 3);
        assert_eq!(r2["chunked"], true);

        let r3 = pool
            .request_json_with_host(
                "POST",
                &format!("{base}/api/dispatch"),
                Some("127.0.0.1:27418"),
                Some("secret-tok"),
                Some(&serde_json::json!({"text": "hi"})),
            )
            .await
            .unwrap();
        assert_eq!(r3["seq"], 4);
        assert_eq!(r3["host"], "127.0.0.1:27418");

        assert_eq!(requests.load(Ordering::SeqCst), 4);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "all 4 sequential requests must reuse 1 Keep-Alive TCP connection"
        );
        assert_eq!(pool.idle_count(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn keepalive_pool_retries_once_on_stale_eof_for_idempotent_get() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_srv = accepted.clone();

        let server = tokio::spawn(async move {
            // Connection #1: serves 1 keep-alive response, then closes immediately after receiving 2nd request!
            let (stream1, _) = listener.accept().await.unwrap();
            accepted_srv.fetch_add(1, Ordering::SeqCst);
            let mut r1 = BufReader::new(stream1);
            let mut buf = Vec::new();
            loop {
                let n = read_crlf_line_bounded(&mut r1, &mut buf, 4096)
                    .await
                    .unwrap();
                if n == 0 || buf.is_empty() {
                    break;
                }
            }
            let body1 = r#"{"ok":true,"conn":1}"#;
            let resp1 = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body1.len(),
                body1
            );
            r1.get_mut().write_all(resp1.as_bytes()).await.unwrap();
            // Wait until the client writes request #2, then drop socket without replying (stale EOF race!).
            let _ = read_crlf_line_bounded(&mut r1, &mut buf, 4096).await;
            drop(r1);

            // Connection #2: serves the automatic retry cleanly.
            let (stream2, _) = listener.accept().await.unwrap();
            accepted_srv.fetch_add(1, Ordering::SeqCst);
            let mut r2 = BufReader::new(stream2);
            loop {
                let n = read_crlf_line_bounded(&mut r2, &mut buf, 4096)
                    .await
                    .unwrap();
                if n == 0 || buf.is_empty() {
                    break;
                }
            }
            let body2 = r#"{"ok":true,"conn":2}"#;
            let resp2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body2.len(),
                body2
            );
            r2.get_mut().write_all(resp2.as_bytes()).await.unwrap();
        });

        let pool = PeerKeepAlivePool::new();
        let url = format!("http://{addr}/api/state");
        let v1 = pool.get_json(&url, None).await.unwrap();
        assert_eq!(v1["conn"], 1);
        let v2 = pool.get_json(&url, None).await.unwrap();
        assert_eq!(v2["conn"], 2);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn sse_subscriber_parses_axum_chunked_sse_and_replays_last_event_id_on_reconnect() {
        let seen_last_ids: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_srv = seen_last_ids.clone();
        let connect_count = Arc::new(AtomicUsize::new(0));
        let cc_srv = connect_count.clone();

        let app = Router::new()
            .route(
                "/api/events",
                get(move |headers: HeaderMap| {
                    let seen = seen_srv.clone();
                    let cc = cc_srv.clone();
                    async move {
                        let lid = headers
                            .get("last-event-id")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        seen.lock().unwrap().push(lid.clone());
                        let attempt = cc.fetch_add(1, Ordering::SeqCst) + 1;

                        let mut items = std::collections::VecDeque::new();
                        if attempt == 1 {
                            items.push_back(
                                Event::default()
                                    .id("10")
                                    .event("output")
                                    .data(r#"{"type":"output","seq":10,"session_id":"s1","dirty":true}"#),
                            );
                            // Stream ends cleanly after event #10 -> triggers reconnect!
                        } else {
                            let expected = lid.as_deref().unwrap_or("");
                            items.push_back(
                                Event::default()
                                    .id("11")
                                    .event("output")
                                    .data(format!(
                                        r#"{{"type":"output","seq":11,"session_id":"replayed-{expected}","dirty":false}}"#
                                    )),
                            );
                        }
                        Sse::new(VecSseStream { items })
                    }
                }),
            )
            .route("/api/ping", post(|| async { r#"{"ok":true}"# }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let sub = PeerSseSubscriber::new(format!("http://{addr}"), Some("tok-1".into()))
            .with_backoff_schedule(vec![Duration::from_millis(25), Duration::from_millis(50)]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = sub.spawn_signals(tx);

        let mut frames = Vec::new();
        let mut saw_reconnect = false;
        let deadline = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => panic!("timed out waiting for SSE frames (got {frames:?})"),
                Some(sig) = rx.recv() => {
                    match sig {
                        PeerSseSignal::Connected { reconnected, .. } => {
                            if reconnected {
                                saw_reconnect = true;
                            }
                        }
                        PeerSseSignal::Frame(f) => {
                            frames.push(f);
                            if frames.len() >= 2 {
                                break;
                            }
                        }
                        PeerSseSignal::Disconnected { .. } => {}
                    }
                }
            }
        }

        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        server.abort();

        assert!(saw_reconnect, "expected SSE subscriber to reconnect");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].id.as_deref(), Some("10"));
        assert_eq!(frames[0].event, "output");
        let ev0 = frames[0].parse_daemon_event().unwrap();
        assert_eq!(ev0.seq(), 10);

        assert_eq!(frames[1].id.as_deref(), Some("11"));
        let ev1 = frames[1].parse_daemon_event().unwrap();
        match ev1 {
            DaemonEvent::Output { session_id, .. } => {
                assert_eq!(session_id.as_str(), "replayed-10");
            }
            other => panic!("unexpected event {other:?}"),
        }

        let recorded = seen_last_ids.lock().unwrap().clone();
        assert!(recorded.len() >= 2);
        assert_eq!(recorded[0], None);
        assert_eq!(recorded[1].as_deref(), Some("10"));
    }

    #[tokio::test]
    async fn peer_sse_subscriber_skips_ping_frames_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let out_ev = DaemonEvent::Output {
                seq: 42,
                session_id: starfane_proto::SessionId::new("tmux-%9"),
                start_seq: Some(0),
                total_messages: Some(1),
                appended_messages: vec![
                    serde_json::json!({"role": "assistant", "text": "after ping"}),
                ],
                live_tail: None,
                metrics: None,
                dirty: false,
            };
            let body = format!(
                "event: ping\ndata: {{}}\n\nevent: ping\ndata:\n\n{}",
                out_ev.to_sse_frame().unwrap()
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n{body}"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let sub = PeerSseSubscriber::new(format!("http://{addr}"), None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = sub.spawn_signals(tx);

        let mut connected = false;
        let mut frames = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => panic!("timed out waiting for non-ping SSE frame"),
                Some(sig) = rx.recv() => {
                    match sig {
                        PeerSseSignal::Connected { .. } => connected = true,
                        PeerSseSignal::Frame(f) => {
                            frames.push(f);
                            break;
                        }
                        PeerSseSignal::Disconnected { error, .. } => {
                            panic!("unexpected disconnect while streaming ping + output: {error}");
                        }
                    }
                }
            }
        }

        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        server.abort();

        assert!(connected);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "output");
        assert_eq!(frames[0].id.as_deref(), Some("42"));
        let ev = frames[0].parse_daemon_event().unwrap();
        assert_eq!(ev.seq(), 42);
    }

    #[test]
    fn bootstrap_and_forward_parsers_handle_auth_and_no_auth() {
        let out = format!(
            "welcome banner\n{BOOT_BEGIN_MARKER}\nBIN=/opt/homebrew/bin/starfane\nPORT=27418\nAUTH=1\nTOKEN=abc123\n{BOOT_END_MARKER}\n"
        );
        let info = parse_bootstrap_output(&out).unwrap();
        assert_eq!(info.remote_bin, "/opt/homebrew/bin/starfane");
        assert_eq!(info.remote_port, 27418);
        assert!(info.auth_enabled);
        assert_eq!(info.token.as_deref(), Some("abc123"));

        let no_auth = format!(
            "{BOOT_BEGIN_MARKER}\nBIN=/usr/local/bin/starfane\nPORT=7878\nAUTH=0\nTOKEN=ignored\n{BOOT_END_MARKER}\n"
        );
        let info2 = parse_bootstrap_output(&no_auth).unwrap();
        assert_eq!(info2.remote_port, 7878);
        assert!(!info2.auth_enabled);
        assert_eq!(info2.token, None);

        let err_out =
            format!("{BOOT_BEGIN_MARKER}\nERR=starfane binary not found\n{BOOT_END_MARKER}\n");
        assert!(parse_bootstrap_output(&err_out).is_err());

        assert_eq!(parse_forward_port("54321\n", "").unwrap(), 54321);
        assert_eq!(
            parse_forward_port("", "Allocated port 55112 for forward\n").unwrap(),
            55112
        );
        assert!(parse_forward_port("no-port", "").is_err());
    }

    #[tokio::test]
    async fn ssh_loopback_tunnel_fake_ssh_bootstrap_forward_and_cancel_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tunnel-test-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("fake_ssh.log");
        let fake_ssh = dir.join("fake-ssh");

        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case " $* " in
  *" -O forward "*)
    printf '51234\n'
    exit 0
    ;;
  *" -O cancel "*)
    exit 0
    ;;
  *" -O exit "*)
    exit 0
    ;;
  *)
    # Consume stdin bootstrap script and return structured markers.
    cat >/dev/null
    printf '{BOOT_BEGIN_MARKER}\nBIN=/home/dev/.local/bin/starfane\nPORT=27418\nAUTH=1\nTOKEN=tok_remote_99\n{BOOT_END_MARKER}\n'
    exit 0
    ;;
esac
"#,
            log_file.display()
        );
        std::fs::write(&fake_ssh, script).unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ssh = SshExec::new("dev@example", dir.join("ctrl.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let tunnel = SshLoopbackTunnel::new(ssh);

        let ep = tunnel.ensure_endpoint().await.unwrap();
        assert_eq!(ep.local_endpoint, "http://127.0.0.1:51234");
        assert_eq!(ep.local_port, 51234);
        assert_eq!(ep.remote_port, 27418);
        assert_eq!(ep.host_header, "127.0.0.1:27418");
        assert_eq!(ep.token.as_deref(), Some("tok_remote_99"));
        assert_eq!(ep.remote_bin, "/home/dev/.local/bin/starfane");

        // Second ensure_endpoint returns cached endpoint without extra SSH calls.
        let ep2 = tunnel.ensure_endpoint().await.unwrap();
        assert_eq!(ep2, ep);

        drop(tunnel);

        let log = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "expected bootstrap + forward + cancel, got:\n{log}"
        );
        assert!(
            lines[0].contains("/bin/sh -s"),
            "call 0 should run bootstrap script: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("-O forward") && lines[1].contains("127.0.0.1:0:127.0.0.1:27418"),
            "call 1 should open dynamic forward: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("-O cancel") && lines[2].contains("127.0.0.1:51234:127.0.0.1:27418"),
            "call 2 should cancel forward on Drop: {}",
            lines[2]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
