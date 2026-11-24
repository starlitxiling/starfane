//! `starfane mcp --agent <a>` — hand-rolled MCP stdio server (contracts §7.3).
//!
//! JSON-RPC 2.0, one JSON object per line on stdin/stdout; logs go to stderr
//! only (stdout is the protocol channel). Fully synchronous: no tokio.
//!
//! The dispatcher ([`McpServer`]) talks to the daemon through the
//! [`Transport`] trait so it can be unit-tested without a running daemon.

use crate::client::{ClientError, DaemonClient};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use starfane_proto::mcp::{negotiate_protocol_version, MCP_TOOL_NAMES};
use starfane_proto::{
    mcp_tool_schemas, AgentKind, ApiError, ArtifactPayload, AuthorRef, BindToken,
    CreatePipelineReq, DaemonCommand, EvidenceQuote, FalsifierSubmission, FalsifierVerdict,
    FinalReport, FindingId, HandoffDraft, HandoffId, HandoffStatus, IdentityResolution, PaneId,
    PipelineSource, PlanDoc, PlaybookKind, RcaReport, ReviewId, ReviewReport, SessionId,
    SessionRecord, Severity, StageBinding, StageKind, TriageReport, Via,
};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const JSONRPC_VERSION: &str = "2.0";
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_NAME: &str = "starfane";
const IDENTITY_HINT: &str =
    "Retry the tool call with `from_session` set to your own session id (see candidates).";

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Failure of a daemon call as seen by the MCP dispatcher.
#[derive(Debug)]
pub enum CallError {
    /// The daemon answered with an error.
    Api(ApiError),
    /// The daemon could not be reached / the connection broke.
    Transport(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Api(e) => write!(f, "{e}"),
            CallError::Transport(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CallError {}

/// One request/response exchange with the daemon.
pub trait Transport {
    fn call(&mut self, cmd: DaemonCommand) -> Result<Value, CallError>;
}

/// Lazily connected UDS transport. On a transport-level failure it drops the
/// connection, reconnects once and retries; API errors are returned as-is.
pub struct UdsTransport {
    socket: PathBuf,
    client: Option<DaemonClient>,
}

impl UdsTransport {
    pub fn new(socket: &Path) -> Self {
        UdsTransport {
            socket: socket.to_path_buf(),
            client: None,
        }
    }
}

/// Connection attempts per call: the first one plus a single reconnect.
const UDS_ATTEMPTS: usize = 2;

fn is_idempotent_read(cmd: &DaemonCommand) -> bool {
    matches!(
        cmd,
        DaemonCommand::Ping
            | DaemonCommand::GetState
            | DaemonCommand::ListSessions { .. }
            | DaemonCommand::ResolveIdentity { .. }
            | DaemonCommand::ReadInbox {
                mark_delivered: false,
                ..
            }
            | DaemonCommand::ListPipelines { .. }
            | DaemonCommand::GetPipeline { .. }
            | DaemonCommand::GetArtifact { .. }
            | DaemonCommand::ListHandoffs { .. }
            | DaemonCommand::GetReview { .. }
            | DaemonCommand::ListReviews { .. }
            | DaemonCommand::GetPrecision { .. }
    )
}

impl Transport for UdsTransport {
    fn call(&mut self, cmd: DaemonCommand) -> Result<Value, CallError> {
        let mut last_err = String::from("daemon call not attempted");
        for attempt in 1..=UDS_ATTEMPTS {
            if self.client.is_none() {
                match DaemonClient::connect(&self.socket, true) {
                    Ok(c) => self.client = Some(c),
                    Err(e) => {
                        log::warn!("starfane mcp: connect attempt {attempt} failed: {e}");
                        last_err = e.to_string();
                        continue;
                    }
                }
            }
            let Some(client) = self.client.as_mut() else {
                continue;
            };
            match client.call(cmd.clone()) {
                Ok(v) => return Ok(v),
                Err(ClientError::Api(e)) => return Err(CallError::Api(e)),
                Err(e) => {
                    log::warn!(
                        "starfane mcp: {} failed on attempt {attempt}: {e}",
                        cmd.name()
                    );
                    last_err = e.to_string();
                    self.client = None;
                    if !is_idempotent_read(&cmd) {
                        return Err(CallError::Transport(last_err));
                    }
                }
            }
        }
        Err(CallError::Transport(last_err))
    }
}

// ---------------------------------------------------------------------------
// Caller environment
// ---------------------------------------------------------------------------

/// Identity inputs captured once at startup (`ResolveIdentity` arguments).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallerEnv {
    pub cwd: Option<String>,
    pub pane: Option<PaneId>,
    pub bind_token: Option<BindToken>,
}

impl CallerEnv {
    pub fn capture() -> Self {
        let cwd = match std::env::current_dir() {
            Ok(p) => Some(p.to_string_lossy().into_owned()),
            Err(e) => {
                log::warn!("starfane mcp: cannot read current directory: {e}");
                None
            }
        };
        CallerEnv {
            cwd,
            pane: non_empty_env("TMUX_PANE").map(PaneId::new),
            bind_token: non_empty_env(starfane_proto::paths::ENV_BIND)
                .or_else(|| non_empty_env("BATON_BIND"))
                .map(BindToken::new),
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Nearest ancestor (inclusive) containing `.git`; `start` itself if none.
fn git_root_of(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(start)
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// Tool errors and argument helpers
// ---------------------------------------------------------------------------

/// A tool-level failure, reported as `isError: true` (never a JSON-RPC error).
#[derive(Debug, Clone, PartialEq)]
struct ToolError {
    message: String,
    data: Option<Value>,
}

impl ToolError {
    fn new(message: impl Into<String>) -> Self {
        ToolError {
            message: message.into(),
            data: None,
        }
    }

    fn with_data(message: impl Into<String>, data: Value) -> Self {
        ToolError {
            message: message.into(),
            data: Some(data),
        }
    }
}

impl From<CallError> for ToolError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Api(api) => ToolError::with_data(
                api.message.clone(),
                json!({ "code": api.code, "message": api.message }),
            ),
            CallError::Transport(m) => ToolError::new(format!("starfane daemon unavailable: {m}")),
        }
    }
}

type Args = Map<String, Value>;

fn req_value<'a>(args: &'a Args, key: &str, why: &str) -> Result<&'a Value, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Err(ToolError::new(format!(
            "missing required argument `{key}`{why}"
        ))),
        Some(v) => Ok(v),
    }
}

