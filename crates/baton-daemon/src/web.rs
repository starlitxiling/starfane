//! Embedded Web console: static assets + REST + SSE (contracts §7.2).
//!
//! Security: binds 127.0.0.1 by default; every `/api/*` call except
//! `/api/health` needs the per-start token (cookie `baton_token`, header
//! `X-Baton-Token` or `Authorization: Bearer`). Host is checked against
//! loopback names (DNS rebinding) and POST Origin must be same-origin.

use crate::core::Core;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use baton_proto::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{broadcast, mpsc};

const INDEX_HTML: &[u8] = include_bytes!("../../../web/index.html");
const APP_JS: &[u8] = include_bytes!("../../../web/app.js");
const APP_CSS: &[u8] = include_bytes!("../../../web/app.css");
const LOGO_SVG: &[u8] = include_bytes!("../../../assets/logo.svg");
const COOKIE: &str = "baton_token";

pub async fn serve(core: Core) -> Result<(), String> {
    let bind = core.config().web.bind.trim();
    let port = core.config().web.port;
    let addr = if let Ok(ip) = bind.parse::<IpAddr>() {
        SocketAddr::new(ip, port).to_string()
    } else if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    };
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!(
        "web console on http://{addr}/ (auth_enabled={}, allow_easytier={}, easytier_cidrs={:?}, allow_tailscale={})",
        core.config().web.auth_enabled,
        core.config().web.allow_easytier,
        core.config().web.easytier_cidrs,
        core.config().web.allow_tailscale
    );
    axum::serve(
        listener,
        router(core).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| format!("serve: {e}"))
}

pub fn router(core: Core) -> Router {
    let api = Router::new()
        .route("/api/state", get(state))
        .route("/api/models", get(models))
        .route("/api/slash-commands", get(slash_commands))
        .route("/api/events", get(events))
        .route("/api/sessions", get(sessions))
        .route("/api/tmux/sessions", get(tmux_sessions))
        .route("/api/sessions/spawn", post(spawn_session))
        .route("/api/sessions/{id}/focus", post(focus))
        .route("/api/sessions/{id}/ack", post(ack_session))
        .route("/api/sessions/{id}/transcript", get(session_transcript))
        .route("/api/panes/{pane_id}/preview", get(pane_preview))
        .route("/api/pipelines/parse", post(parse))
        .route("/api/pipelines", get(list_pipelines).post(create_pipeline))
        .route("/api/pipelines/{id}", get(get_pipeline))
        .route("/api/pipelines/{id}/confirm", post(confirm_pipeline))
        .route("/api/pipelines/{id}/action", post(pipeline_action))
        .route("/api/debug", post(create_debug))
        .route("/api/debug/{id}/{verb}", post(debug_verb))
        .route("/api/handoffs", get(list_handoffs).post(create_handoff))
        .route("/api/artifacts/{id}", get(get_artifact))
        .route("/api/reviews", get(list_reviews))
        .route("/api/reviews/precision", get(precision))
        .route("/api/reviews/{id}", get(get_review))
        .route("/api/reviews/{id}/findings/{fid}/feedback", post(feedback))
        .route("/api/reviews/{id}/findings/{fid}/verdict", post(verdict))
        .route("/api/dispatch", post(dispatch))
        .route(
            "/api/uploads/image",
            post(upload_image).layer(DefaultBodyLimit::max(
                crate::core::uploads::MAX_UPLOAD_BODY_BYTES,
            )),
        )
        .route("/api/uploads/{id}", get(get_upload))
        .route("/api/feishu/test", post(feishu_test))
        .route("/api/feishu/mute", post(feishu_mute))
        .layer(middleware::from_fn_with_state(core.clone(), auth));
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/assets/{file}", get(asset))
        .route("/auth", get(auth_route))
        .route("/api/health", get(health))
        .merge(api)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(core.clone(), peer_ip_gate))
        .with_state(core)
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn respond(r: Result<serde_json::Value, ApiError>) -> Response {
    match r {
        Ok(data) => json_response(
            StatusCode::OK,
            serde_json::json!({"ok": true, "data": data}),
        ),
        Err(e) => {
            let status = StatusCode::from_u16(e.code.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_response(status, serde_json::json!({"ok": false, "error": e}))
        }
    }
}

fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    let src: &[u8] = if body.iter().all(|b| b.is_ascii_whitespace()) {
        b"{}"
    } else {
        body
    };
    serde_json::from_slice(src)
        .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}

async fn not_found() -> Response {
    respond(Err(ApiError::not_found("no such route")))
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| k.trim() == COOKIE)
        .map(|(_, v)| v.trim().to_string())
}