fn opt_str<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(other) => Err(ToolError::new(format!(
            "argument `{key}` must be a string, got {}",
            type_name(other)
        ))),
    }
}

/// Required, non-blank string argument.
fn req_str<'a>(args: &'a Args, key: &str, why: &str) -> Result<&'a str, ToolError> {
    match opt_str(args, key)? {
        Some(s) if !s.trim().is_empty() => Ok(s),
        Some(_) => Err(ToolError::new(format!(
            "argument `{key}` must not be empty{why}"
        ))),
        None => Err(ToolError::new(format!(
            "missing required argument `{key}`{why}"
        ))),
    }
}

fn opt_bool(args: &Args, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(ToolError::new(format!(
            "argument `{key}` must be a boolean, got {}",
            type_name(other)
        ))),
    }
}

fn parse_as<T: DeserializeOwned>(value: Value, what: &str) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|e| ToolError::new(format!("invalid `{what}`: {e}")))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `{stage: shorthand}` → typed bindings.
fn parse_binding_map(map: &Args) -> Result<BTreeMap<StageKind, StageBinding>, ToolError> {
    let mut out = BTreeMap::new();
    for (stage, value) in map {
        let kind = StageKind::parse(stage)
            .ok_or_else(|| ToolError::new(format!("bindings: unknown stage `{stage}`")))?;
        let shorthand = value.as_str().ok_or_else(|| {
            ToolError::new(format!("bindings.{stage} must be a shorthand string"))
        })?;
        let binding = StageBinding::from_shorthand(shorthand)
            .map_err(|e| ToolError::new(format!("bindings.{stage}: {e}")))?;
        out.insert(kind, binding);
    }
    Ok(out)
}