fn has_valid_cookie(headers: &HeaderMap, expected: &[u8]) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k.trim() == COOKIE && ct_eq(v.trim().as_bytes(), expected))
}

fn header_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-baton-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|s| s.trim().to_string())
        })
}

/// True when `ip` is a Tailscale CGNAT IPv4 (`100.64.0.0/10`: `100.64.0.0`–`100.127.255.255`).
fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    let oct = ip.octets();
    oct[0] == 100 && (oct[1] & 0b1100_0000) == 64
}

fn is_allowed_peer_v4(v4: Ipv4Addr, web: &crate::config::WebConfig) -> bool {
    v4.is_loopback()
        || (web.allow_easytier && crate::config::ipv4_in_cidrs(v4, &web.easytier_cidrs))
        || (web.allow_tailscale && is_tailscale_ipv4(v4))
}

/// True when peer `ip` is permitted to access the Web console:
/// always allows loopback (`127.0.0.0/8`, `::1`, `::ffff:127.x.x.x`), allows
/// EasyTier CIDRs (`web.easytier_cidrs`) when `allow_easytier` is true, and allows
/// Tailscale CGNAT (`100.64.0.0/10`, `::ffff:100.64.0.0/106`) when `allow_tailscale` is true.
/// Rejects LAN (`192.168.x.x`, `10.17.x.x`, `172.16.x.x`) and public IPs.
pub fn is_allowed_peer_ip(ip: IpAddr, web: &crate::config::WebConfig) -> bool {
    match ip {
        IpAddr::V4(v4) => is_allowed_peer_v4(v4, web),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_allowed_peer_v4(v4, web))
        }
    }
}

/// Global peer-IP gate applied to every route when `auth_enabled == false` or binding `0.0.0.0`.
async fn peer_ip_gate(State(core): State<Core>, req: Request, next: Next) -> Response {
    let web = &core.config().web;
    if !web.auth_enabled || web.bind == "0.0.0.0" || web.bind == "::" {
        if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
            if !is_allowed_peer_ip(addr.ip(), web) {
                log::warn!("web: rejected peer IP {}", addr.ip());
                return forbidden("peer IP not allowed");
            }
        }
    }
    next.run(req).await
}

fn split_host_port(host: &str) -> Option<(&str, Option<u16>)> {
    let h = host.trim();
    if h.is_empty() || h.contains('/') || h.contains('@') {
        return None;
    }
    if let Some(after_open) = h.strip_prefix('[') {
        let close = after_open.find(']')?;
        let host_only = &h[..=close + 1];
        let rest = &after_open[close + 1..];
        if rest.is_empty() {
            return Some((host_only, None));
        }
        let port = rest.strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((host_only, Some(port)));
    }
    if let Some((host_only, port_str)) = h.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return (!host_only.is_empty()).then_some((host_only, Some(port)));
        }
    }
    Some((h, None))
}

pub fn is_allowed_host(core: &Core, host: &str) -> bool {
    let web = &core.config().web;
    let Some((host_only, port_opt)) = split_host_port(host) else {
        return false;
    };
    if web.allow_tailscale {
        let lower = host_only.to_ascii_lowercase();
        if lower.ends_with(".ts.net")
            && lower.len() > ".ts.net".len()
            && port_opt.is_none_or(|p| p == web.port || p == 80 || p == 443)
        {
            return true;
        }
    }
    if port_opt != Some(web.port) {
        return false;
    }
    if matches!(host_only, "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    if web.bind != "127.0.0.1" && web.bind != "0.0.0.0" && web.bind != "::" {
        let bracketed_bind = if web.bind.contains(':') && !web.bind.starts_with('[') {
            format!("[{}]", web.bind)
        } else {
            web.bind.clone()
        };
        if host_only == web.bind || host_only == bracketed_bind {
            return true;
        }
    }
    if let Ok(v4) = host_only.parse::<Ipv4Addr>() {
        if (web.allow_easytier && crate::config::ipv4_in_cidrs(v4, &web.easytier_cidrs))
            || (web.allow_tailscale && is_tailscale_ipv4(v4))
        {
            return true;
        }
    }
    false
}

fn host_ok(core: &Core, headers: &HeaderMap) -> bool {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| is_allowed_host(core, h))
}

fn origin_ok(core: &Core, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        None => true,
        Some(o) => o
            .strip_prefix("http://")
            .or_else(|| o.strip_prefix("https://"))
            .is_some_and(|host| is_allowed_host(core, host)),
    }
}

fn forbidden(msg: &str) -> Response {
    json_response(
        StatusCode::FORBIDDEN,
        serde_json::json!({"ok": false, "error": {"code": "forbidden", "message": msg}}),
    )
}

fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn auth(State(core): State<Core>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if !host_ok(&core, headers) {
        return forbidden("host not allowed");
    }
    if req.method() != Method::GET && !origin_ok(&core, headers) {
        return forbidden("cross-origin request rejected");
    }
    if !core.config().web.auth_enabled {
        return next.run(req).await;
    }
    let expected = core.web_token().as_bytes().to_vec();
    let from_cookie = has_valid_cookie(headers, &expected);
    let from_header = header_token(headers).is_some_and(|t| ct_eq(t.as_bytes(), &expected));
    if !from_cookie && !from_header {
        return json_response(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"ok": false, "error": {"code": "unauthorized",
                "message": "missing or invalid token; open the URL printed by `baton web`"}}),
        );
    }
    let mut resp = next.run(req).await;
    // EventSource cannot send headers: hand out the cookie on header-auth success.
    if from_header && !from_cookie {
        resp.headers_mut()
            .append(header::SET_COOKIE, session_cookie(core.web_token()));
    }
    resp
}

// ---------------------------------------------------------------------------
// Static
// ---------------------------------------------------------------------------

fn static_response(bytes: Vec<u8>, ctype: &'static str) -> Response {
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src 'self'",
        ),
    );
    resp
}

fn load_asset(core: &Core, name: &str) -> Option<(Vec<u8>, &'static str)> {
    let ctype = if name.ends_with(".js") || name.ends_with(".mjs") {
        "text/javascript; charset=utf-8"
    } else if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    };
    if let Some(dir) = core.web_dir() {
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return None;
        }
        return std::fs::read(dir.join(name))
            .ok()
            .or_else(|| (name == "logo.svg").then(|| LOGO_SVG.to_vec()))
            .map(|b| (b, ctype));
    }
    let bytes = match name {
        "index.html" => INDEX_HTML,
        "app.js" => APP_JS,
        "app.css" => APP_CSS,
        "logo.svg" => LOGO_SVG,
        _ => return None,
    };
    Some((bytes.to_vec(), ctype))
}

async fn index(State(core): State<Core>, headers: HeaderMap) -> Response {
    if !host_ok(&core, &headers) {
        return forbidden("host not allowed");
    }
    match load_asset(&core, "index.html") {
        Some((b, t)) => static_response(b, t),
        None => respond(Err(ApiError::not_found("index.html"))),
    }
}

async fn asset(State(core): State<Core>, headers: HeaderMap, Path(file): Path<String>) -> Response {
    if !host_ok(&core, &headers) {
        return forbidden("host not allowed");
    }
    match load_asset(&core, &file) {
        Some((b, t)) => static_response(b, t),
        None => respond(Err(ApiError::not_found(format!("asset {file}")))),
    }
}

async fn auth_route(
    State(core): State<Core>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !host_ok(&core, &headers) {
        return forbidden("host not allowed");
    }
    let ok = q
        .get("token")
        .is_some_and(|t| ct_eq(t.as_bytes(), core.web_token().as_bytes()));
    if !ok {
        return json_response(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"ok": false, "error": {"code": "unauthorized", "message": "bad token"}}),
        );
    }
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    resp.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(core.web_token()));
    resp
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async fn health(State(core): State<Core>, headers: HeaderMap) -> Response {
    if !host_ok(&core, &headers) {
        return forbidden("host not allowed");
    }
    respond(Ok(serde_json::json!({
        "ok": true,
        "version": VERSION,
        "uptime_ms": id::now_ms().saturating_sub(core.started_ms()),
        "auth_enabled": core.config().web.auth_enabled,
    })))
}

async fn state(State(core): State<Core>) -> Response {
    respond(Ok(core.state_json()))
}

async fn models(State(core): State<Core>) -> Response {
    let r = core
        .model_catalog()
        .await
        .and_then(|c| serde_json::to_value(c).map_err(|e| ApiError::internal(e.to_string())));
    respond(r)
}

async fn slash_commands(
    State(core): State<Core>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let r = async {
        let agent = match q.get("agent").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(raw) => Some(
                AgentKind::parse(raw)
                    .ok_or_else(|| ApiError::bad_request(format!("unknown agent {raw:?}")))?,
            ),
            None => None,
        };
        let cat = core.slash_commands(agent).await?;
        serde_json::to_value(cat).map_err(|e| ApiError::internal(e.to_string()))
    };
    respond(r.await)
}