fn describe_candidates(candidates: &[SessionRecord]) -> String {
    candidates
        .iter()
        .map(|c| {
            format!(
                "- {} (agent={}, pane={}, cwd={}, state={:?})",
                c.session_id,
                c.agent.as_str(),
                c.pane.as_ref().map(|p| p.as_str()).unwrap_or("-"),
                c.cwd.as_deref().unwrap_or("-"),
                c.state
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// JSON-RPC framing
// ---------------------------------------------------------------------------

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

/// MCP `tools/call` result envelope.
fn tool_result(outcome: Result<Value, ToolError>) -> Value {
    match outcome {
        Ok(data) => {
            let structured = match data {
                Value::Object(_) => data,
                other => json!({ "result": other }),
            };
            json!({
                "content": [{ "type": "text", "text": structured.to_string() }],
                "structuredContent": structured,
                "isError": false
            })
        }
        Err(e) => {
            let mut structured = Map::new();
            structured.insert("error".into(), Value::String(e.message.clone()));
            let mut text = e.message;
            if let Some(data) = e.data {
                text.push('\n');
                text.push_str(&data.to_string());
                match data {
                    Value::Object(m) => {
                        for (k, v) in m {
                            structured.entry(k).or_insert(v);
                        }
                    }
                    other => {
                        structured.insert("data".into(), other);
                    }
                }
            }
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": Value::Object(structured),
                "isError": true
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct McpServer<T: Transport> {
    agent: AgentKind,
    env: CallerEnv,
    transport: T,
    identity: Option<SessionId>,
}

impl<T: Transport> McpServer<T> {
    pub fn new(agent: AgentKind, env: CallerEnv, transport: T) -> Self {
        McpServer {
            agent,
            env,
            transport,
            identity: None,
        }
    }

    /// Serve until EOF. Each non-empty input line yields at most one output line.
    pub fn serve<R: BufRead, W: Write>(&mut self, mut reader: R, mut writer: W) -> io::Result<()> {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            let response = match std::str::from_utf8(&buf) {
                Ok(line) if line.trim().is_empty() => None,
                Ok(line) => self.handle_line(line.trim()),
                Err(e) => Some(rpc_error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("invalid UTF-8: {e}"),
                )),
            };
            if let Some(resp) = response {
                let line = serde_json::to_string(&resp)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
    }

    /// Handle one JSON-RPC line; `None` for notifications and client responses.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(line) {
            Ok(msg) => self.handle_message(msg),
            Err(e) => Some(rpc_error(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
            )),
        }
    }

    fn handle_message(&mut self, msg: Value) -> Option<Value> {
        let Value::Object(obj) = msg else {
            return Some(rpc_error(
                Value::Null,
                INVALID_REQUEST,
                "request must be a JSON object (batches are not supported)",
            ));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if obj.contains_key("result") || obj.contains_key("error") {
                // A response to a server→client request; we never send any.
                log::debug!("starfane mcp: ignoring client response {id:?}");
                return None;
            }
            return Some(rpc_error(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "missing `method`",
            ));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notification: never answered (initialized, cancelled, ...).
            log::debug!("starfane mcp: notification {method}");
            return None;
        };
        let reply = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": negotiate_protocol_version(requested),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": SERVER_NAME, "version": starfane_proto::VERSION }
                    }),
                )
            }
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": mcp_tool_schemas() })),
            "tools/call" => match self.tools_call(&params) {
                Ok(result) => rpc_result(id, result),
                Err(message) => rpc_error(id, INVALID_PARAMS, message),
            },
            other => rpc_error(id, METHOD_NOT_FOUND, format!("method not found: {other}")),
        };
        Some(reply)
    }

    /// `Err` = JSON-RPC invalid params (unknown tool / malformed envelope).
    fn tools_call(&mut self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "tools/call requires a string `name`".to_string())?;
        if !MCP_TOOL_NAMES.contains(&name) {
            return Err(format!("unknown tool: {name}"));
        }
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(other) => {
                return Err(format!(
                    "tools/call `arguments` must be an object, got {}",
                    type_name(other)
                ))
            }
        };
        let outcome = self.call_tool(name, &args);
        if let Err(e) = &outcome {
            log::info!("starfane mcp: tool {name} returned error: {}", e.message);
        }
        Ok(tool_result(outcome))
    }

    fn call(&mut self, cmd: DaemonCommand) -> Result<Value, ToolError> {
        self.transport.call(cmd).map_err(ToolError::from)
    }

    /// Caller session: explicit `from_session` > cached identity > ResolveIdentity.
    fn caller(&mut self, args: &Args) -> Result<SessionId, ToolError> {
        if let Some(s) = opt_str(args, "from_session")? {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(SessionId::new(s));
            }
        }
        if let Some(id) = &self.identity {
            return Ok(id.clone());
        }
        let resolution = self.call(DaemonCommand::ResolveIdentity {
            agent: self.agent,
            cwd: self.env.cwd.clone(),
            pane: self.env.pane.clone(),
            bind_token: self.env.bind_token.clone(),
            session_hint: None,
        })?;
        let resolution: IdentityResolution = parse_as(resolution, "identity resolution")?;
        match resolution {
            IdentityResolution::Resolved { session_id, via } => {
                log::info!("starfane mcp: identity resolved to {session_id} via {via:?}");
                if !session_id.as_str().starts_with("tmux-") {
                    self.identity = Some(session_id.clone());
                }
                Ok(session_id)
            }
            IdentityResolution::Ambiguous { candidates } => Err(ToolError::with_data(
                format!(
                    "cannot tell which {} session this is ({} candidates):\n{}\n{IDENTITY_HINT}",
                    self.agent.as_str(),
                    candidates.len(),
                    describe_candidates(&candidates)
                ),
                json!({ "candidates": candidates, "hint": IDENTITY_HINT }),
            )),
            IdentityResolution::Unknown => Err(ToolError::with_data(
                format!(
                    "starfane does not know this {} session yet (no hook event matched pane/cwd). {IDENTITY_HINT}",
                    self.agent.as_str()
                ),
                json!({ "candidates": [], "hint": IDENTITY_HINT }),
            )),
        }
    }

    /// Optional caller for informational fields: never triggers a resolution.
    fn known_caller(&self, args: &Args) -> Result<Option<SessionId>, ToolError> {
        Ok(opt_str(args, "from_session")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(SessionId::new)
            .or_else(|| self.identity.clone()))
    }

    fn call_tool(&mut self, name: &str, args: &Args) -> Result<Value, ToolError> {
        let explicit_from = opt_str(args, "from_session")
            .ok()
            .flatten()
            .is_some_and(|s| !s.trim().is_empty());
        let had_cached = !explicit_from && self.identity.is_some();
        let res = self.call_tool_inner(name, args);
        if had_cached {
            if let Err(ref e) = res {
                if e.data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str)
                    == Some("not_found")
                    && e.message.contains("session")
                {
                    log::info!(
                        "starfane mcp: cached session expired, clearing identity and retrying"
                    );
                    self.identity = None;
                    return self.call_tool_inner(name, args);
                }
            }
        }
        res
    }

    fn call_tool_inner(&mut self, name: &str, args: &Args) -> Result<Value, ToolError> {
        match name {
            "list_sessions" => self.tool_list_sessions(args),
            "create_handoff" => self.tool_create_handoff(args),
            "read_inbox" => self.tool_read_inbox(args),
            "ack_handoff" => self.tool_ack_handoff(args),
            "submit_review" => {
                let review: ReviewReport =
                    parse_as(req_value(args, "review", "")?.clone(), "review")?;
                self.submit(args, ArtifactPayload::Review(review))
            }
            "submit_triage" => {
                let triage: TriageReport =
                    parse_as(req_value(args, "triage", "")?.clone(), "triage")?;
                self.submit(args, ArtifactPayload::Triage(triage))
            }
            "report_milestone" => self.tool_report_milestone(args),
            "submit_falsifier_verdict" => self.tool_falsifier(args),
            "start_pipeline" => self.tool_start_pipeline(args),
            other => Err(ToolError::new(format!("unknown tool: {other}"))),
        }
    }

    fn tool_list_sessions(&mut self, args: &Args) -> Result<Value, ToolError> {
        let project = opt_str(args, "project")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let include_subagents = opt_bool(args, "include_subagents")?.unwrap_or(false);
        let include_ended = opt_bool(args, "include_ended")?.unwrap_or(false);
        let sessions = self.call(DaemonCommand::ListSessions {
            project,
            include_subagents,
            include_ended,
        })?;
        Ok(json!({ "sessions": sessions }))
    }

    fn tool_create_handoff(&mut self, args: &Args) -> Result<Value, ToolError> {
        for key in ["to", "intent", "mode", "summary", "body_md"] {
            req_value(args, key, "")?;
        }
        // Validate the shape before resolving identity (from_session is a placeholder here).
        let mut draft_obj = args.clone();
        draft_obj.insert("from_session".into(), Value::String(String::new()));
        let mut draft: HandoffDraft =
            parse_as(Value::Object(draft_obj), "create_handoff arguments")?;
        if draft.summary.trim().is_empty() {
            return Err(ToolError::new("argument `summary` must not be empty"));
        }
        draft.from_session = self.caller(args)?;
        self.call(DaemonCommand::CreateHandoff { draft })
    }

    fn tool_read_inbox(&mut self, args: &Args) -> Result<Value, ToolError> {
        let mark_delivered = opt_bool(args, "mark_delivered")?.unwrap_or(true);
        let session_id = self.caller(args)?;
        let items = self.call(DaemonCommand::ReadInbox {
            session_id,
            mark_delivered,
        })?;
        Ok(json!({ "items": items }))
    }

    fn tool_ack_handoff(&mut self, args: &Args) -> Result<Value, ToolError> {
        let handoff_id = HandoffId::new(req_str(args, "handoff_id", "")?.trim());
        let status = match req_str(args, "status", "")? {
            "acked" => HandoffStatus::Acked,
            "done" => HandoffStatus::Done,
            other => {
                return Err(ToolError::new(format!(
                    "argument `status` must be \"acked\" or \"done\", got {other:?}"
                )))
            }
        };
        let note = opt_str(args, "note")?.map(str::to_string);
        let session_id = self.caller(args)?;
        self.call(DaemonCommand::AckHandoff {
            session_id,
            handoff_id,
            status,
            note,
        })
    }

    fn submit(&mut self, args: &Args, artifact: ArtifactPayload) -> Result<Value, ToolError> {
        let session_id = self.caller(args)?;
        self.call(DaemonCommand::SubmitArtifact {
            session_id,
            artifact,
        })
    }

    fn tool_report_milestone(&mut self, args: &Args) -> Result<Value, ToolError> {
        match req_str(args, "kind", "")? {
            "plan" => {
                let doc: PlanDoc =
                    parse_as(req_value(args, "plan", " for kind=plan")?.clone(), "plan")?;
                self.submit(args, ArtifactPayload::Plan(doc))
            }
            "rca" => {
                let doc: RcaReport =
                    parse_as(req_value(args, "rca", " for kind=rca")?.clone(), "rca")?;
                self.submit(args, ArtifactPayload::Rca(doc))
            }
            "final_report" => {
                let doc: FinalReport = parse_as(
                    req_value(args, "report", " for kind=final_report")?.clone(),
                    "report",
                )?;
                self.submit(args, ArtifactPayload::Report(doc))
            }
            "progress" => {
                let text = req_str(args, "text", " for kind=progress")?.to_string();
                let notify_human = opt_bool(args, "notify_human")?.unwrap_or(false);
                let session_id = self.caller(args)?;
                self.call(DaemonCommand::ReportProgress {
                    session_id,
                    text,
                    notify_human,
                })
            }
            other => Err(ToolError::new(format!(
                "argument `kind` must be one of plan|rca|final_report|progress, got {other:?}"
            ))),
        }
    }

    fn tool_falsifier(&mut self, args: &Args) -> Result<Value, ToolError> {
        let review_id = ReviewId::new(req_str(args, "review_id", "")?.trim());
        let finding_id = FindingId::new(req_str(args, "finding_id", "")?.trim());
        let rationale = req_str(args, "rationale", "")?.to_string();
        let verdict = match req_str(args, "verdict", "")? {
            "confirmed" => FalsifierVerdict::Confirmed,
            "needs_human" => FalsifierVerdict::NeedsHuman,
            "disproved" => FalsifierVerdict::Disproved,
            "downgraded" => {
                let to = req_str(args, "to", " for verdict=downgraded")?;
                let to: Severity = parse_as(Value::String(to.to_string()), "to")?;
                FalsifierVerdict::Downgraded { to }
            }
            other => {
                return Err(ToolError::new(format!(
                    "argument `verdict` must be one of confirmed|needs_human|downgraded|disproved, got {other:?}"
                )))
            }
        };
        let counter_evidence: Vec<EvidenceQuote> = match args.get("counter_evidence") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => parse_as(v.clone(), "counter_evidence")?,
        };
        let session_id = self.caller(args)?;
        let submission = FalsifierSubmission {
            review_id,
            finding_id,
            verdict,
            rationale,
            counter_evidence,
            falsifier: AuthorRef {
                agent: self.agent,
                session_id: session_id.clone(),
                model: None,
            },
        };
        self.call(DaemonCommand::SubmitFalsifierVerdict {
            session_id,
            submission,
        })
    }

    fn tool_start_pipeline(&mut self, args: &Args) -> Result<Value, ToolError> {
        let goal = req_str(args, "goal", "")?.trim().to_string();
        let utterance = opt_str(args, "utterance")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let playbook = match opt_str(args, "playbook")? {
            Some(s) => Some(PlaybookKind::parse(s).ok_or_else(|| {
                ToolError::new(format!(
                    "argument `playbook` must be \"feature\" or \"debug\", got {s:?}"
                ))
            })?),
            None => None,
        };
        let explicit_bindings = match args.get("bindings") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) if m.is_empty() => None,
            Some(Value::Object(m)) => Some(parse_binding_map(m)?),
            Some(other) => {
                return Err(ToolError::new(format!(
                    "argument `bindings` must be an object of stage → shorthand, got {}",
                    type_name(other)
                )))
            }
        };
        let project_root = match opt_str(args, "project_root")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(p) => p.to_string(),
            None => {
                let cwd = self.env.cwd.as_deref().ok_or_else(|| {
                    ToolError::new(
                        "argument `project_root` is required (working directory unknown)",
                    )
                })?;
                git_root_of(Path::new(cwd)).to_string_lossy().into_owned()
            }
        };
        let operator = self.known_caller(args)?.map(|s| s.0);
        let text = utterance.unwrap_or_else(|| goal.clone());

        let parsed = self.call(DaemonCommand::ParseUtterance {
            text: text.clone(),
            playbook,
            project: Some(project_root.clone()),
        })?;
        let playbook = match playbook {
            Some(p) => p,
            None => parse_as(
                req_value_of(&parsed, "playbook", "parse_utterance result")?,
                "parsed.playbook",
            )?,
        };
        let mut bindings: BTreeMap<StageKind, StageBinding> = match explicit_bindings {
            Some(b) => b,
            None => match parsed.get("resolved") {
                None | Some(Value::Null) => BTreeMap::new(),
                Some(v) => parse_as(v.clone(), "parsed.resolved")?,
            },
        };
        bindings.retain(|stage, _| stage.playbook() == playbook);
        let req = CreatePipelineReq {
            playbook,
            project_root,
            goal,
            bindings,
            options: None,
            source: PipelineSource {
                via: Via::Mcp,
                utterance: Some(text),
                operator,
            },
        };
        let pipeline = self.call(DaemonCommand::CreatePipeline { req })?;
        Ok(json!({ "pipeline": pipeline, "parsed": parsed, "needs_confirmation": true }))
    }
}