fn flag(q: &HashMap<String, String>, key: &str) -> bool {
    q.get(key)
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

async fn sessions(State(core): State<Core>, Query(q): Query<HashMap<String, String>>) -> Response {
    respond(
        core.handle(DaemonCommand::ListSessions {
            project: q.get("project").cloned().filter(|s| !s.is_empty()),
            include_subagents: flag(&q, "subagents"),
            include_ended: flag(&q, "ended") || flag(&q, "include_ended"),
        })
        .await,
    )
}

async fn focus(State(core): State<Core>, Path(id): Path<String>) -> Response {
    respond(core.focus_session(&SessionId(id)).await)
}

async fn ack_session(State(core): State<Core>, Path(id): Path<String>) -> Response {
    respond(
        core.handle(DaemonCommand::AckSession {
            session_id: SessionId(id),
        })
        .await,
    )
}

async fn session_transcript(
    State(core): State<Core>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(crate::core::transcript::DEFAULT_TRANSCRIPT_LIMIT)
        .clamp(1, crate::core::transcript::MAX_TRANSCRIPT_LIMIT);
    let r = core
        .session_transcript(&SessionId(id), limit)
        .await
        .and_then(|t| serde_json::to_value(t).map_err(|e| ApiError::internal(e.to_string())));
    respond(r)
}

async fn pane_preview(
    State(core): State<Core>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let lines = q
        .get("lines")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, baton_tmux::MAX_CAPTURE_LINES);
    let r = core
        .preview_pane(&PaneId(pane_id), lines)
        .await
        .and_then(|p| serde_json::to_value(p).map_err(|e| ApiError::internal(e.to_string())));
    respond(r)
}

async fn tmux_sessions(State(core): State<Core>) -> Response {
    let r = core
        .list_tmux_sessions()
        .await
        .map(|sessions| serde_json::json!({ "sessions": sessions }));
    respond(r)
}

#[derive(Deserialize)]
struct SpawnSessionBody {
    agent: AgentKind,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    tmux_session: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

async fn spawn_session(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: SpawnSessionBody = parse_body(&body)?;
        let effort = match b.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(e) => Some(
                Effort::parse(e)
                    .ok_or_else(|| ApiError::bad_request(format!("invalid effort {e:?}")))?,
            ),
            None => None,
        };
        core.spawn_direct_session(
            b.agent,
            b.project_root,
            b.tmux_session,
            b.model,
            effort,
            b.prompt,
            b.name,
        )
        .await
    };
    respond(r.await)
}

#[derive(Deserialize)]
struct ParseBody {
    text: String,
    #[serde(default)]
    playbook: Option<PlaybookKind>,
    #[serde(default)]
    project: Option<String>,
}

async fn parse(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: ParseBody = parse_body(&body)?;
        core.handle(DaemonCommand::ParseUtterance {
            text: b.text,
            playbook: b.playbook,
            project: b.project.filter(|s| !s.trim().is_empty()),
        })
        .await
    };
    respond(r.await)
}

async fn list_pipelines(
    State(core): State<Core>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    respond(
        core.handle(DaemonCommand::ListPipelines {
            project: q.get("project").cloned().filter(|s| !s.is_empty()),
            include_done: flag(&q, "include_done") || !q.contains_key("include_done"),
        })
        .await,
    )
}

/// Accept a project name / prefix, an absolute directory, or empty (auto fallback).
fn resolve_root_or_default(core: &Core, root: Option<&str>) -> Result<String, ApiError> {
    let trimmed = root.unwrap_or("").trim();
    if trimmed.is_empty() {
        return Ok(core.default_project_root());
    }
    let p = std::path::Path::new(trimmed);
    if p.is_absolute() && p.is_dir() {
        return Ok(trimmed.to_string());
    }
    core.find_project(trimmed)
}

fn default_auto_confirm() -> bool {
    true
}

#[derive(Deserialize)]
struct RelaxedCreateReq {
    #[serde(default = "default_feature_playbook")]
    playbook: PlaybookKind,
    #[serde(default)]
    project_root: Option<String>,
    goal: String,
    #[serde(default)]
    bindings: BTreeMap<StageKind, StageBinding>,
    #[serde(default)]
    options: Option<PipelineOptions>,
    #[serde(default)]
    source: Option<PipelineSource>,
    #[serde(default)]
    utterance: Option<String>,
    #[serde(default)]
    auto_confirm: Option<bool>,
}