fn req_value_of(obj: &Value, key: &str, what: &str) -> Result<Value, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Err(ToolError::new(format!("{what} is missing `{key}`"))),
        Some(v) => Ok(v.clone()),
    }
}

/// Entry point of `starfane mcp --agent <a>`. Returns the process exit code.
pub fn run_stdio(agent: AgentKind, socket: &Path) -> i32 {
    let env = CallerEnv::capture();
    log::info!(
        "starfane mcp: serving agent={} socket={} pane={:?}",
        agent.as_str(),
        socket.display(),
        env.pane
    );
    let mut server = McpServer::new(agent, env, UdsTransport::new(socket));
    let stdin = io::stdin();
    let stdout = io::stdout();
    match server.serve(stdin.lock(), stdout.lock()) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("starfane mcp: stdio error: {e}");
            eprintln!("starfane mcp: stdio error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starfane_proto::ErrorCode;
    use std::collections::VecDeque;

    type Responder = Box<dyn FnMut(&DaemonCommand) -> Result<Value, CallError>>;

    struct StubTransport {
        calls: Vec<DaemonCommand>,
        responder: Responder,
    }

    impl Transport for StubTransport {
        fn call(&mut self, cmd: DaemonCommand) -> Result<Value, CallError> {
            let r = (self.responder)(&cmd);
            self.calls.push(cmd);
            r
        }
    }

    fn resolved(id: &str) -> Value {
        json!({"status": "resolved", "session_id": id, "via": "env"})
    }

    /// Identity resolves to `s-cached`; everything else returns `{}`.
    fn default_responder() -> Responder {
        Box::new(|cmd| match cmd {
            DaemonCommand::ResolveIdentity { .. } => Ok(resolved("s-cached")),
            DaemonCommand::ListSessions { .. } | DaemonCommand::ReadInbox { .. } => Ok(json!([])),
            _ => Ok(json!({})),
        })
    }

    fn server(responder: Responder) -> McpServer<StubTransport> {
        McpServer::new(
            AgentKind::Codex,
            CallerEnv {
                cwd: Some("/tmp/proj".into()),
                pane: Some(PaneId::new("%3")),
                bind_token: None,
            },
            StubTransport {
                calls: Vec::new(),
                responder,
            },
        )
    }

    fn call_tool(s: &mut McpServer<StubTransport>, name: &str, args: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}});
        let resp = s.handle_line(&req.to_string()).expect("response");
        assert_eq!(resp["id"], 7);
        resp["result"].clone()
    }

    fn session_of(cmd: &DaemonCommand) -> Option<&str> {
        match cmd {
            DaemonCommand::ReadInbox { session_id, .. }
            | DaemonCommand::AckHandoff { session_id, .. }
            | DaemonCommand::SubmitArtifact { session_id, .. }
            | DaemonCommand::SubmitFalsifierVerdict { session_id, .. }
            | DaemonCommand::ReportProgress { session_id, .. } => Some(session_id.as_str()),
            DaemonCommand::CreateHandoff { draft } => Some(draft.from_session.as_str()),
            _ => None,
        }
    }

    #[test]
    fn initialize_negotiates_and_reports_server_info() {
        let mut s = server(default_responder());
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#)
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(r["result"]["serverInfo"]["name"], "starfane");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            starfane_proto::VERSION
        );
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#)
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(
            s.transport.calls.is_empty(),
            "initialize must not touch the daemon"
        );
    }

    #[test]
    fn notifications_ping_and_errors() {
        let mut s = server(default_responder());
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#)
            .unwrap();
        assert_eq!(r["id"], "p");
        assert_eq!(r["result"], json!({}));
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
            .unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = s.handle_line("{not json").unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert_eq!(r["id"], Value::Null);
        let r = s.handle_line("[1,2]").unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = call_tool_raw(&mut s, json!({"name": "nope"}));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    fn call_tool_raw(s: &mut McpServer<StubTransport>, params: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":params});
        s.handle_line(&req.to_string()).unwrap()
    }

    #[test]
    fn tools_list_has_nine_tools() {
        let mut s = server(default_responder());
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, MCP_TOOL_NAMES);
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    }

    #[test]
    fn serve_reads_lines_and_writes_one_line_per_request() {
        let mut s = server(default_responder());
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions"}}"#,
            "\n"
        );
        let mut out = Vec::new();
        s.serve(input.as_bytes(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["result"]["isError"], false);
        assert_eq!(
            lines[1]["result"]["structuredContent"],
            json!({"sessions": []})
        );
        assert_eq!(lines[1]["result"]["content"][0]["type"], "text");
        assert!(matches!(
            s.transport.calls.as_slice(),
            [DaemonCommand::ListSessions {
                include_subagents: false,
                project: None,
                include_ended: false,
            }]
        ));
    }

    #[test]
    fn argument_validation_reports_is_error_without_daemon_calls() {
        let mut s = server(default_responder());
        let cases = [
            ("create_handoff", json!({"intent": "review"}), "`to`"),
            (
                "ack_handoff",
                json!({"handoff_id": "H-1", "status": "pending"}),
                "acked",
            ),
            ("report_milestone", json!({"kind": "progress"}), "`text`"),
            ("report_milestone", json!({"kind": "plan"}), "`plan`"),
            ("report_milestone", json!({"kind": "weird"}), "kind"),
            (
                "submit_falsifier_verdict",
                json!({"review_id":"R-1","finding_id":"F-1","verdict":"downgraded","rationale":"x"}),
                "`to`",
            ),
            (
                "submit_review",
                json!({"review": {"depth": "D9"}}),
                "invalid `review`",
            ),
            ("start_pipeline", json!({"utterance": "x"}), "`goal`"),
            (
                "start_pipeline",
                json!({"goal": "g", "bindings": {"bogus": "codex"}}),
                "unknown stage",
            ),
            (
                "list_sessions",
                json!({"include_subagents": "yes"}),
                "boolean",
            ),
        ];
        for (tool, args, needle) in cases {
            let r = call_tool(&mut s, tool, args);
            assert_eq!(r["isError"], true, "{tool} should fail");
            let msg = r["content"][0]["text"].as_str().unwrap();
            assert!(msg.contains(needle), "{tool}: {msg:?} lacks {needle:?}");
            assert!(r["structuredContent"]["error"].is_string());
        }
        assert!(s.transport.calls.is_empty());
    }

    #[test]
    fn from_session_wins_over_cached_identity() {
        let mut s = server(default_responder());
        // First call resolves and caches the identity.
        let r = call_tool(&mut s, "read_inbox", json!({}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"], json!({"items": []}));
        // Cached: no second ResolveIdentity.
        call_tool(
            &mut s,
            "ack_handoff",
            json!({"handoff_id": "H-1", "status": "done"}),
        );
        // Explicit from_session wins.
        call_tool(
            &mut s,
            "report_milestone",
            json!({"kind": "progress", "text": "half way", "from_session": "s-explicit"}),
        );
        let calls = &s.transport.calls;
        let resolves = calls
            .iter()
            .filter(|c| matches!(c, DaemonCommand::ResolveIdentity { .. }))
            .count();
        assert_eq!(resolves, 1);
        let sessions: Vec<&str> = calls.iter().filter_map(session_of).collect();
        assert_eq!(sessions, vec!["s-cached", "s-cached", "s-explicit"]);
        match &calls[0] {
            DaemonCommand::ResolveIdentity {
                agent, pane, cwd, ..
            } => {
                assert_eq!(*agent, AgentKind::Codex);
                assert_eq!(pane.as_ref().map(|p| p.as_str()), Some("%3"));
                assert_eq!(cwd.as_deref(), Some("/tmp/proj"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn from_session_skips_resolution_entirely() {
        let mut s = server(default_responder());
        call_tool(
            &mut s,
            "read_inbox",
            json!({"from_session": "s-x", "mark_delivered": false}),
        );
        assert!(matches!(
            s.transport.calls.as_slice(),
            [DaemonCommand::ReadInbox { session_id, mark_delivered: false }] if session_id.as_str() == "s-x"
        ));
    }

    #[test]
    fn ambiguous_identity_lists_candidates_and_is_not_cached() {
        let mut s = server(Box::new(|cmd| match cmd {
            DaemonCommand::ResolveIdentity { .. } => {
                Ok(json!({"status": "ambiguous", "candidates": [{
                    "session_id": "s-a", "agent": "codex", "pane": "%1", "pane_resolution": "cwd_match",
                    "cwd": "/tmp/proj", "git_root": null, "model": null, "state": "idle",
                    "state_since_ms": 0, "last_event_id": null, "last_event_ms": 0,
                    "last_message": null, "parent_session_id": null, "is_subagent": false,
                    "subagent_count": 0, "active_subagents": 0, "stage": null, "unread_inbox": 0,
                    "confidence": "exact"
                }]}))
            }
            _ => Ok(json!({})),
        }));
        let r = call_tool(&mut s, "read_inbox", json!({}));
        assert_eq!(r["isError"], true);
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("s-a"), "{text}");
        assert!(text.contains("from_session"));
        assert_eq!(r["structuredContent"]["candidates"][0]["session_id"], "s-a");
        call_tool(&mut s, "read_inbox", json!({}));
        let resolves = s
            .transport
            .calls
            .iter()
            .filter(|c| matches!(c, DaemonCommand::ResolveIdentity { .. }))
            .count();
        assert_eq!(resolves, 2, "non-resolved identities must not be cached");
    }

    #[test]
    fn unknown_identity_and_api_errors_become_is_error() {
        let mut s = server(Box::new(|cmd| match cmd {
            DaemonCommand::ResolveIdentity { .. } => Ok(json!({"status": "unknown"})),
            DaemonCommand::ListSessions { .. } => Err(CallError::Api(ApiError::new(
                ErrorCode::Unavailable,
                "tmux down",
            ))),
            _ => Ok(json!({})),
        }));
        let r = call_tool(
            &mut s,
            "submit_triage",
            json!({"triage": {
                "symptom": "500s", "hypotheses": []
            }}),
        );
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("from_session"));
        let r = call_tool(&mut s, "list_sessions", json!({}));
        assert_eq!(r["isError"], true);
        assert_eq!(r["structuredContent"]["code"], "unavailable");
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tmux down"));
    }

    #[test]
    fn create_handoff_and_falsifier_build_commands() {
        let mut s = server(default_responder());
        let r = call_tool(
            &mut s,
            "create_handoff",
            json!({
                "to": {"stage": {"pipeline_id": "P-1", "stage": "test_review"}},
                "intent": "review", "mode": "read_only", "summary": "done", "body_md": "b",
                "self_test": {"cmd": "cargo test", "outcome": "pass"},
                "from_session": "s-builder"
            }),
        );
        assert_eq!(r["isError"], false, "{r}");
        let r = call_tool(
            &mut s,
            "submit_falsifier_verdict",
            json!({"review_id":"R-1","finding_id":"F-2","verdict":"downgraded","to":"minor",
                   "rationale":"guarded","from_session":"s-f"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        match &s.transport.calls[..] {
            [DaemonCommand::CreateHandoff { draft }, DaemonCommand::SubmitFalsifierVerdict {
                session_id,
                submission,
            }] => {
                assert_eq!(draft.from_session.as_str(), "s-builder");
                assert!(draft.self_test.is_some());
                assert_eq!(session_id.as_str(), "s-f");
                assert_eq!(
                    submission.verdict,
                    FalsifierVerdict::Downgraded {
                        to: Severity::Minor
                    }
                );
                assert_eq!(submission.falsifier.session_id.as_str(), "s-f");
                assert_eq!(submission.falsifier.agent, AgentKind::Codex);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn milestone_plan_without_header_is_submitted() {
        let mut s = server(default_responder());
        let r = call_tool(
            &mut s,
            "report_milestone",
            json!({"kind": "plan", "from_session": "s-p", "plan": {
                "goal": "g", "design_md": "d", "tasks": [{"id": "t1", "title": "x"}],
                "acceptance": ["a"], "test_plan": ["t"]
            }}),
        );
        assert_eq!(r["isError"], false, "{r}");
        assert!(matches!(
            &s.transport.calls[..],
            [DaemonCommand::SubmitArtifact {
                artifact: ArtifactPayload::Plan(_),
                ..
            }]
        ));
    }

    #[test]
    fn start_pipeline_parses_then_creates() {
        let mut queue: VecDeque<Value> = VecDeque::from(vec![
            json!({"playbook": "debug", "playbook_explicit": false, "goal": "500 on login",
                   "bindings": {}, "resolved": {"triage": {"agent": "codex"}, "deep": {"agent": "claude"}},
                   "warnings": []}),
            json!({"id": "P-1", "status": "awaiting_binding_confirm"}),
        ]);
        let mut s = server(Box::new(move |_| {
            Ok(queue.pop_front().unwrap_or(json!({})))
        }));
        let r = call_tool(
            &mut s,
            "start_pipeline",
            json!({"goal": "500 on login", "utterance": "查错 登录 500，快筛用 codex",
                   "project_root": "/repo"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["needs_confirmation"], true);
        assert_eq!(r["structuredContent"]["pipeline"]["id"], "P-1");
        match &s.transport.calls[..] {
            [DaemonCommand::ParseUtterance {
                text,
                playbook,
                project,
            }, DaemonCommand::CreatePipeline { req }] => {
                assert_eq!(text, "查错 登录 500，快筛用 codex");
                assert_eq!(*playbook, None);
                assert_eq!(project.as_deref(), Some("/repo"));
                assert_eq!(req.playbook, PlaybookKind::Debug);
                assert_eq!(req.goal, "500 on login");
                assert_eq!(req.project_root, "/repo");
                assert_eq!(req.source.via, Via::Mcp);
                assert_eq!(
                    req.bindings[&StageKind::Deep].agent,
                    Some(AgentKind::Claude)
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn start_pipeline_explicit_bindings_override_parsed() {
        let mut s = server(Box::new(|cmd| match cmd {
            DaemonCommand::ParseUtterance { .. } => Ok(json!({
                "playbook": "feature", "resolved": {"plan": {"agent": "claude"}}
            })),
            _ => Ok(json!({"id": "P-2"})),
        }));
        let r = call_tool(
            &mut s,
            "start_pipeline",
            json!({"goal": "add button", "playbook": "feature", "project_root": "/repo",
                   "bindings": {"implement": "codex:gpt-5@high", "test_review": "agy /swarm"}}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let DaemonCommand::CreatePipeline { req } = &s.transport.calls[1] else {
            panic!("expected CreatePipeline");
        };
        assert_eq!(req.bindings.len(), 2);
        assert!(!req.bindings.contains_key(&StageKind::Plan));
        assert_eq!(
            req.bindings[&StageKind::Implement].model.as_deref(),
            Some("gpt-5")
        );
        assert_eq!(req.source.utterance.as_deref(), Some("add button"));
    }

    #[test]
    fn git_root_walks_up() {
        let dir = std::env::temp_dir().join(format!("starfane-mcp-git-{}", std::process::id()));
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(git_root_of(&nested), dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