fn default_feature_playbook() -> PlaybookKind {
    PlaybookKind::Feature
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CreateBody {
    Wrapped {
        req: RelaxedCreateReq,
        #[serde(default)]
        auto_confirm: Option<bool>,
    },
    Flat(RelaxedCreateReq),
}

async fn create_pipeline(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let parsed_body: CreateBody = parse_body(&body)?;
        let (raw, outer_auto) = match parsed_body {
            CreateBody::Wrapped { req, auto_confirm } => (req, auto_confirm),
            CreateBody::Flat(req) => (req, None),
        };
        let auto_confirm = outer_auto
            .or(raw.auto_confirm)
            .unwrap_or_else(default_auto_confirm);
        let root = resolve_root_or_default(&core, raw.project_root.as_deref())?;
        let mut bindings = raw.bindings;
        let utterance = raw
            .utterance
            .or_else(|| raw.source.as_ref().and_then(|s| s.utterance.clone()))
            .filter(|s| !s.trim().is_empty());
        if bindings.is_empty() {
            if let Some(utt) = &utterance {
                if let Ok(parsed) = core
                    .handle(DaemonCommand::ParseUtterance {
                        text: utt.clone(),
                        playbook: Some(raw.playbook),
                        project: Some(root.clone()),
                    })
                    .await
                {
                    bindings =
                        serde_json::from_value(parsed.get("bindings").cloned().unwrap_or_default())
                            .unwrap_or_default();
                    bindings.retain(|stage, _| stage.playbook() == raw.playbook);
                }
            }
        }
        let req = CreatePipelineReq {
            playbook: raw.playbook,
            project_root: root,
            goal: raw.goal,
            bindings,
            options: raw.options,
            source: PipelineSource {
                via: Via::Web,
                utterance,
                operator: raw.source.and_then(|s| s.operator),
            },
        };
        let created = core.handle(DaemonCommand::CreatePipeline { req }).await?;
        if auto_confirm {
            if let Some(pid) = created.get("id").and_then(|v| v.as_str()) {
                return core
                    .handle(DaemonCommand::ConfirmPipeline {
                        pipeline_id: PipelineId(pid.to_string()),
                        overrides: None,
                    })
                    .await;
            }
        }
        Ok(created)
    };
    respond(r.await)
}

async fn get_pipeline(State(core): State<Core>, Path(id): Path<String>) -> Response {
    respond(
        core.handle(DaemonCommand::GetPipeline {
            pipeline_id: PipelineId(id),
        })
        .await,
    )
}

#[derive(Deserialize, Default)]
struct ConfirmBody {
    #[serde(default)]
    overrides: Option<BTreeMap<StageKind, StageBinding>>,
}

async fn confirm_pipeline(
    State(core): State<Core>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let r = async {
        let b: ConfirmBody = parse_body(&body)?;
        core.handle(DaemonCommand::ConfirmPipeline {
            pipeline_id: PipelineId(id),
            overrides: b.overrides.filter(|m| !m.is_empty()),
        })
        .await
    };
    respond(r.await)
}

async fn pipeline_action(
    State(core): State<Core>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let r = async {
        let action: PipelineAction = parse_body(&body)?;
        core.handle(DaemonCommand::PipelineAction {
            pipeline_id: PipelineId(id),
            action,
        })
        .await
    };
    respond(r.await)
}

#[derive(Deserialize)]
struct DebugBody {
    #[serde(default)]
    project_root: Option<String>,
    symptom: String,
    #[serde(default)]
    utterance: Option<String>,
    #[serde(default = "default_auto_confirm")]
    auto_confirm: bool,
}

async fn create_debug(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: DebugBody = parse_body(&body)?;
        let root = resolve_root_or_default(&core, b.project_root.as_deref())?;
        let text = b
            .utterance
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| b.symptom.clone());
        let parsed = core
            .handle(DaemonCommand::ParseUtterance {
                text: text.clone(),
                playbook: Some(PlaybookKind::Debug),
                project: Some(root.clone()),
            })
            .await?;
        let mut bindings: BTreeMap<StageKind, StageBinding> =
            serde_json::from_value(parsed.get("bindings").cloned().unwrap_or_default())
                .unwrap_or_default();
        bindings.retain(|stage, _| stage.playbook() == PlaybookKind::Debug);
        let created = core
            .handle(DaemonCommand::CreatePipeline {
                req: CreatePipelineReq {
                    playbook: PlaybookKind::Debug,
                    project_root: root,
                    goal: b.symptom,
                    bindings,
                    options: None,
                    source: PipelineSource {
                        via: Via::Web,
                        utterance: Some(text),
                        operator: None,
                    },
                },
            })
            .await?;
        if b.auto_confirm {
            if let Some(pid) = created.get("id").and_then(|v| v.as_str()) {
                return core
                    .handle(DaemonCommand::ConfirmPipeline {
                        pipeline_id: PipelineId(pid.to_string()),
                        overrides: None,
                    })
                    .await;
            }
        }
        Ok(created)
    };
    respond(r.await)
}

async fn debug_verb(
    State(core): State<Core>,
    Path((id, verb)): Path<(String, String)>,
) -> Response {
    let action = match verb.as_str() {
        "continue" => PipelineAction::ContinueDeep,
        "stop" => PipelineAction::StopDeep,
        "mitigate" => PipelineAction::AdoptMitigation,
        other => {
            return respond(Err(ApiError::not_found(format!(
                "unknown debug action {other}"
            ))))
        }
    };
    respond(
        core.handle(DaemonCommand::PipelineAction {
            pipeline_id: PipelineId(id),
            action,
        })
        .await,
    )
}

async fn list_handoffs(
    State(core): State<Core>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    respond(
        core.handle(DaemonCommand::ListHandoffs {
            session_id: q
                .get("session")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(SessionId),
            pipeline_id: q
                .get("pipeline")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(PipelineId),
        })
        .await,
    )
}

#[derive(Deserialize)]
struct HandoffBody {
    draft: HandoffDraft,
}

async fn create_handoff(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: HandoffBody = parse_body(&body)?;
        core.handle(DaemonCommand::CreateHandoff { draft: b.draft })
            .await
    };
    respond(r.await)
}

async fn get_artifact(State(core): State<Core>, Path(id): Path<String>) -> Response {
    respond(
        core.handle(DaemonCommand::GetArtifact { id: ArtifactId(id) })
            .await,
    )
}

async fn list_reviews(
    State(core): State<Core>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    respond(
        core.handle(DaemonCommand::ListReviews {
            pipeline_id: q
                .get("pipeline")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(PipelineId),
        })
        .await,
    )
}

async fn precision(State(core): State<Core>, Query(q): Query<HashMap<String, String>>) -> Response {
    let agent = q.get("agent").and_then(|a| AgentKind::parse(a));
    respond(
        core.handle(DaemonCommand::GetPrecision {
            agent,
            model: q.get("model").cloned().filter(|s| !s.is_empty()),
        })
        .await,
    )
}

async fn get_review(State(core): State<Core>, Path(id): Path<String>) -> Response {
    respond(
        core.handle(DaemonCommand::GetReview {
            review_id: ReviewId(id),
        })
        .await,
    )
}

#[derive(Deserialize)]
struct FeedbackBody {
    label: FeedbackLabel,
    #[serde(default)]
    note: Option<String>,
}

async fn feedback(
    State(core): State<Core>,
    Path((id, fid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let r = async {
        let b: FeedbackBody = parse_body(&body)?;
        core.handle(DaemonCommand::ReviewFeedback {
            review_id: ReviewId(id),
            finding_id: FindingId(fid),
            label: b.label,
            note: b.note,
            by: "web".into(),
        })
        .await
    };
    respond(r.await)
}

async fn verdict(
    State(core): State<Core>,
    Path((id, fid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let r = async {
        let v: serde_json::Value = parse_body(&body)?;
        let verdict: FalsifierVerdict = serde_json::from_value(serde_json::json!({
            "verdict": v.get("verdict").cloned().unwrap_or_default(),
            "to": v.get("to").cloned().unwrap_or(serde_json::Value::Null),
        }))
        .or_else(|_| {
            serde_json::from_value(
                serde_json::json!({"verdict": v.get("verdict").cloned().unwrap_or_default()}),
            )
        })
        .map_err(|e| ApiError::bad_request(format!("invalid verdict: {e}")))?;
        core.handle(DaemonCommand::HumanVerdict {
            review_id: ReviewId(id),
            finding_id: FindingId(fid),
            verdict,
            note: v
                .get("note")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string()),
            by: "web".into(),
        })
        .await
    };
    respond(r.await)
}

#[derive(Deserialize)]
struct DispatchBody {
    target: DispatchTarget,
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<PasteAttachment>,
    #[serde(default)]
    allow: Vec<DeliveryLevel>,
    #[serde(default)]
    interactive: bool,
}

async fn dispatch(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: DispatchBody = parse_body(&body)?;
        core.handle(DaemonCommand::Dispatch {
            target: b.target,
            text: b.text,
            attachments: b.attachments,
            allow: b.allow,
            interactive: b.interactive,
        })
        .await
    };
    respond(r.await)
}

#[derive(Deserialize)]
struct UploadImageBody {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    data_base64: String,
}

async fn upload_image(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: UploadImageBody = parse_body(&body)?;
        let state_dir = core.state_dir().to_path_buf();
        let saved = tokio::task::spawn_blocking(move || {
            crate::core::uploads::save_uploaded_image(
                &state_dir,
                b.filename.as_deref(),
                b.mime.as_deref(),
                &b.data_base64,
            )
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
        serde_json::to_value(saved).map_err(|e| ApiError::internal(e.to_string()))
    };
    respond(r.await)
}

async fn get_upload(State(core): State<Core>, Path(id): Path<String>) -> Response {
    let state_dir = core.state_dir().to_path_buf();
    let res = tokio::task::spawn_blocking(move || {
        crate::core::uploads::load_uploaded_image(&state_dir, &id)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))
    .and_then(|r| r);
    match res {
        Ok((bytes, mime)) => {
            let mut resp = Response::new(Body::from(bytes));
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            resp.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            );
            resp
        }
        Err(e) => respond(Err(e)),
    }
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

async fn feishu_test(State(core): State<Core>, body: Bytes) -> Response {
    let r = async {
        let b: TextBody = parse_body(&body)?;
        core.handle(DaemonCommand::FeishuTest { text: b.text })
            .await
    };
    respond(r.await)
}

/// Maximum custom Feishu mute duration: 7 days (10,080 minutes).
pub const MAX_MUTE_MINUTES: u64 = 10_080;
const MAX_MUTE_MS: u64 = MAX_MUTE_MINUTES * 60_000;

#[derive(Deserialize)]
struct MuteBody {
    #[serde(default)]
    minutes: Option<u64>,
    #[serde(default)]
    until_ms: Option<u64>,
}

fn resolve_mute_until(
    minutes: Option<u64>,
    until_ms: Option<u64>,
    now_ms: u64,
) -> Result<Option<u64>, ApiError> {
    if let Some(ts) = until_ms {
        if ts <= now_ms {
            return Err(ApiError::bad_request("until_ms must be in the future"));
        }
        if ts > now_ms.saturating_add(MAX_MUTE_MS) {
            return Err(ApiError::bad_request(
                "until_ms cannot exceed 7 days from now",
            ));
        }
        return Ok(Some(ts));
    }
    match minutes {
        Some(0) => Ok(None),
        Some(m) if m > MAX_MUTE_MINUTES => Err(ApiError::bad_request(format!(
            "minutes cannot exceed {MAX_MUTE_MINUTES} (7 days)"
        ))),
        Some(m) => Ok(Some(now_ms.saturating_add(m.saturating_mul(60_000)))),
        None => Err(ApiError::bad_request(
            "either minutes or until_ms is required",
        )),
    }
}

async fn feishu_mute(State(core): State<Core>, body: Bytes) -> Response {
    let r = (|| {
        let b: MuteBody = parse_body(&body)?;
        let until = resolve_mute_until(b.minutes, b.until_ms, id::now_ms())?;
        core.set_feishu_mute(until);
        core.audit(
            "web",
            serde_json::json!({"mute_minutes": b.minutes, "until_ms": until}),
        );
        Ok(serde_json::json!({"feishu_muted_until_ms": until}))
    })();
    respond(r)
}

// ---------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------

struct EventStream {
    rx: mpsc::Receiver<Event>,
}

impl futures_core::Stream for EventStream {
    type Item = Result<Event, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|o| o.map(Ok))
    }
}

fn to_sse(e: &DaemonEvent) -> Option<Event> {
    let data = serde_json::to_string(e).ok()?;
    Some(
        Event::default()
            .id(e.seq().to_string())
            .event(e.event_name())
            .data(data),
    )
}

async fn events(State(core): State<Core>, headers: HeaderMap) -> Response {
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // Subscribe before reading the replay buffer so nothing falls in between.
    let mut sub = core.subscribe();
    let replay = last.map(|l| core.replay_since(l)).unwrap_or_default();
    let (tx, rx) = mpsc::channel::<Event>(256);
    tokio::spawn(async move {
        let mut sent = last.unwrap_or(0);
        for e in replay {
            sent = sent.max(e.seq());
            let Some(ev) = to_sse(&e) else { continue };
            if tx.send(ev).await.is_err() {
                return;
            }
        }
        loop {
            match sub.recv().await {
                Ok(e) => {
                    if e.seq() <= sent && last.is_some() {
                        continue;
                    }
                    sent = sent.max(e.seq());
                    let Some(ev) = to_sse(&e) else { continue };
                    if tx.send(ev).await.is_err() {
                        return; // client went away
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("sse client lagged by {n} events");
                    let ev = Event::default().event("resync").data("{}");
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Sse::new(EventStream { rx })
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; baton_token=xyz; b=2"),
        );
        assert_eq!(cookie_token(&h).as_deref(), Some("xyz"));
        let mut h2 = HeaderMap::new();
        h2.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        assert_eq!(header_token(&h2).as_deref(), Some("tok"));
    }

    #[test]
    fn embedded_assets_present() {
        assert!(INDEX_HTML.starts_with(b"<!doctype html>"));
        assert!(!APP_JS.is_empty());
        assert!(!APP_CSS.is_empty());
        assert!(LOGO_SVG.starts_with(b"<svg"));
    }

    #[test]
    fn easytier_tailscale_and_loopback_peer_ip_gate() {
        let parse = |s: &str| s.parse::<IpAddr>().unwrap();
        let et_only = crate::config::WebConfig {
            allow_easytier: true,
            easytier_cidrs: vec!["10.144.144.0/24".into()],
            allow_tailscale: false,
            ..crate::config::WebConfig::default()
        };
        let none_allowed = crate::config::WebConfig {
            allow_easytier: false,
            allow_tailscale: false,
            ..et_only.clone()
        };
        let ts_only = crate::config::WebConfig {
            allow_easytier: false,
            allow_tailscale: true,
            ..et_only.clone()
        };

        // Loopback always allowed regardless of overlay toggles
        assert!(is_allowed_peer_ip(parse("127.0.0.1"), &none_allowed));
        assert!(is_allowed_peer_ip(parse("127.0.0.1"), &et_only));
        assert!(is_allowed_peer_ip(parse("::1"), &none_allowed));
        assert!(is_allowed_peer_ip(parse("::ffff:127.0.0.1"), &et_only));

        // Option A: EasyTier allowed, Tailscale blocked
        assert!(is_allowed_peer_ip(parse("10.144.144.1"), &et_only));
        assert!(is_allowed_peer_ip(parse("10.144.144.2"), &et_only));
        assert!(is_allowed_peer_ip(parse("10.144.144.3"), &et_only));
        assert!(is_allowed_peer_ip(parse("::ffff:10.144.144.3"), &et_only));
        assert!(!is_allowed_peer_ip(parse("100.64.10.20"), &et_only));
        assert!(!is_allowed_peer_ip(parse("100.64.0.1"), &et_only));

        // EasyTier rejected when allow_easytier == false
        assert!(!is_allowed_peer_ip(parse("10.144.144.3"), &none_allowed));

        // Tailscale CGNAT (100.64.0.0/10) allowed when allow_tailscale == true
        assert!(is_allowed_peer_ip(parse("100.64.10.20"), &ts_only));
        assert!(is_allowed_peer_ip(parse("100.64.0.1"), &ts_only));
        assert!(is_allowed_peer_ip(parse("100.127.255.254"), &ts_only));
        assert!(is_allowed_peer_ip(parse("::ffff:100.64.10.20"), &ts_only));

        // LAN / neighbor subnets / public / out-of-range IPs strictly rejected
        for cfg in [&et_only, &ts_only] {
            assert!(!is_allowed_peer_ip(parse("192.168.1.5"), cfg));
            assert!(!is_allowed_peer_ip(parse("10.17.0.50"), cfg));
            assert!(!is_allowed_peer_ip(parse("10.144.145.1"), cfg));
            assert!(!is_allowed_peer_ip(parse("10.0.0.8"), cfg));
            assert!(!is_allowed_peer_ip(parse("172.16.0.5"), cfg));
            assert!(!is_allowed_peer_ip(parse("8.8.8.8"), cfg));
            assert!(!is_allowed_peer_ip(parse("100.63.255.255"), cfg));
            assert!(!is_allowed_peer_ip(parse("100.128.0.1"), cfg));
        }
    }

    #[test]
    fn custom_feishu_mute_validation() {
        let now = 1_700_000_000_000u64;
        assert_eq!(resolve_mute_until(Some(0), None, now).unwrap(), None);
        assert_eq!(
            resolve_mute_until(Some(45), None, now).unwrap(),
            Some(now + 45 * 60_000)
        );
        assert_eq!(
            resolve_mute_until(Some(MAX_MUTE_MINUTES), None, now).unwrap(),
            Some(now + MAX_MUTE_MS)
        );
        assert!(resolve_mute_until(Some(MAX_MUTE_MINUTES + 1), None, now).is_err());

        let future = now + 3_600_000;
        assert_eq!(
            resolve_mute_until(None, Some(future), now).unwrap(),
            Some(future)
        );
        assert!(resolve_mute_until(None, Some(now), now).is_err());
        assert!(resolve_mute_until(None, Some(now - 1), now).is_err());
        assert!(resolve_mute_until(None, Some(now + MAX_MUTE_MS + 1), now).is_err());
        assert!(resolve_mute_until(None, None, now).is_err());
    }
}
