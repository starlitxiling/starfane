//! Structured transcript JSONL reader for Claude Code, Codex CLI, and Antigravity (AGY).
//!
//! Reads at most the trailing 8 MiB of a session's transcript JSONL file, extracts the
//! last `limit` clean user/assistant messages (truncating each message at 64 KiB on a UTF-8
//! character boundary), hydrates truncated AGY entries from sibling `transcript_full.jsonl`
//! by `step_index`, and skips malformed lines without ever panicking.

use super::metrics::{
    extract_transcript_metrics, file_signature, merge_metrics, resolve_codex_transcript_by_id_in,
};
use super::{lock, Core};
use baton_proto::{AgentKind, ApiError, SessionId, SessionMetrics, SessionRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Hard cap on trailing bytes kept from any transcript file (8 MiB).
pub const MAX_TRANSCRIPT_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Hard cap on backward scan distance when skipping giant non-message JSONL lines (32 MiB).
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 32 * 1024 * 1024;
/// Backward read chunk size for `read_tail_utf8`.
const TAIL_CHUNK_BYTES: usize = 512 * 1024;
/// Hard cap on each individual message body (64 KiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Default number of messages returned by `GET /api/sessions/{id}/transcript`.
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 60;
/// Maximum allowed `limit` query value.
pub const MAX_TRANSCRIPT_LIMIT: usize = 500;
/// Maximum in-memory cached transcripts.
const MAX_CACHED_TRANSCRIPTS: usize = 256;

/// File `(mtime_ns, len)` cache entry for parsed structured transcripts.
#[derive(Debug, Clone)]
pub(crate) struct CachedTranscript {
    pub sig: (u128, u64),
    pub full_sig: Option<(u128, u64)>,
    pub limit: usize,
    pub session_metrics: Option<SessionMetrics>,
    pub resp: TranscriptResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptResponse {
    /// `"transcript"` when backed by a readable JSONL transcript file, `"none"` otherwise.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    pub messages: Vec<TranscriptMessage>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub total_messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SessionMetrics>,
}

impl TranscriptResponse {
    fn none(metrics: Option<SessionMetrics>) -> Self {
        Self {
            source: "none".to_string(),
            transcript_path: None,
            messages: Vec::new(),
            has_more: false,
            total_messages: 0,
            metrics,
        }
    }
}

impl Core {
    /// Read and parse recent structured conversation messages for `sid`, using the
    /// `(mtime_ns, len)` cache whenever both the compact transcript and sibling
    /// `transcript_full.jsonl` (if any) are unchanged.
    pub async fn session_transcript(
        &self,
        sid: &SessionId,
        limit: usize,
    ) -> Result<TranscriptResponse, ApiError> {
        let clamped = limit.clamp(1, MAX_TRANSCRIPT_LIMIT);
        let (session, cached) = {
            let st = lock(&self.inner.state);
            let session = st
                .sessions
                .get(sid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("session {sid}")))?;
            let cached = session
                .transcript_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|raw| {
                    let pb = PathBuf::from(raw);
                    if session.agent == AgentKind::Agy {
                        canonicalize_agy_transcript_path(&pb)
                    } else {
                        pb
                    }
                })
                .and_then(|pb| {
                    let sig = file_signature(&pb)?;
                    let full_sig =
                        sibling_transcript_full(Some(&pb)).and_then(|f| file_signature(&f));
                    let hit = st.transcript_cache.get(&pb)?;
                    if hit.sig == sig
                        && hit.full_sig == full_sig
                        && hit.limit >= clamped
                        && hit.session_metrics == session.metrics
                    {
                        let mut resp = hit.resp.clone();
                        let had_more_beyond_cache = hit.resp.has_more;
                        if resp.messages.len() > clamped {
                            let drop_n = resp.messages.len() - clamped;
                            resp.messages.drain(0..drop_n);
                            resp.has_more = true;
                        } else {
                            resp.has_more = had_more_beyond_cache;
                        }
                        Some(resp)
                    } else {
                        None
                    }
                });
            (session, cached)
        };
        if let Some(resp) = cached {
            return Ok(resp);
        }

        let session_for_cache = session.metrics.clone();
        let resp = tokio::task::spawn_blocking(move || load_session_transcript(&session, clamped))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;

        if resp.source == "transcript" {
            if let Some(path_str) = resp.transcript_path.as_deref() {
                let pb = PathBuf::from(path_str);
                if let Some(sig) = file_signature(&pb) {
                    let full_sig =
                        sibling_transcript_full(Some(&pb)).and_then(|f| file_signature(&f));
                    let mut st = lock(&self.inner.state);
                    if st.transcript_cache.len() >= MAX_CACHED_TRANSCRIPTS {
                        st.transcript_cache.clear();
                    }
                    st.transcript_cache.insert(
                        pb,
                        CachedTranscript {
                            sig,
                            full_sig,
                            limit: clamped,
                            session_metrics: session_for_cache,
                            resp: resp.clone(),
                        },
                    );
                }
            }
        }
        Ok(resp)
    }
}

/// Prefer compact `transcript.jsonl` over sibling `transcript_full.jsonl` when both exist,
/// because `transcript_full.jsonl` often contains multi-megabyte `GENERIC` tool outputs.
pub(crate) fn canonicalize_agy_transcript_path(path: &Path) -> PathBuf {
    if path.file_name().and_then(|n| n.to_str()) == Some("transcript_full.jsonl") {
        let compact = path.with_file_name("transcript.jsonl");
        if compact.is_file() {
            return compact;
        }
    }
    path.to_path_buf()
}

/// Resolve transcript path (exact matches only—never guess across unrelated sessions)
/// and parse up to `limit` trailing messages.
pub(crate) fn load_session_transcript(session: &SessionRecord, limit: usize) -> TranscriptResponse {
    let Some(path) = resolve_transcript_path(session) else {
        return TranscriptResponse::none(session.metrics.clone());
    };
    let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let Ok(tail) = read_tail_utf8(&path, MAX_TRANSCRIPT_READ_BYTES) else {
        return TranscriptResponse::none(session.metrics.clone());
    };
    let (messages, total_parsed) = if session.agent == AgentKind::Agy {
        parse_agy_jsonl_with_hydration_counted(&tail, Some(&path), limit)
    } else {
        parse_transcript_for_agent_counted(session.agent, &tail, limit)
    };
    let has_more = total_parsed > messages.len() || file_len > MAX_TRANSCRIPT_READ_BYTES;
    let now_ms = baton_proto::id::now_ms();
    let parsed_metrics = extract_transcript_metrics(session.agent, &tail, now_ms);
    let metrics = if session
        .metrics
        .as_ref()
        .is_some_and(|m| m.source == "cc_switch")
    {
        super::metrics::merge_cc_switch_with_fallback(session.metrics.clone(), parsed_metrics, None)
    } else {
        merge_metrics(parsed_metrics, session.metrics.clone())
    };
    TranscriptResponse {
        source: "transcript".to_string(),
        transcript_path: Some(path.to_string_lossy().into_owned()),
        messages,
        has_more,
        total_messages: total_parsed,
        metrics,
    }
}

/// Locate the transcript JSONL file for `session` using strictly exact identity matches:
/// 1. Persisted `session.transcript_path` reported by hooks or exact PID/resume mapping.
/// 2. For non-legacy sessions (`!discovered_only`), deterministic exact path by `session_id`:
///    - AGY: `~/.gemini/antigravity-cli/brain/<session_id>/.system_generated/logs/transcript.jsonl`
///    - Claude: `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`
pub(crate) fn resolve_transcript_path(session: &SessionRecord) -> Option<PathBuf> {
    if let Some(p) = session
        .transcript_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut pb = PathBuf::from(p);
        if session.agent == AgentKind::Agy {
            pb = canonicalize_agy_transcript_path(&pb);
        }
        if pb.is_file() {
            return Some(pb);
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    resolve_transcript_path_in(&home, session)
}

pub(crate) fn resolve_transcript_path_in(home: &Path, session: &SessionRecord) -> Option<PathBuf> {
    let sid = session.session_id.as_str();
    if sid.is_empty() || sid.contains("..") {
        return None;
    }

    match session.agent {
        AgentKind::Agy => {
            if sid.starts_with("tmux-") || sid.contains('/') {
                return None;
            }
            let p = home
                .join(".gemini/antigravity-cli/brain")
                .join(sid)
                .join(".system_generated/logs/transcript.jsonl");
            p.is_file().then_some(p)
        }
        AgentKind::Claude => {
            if session.discovered_only && !session.is_subagent {
                return None;
            }
            let projects_dir = home.join(".claude/projects");
            if session.is_subagent {
                // Subagent session_id format: `<root_session_id>/<agent_id>`.
                let (root_id, agent_id) = sid.split_once('/')?;
                if root_id.is_empty()
                    || agent_id.is_empty()
                    || root_id.contains('/')
                    || agent_id.contains('/')
                {
                    return None;
                }
                let rel = PathBuf::from(root_id)
                    .join("subagents")
                    .join(format!("agent-{agent_id}.jsonl"));
                for dir in [session.cwd.as_deref(), session.git_root.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    let cand = projects_dir.join(encode_claude_project_dir(dir)).join(&rel);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
                if let Ok(entries) = std::fs::read_dir(&projects_dir) {
                    for entry in entries.flatten() {
                        let cand = entry.path().join(&rel);
                        if cand.is_file() {
                            return Some(cand);
                        }
                    }
                }
                return None;
            }
            if sid.contains('/') {
                return None;
            }
            for dir in [session.cwd.as_deref(), session.git_root.as_deref()]
                .into_iter()
                .flatten()
            {
                let enc = encode_claude_project_dir(dir);
                let p = projects_dir.join(enc).join(format!("{sid}.jsonl"));
                if p.is_file() {
                    return Some(p);
                }
            }
            None
        }
        AgentKind::Codex => {
            if session.discovered_only || sid.starts_with("tmux-") || sid.contains('/') {
                return None;
            }
            resolve_codex_transcript_by_id_in(home, sid)
        }
    }
}

fn encode_claude_project_dir(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_skippable_giant_line(line: &[u8]) -> bool {
    if line.len() <= 1024 * 1024 {
        return false;
    }
    contains_subslice(line, b"\"type\":\"compacted\"")
        || contains_subslice(line, b"\"type\": \"compacted\"")
        || contains_subslice(line, b"\"type\":\"GENERIC\"")
        || contains_subslice(line, b"\"type\": \"GENERIC\"")
}

/// Read complete trailing JSONL lines totaling at most `max_bytes`, scanning backwards in
/// 512 KiB chunks up to 16 MiB and skipping >1 MiB `compacted` / `GENERIC` lines so huge
/// snapshots or tool blobs never blank out recent turns.
pub(crate) fn read_tail_utf8(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 || max_bytes == 0 {
        return Ok(String::new());
    }
    if len <= max_bytes {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf)?;
        return Ok(String::from_utf8_lossy(&buf).into_owned());
    }

    let scan_budget = MAX_TRANSCRIPT_SCAN_BYTES.max(max_bytes.saturating_mul(8));
    let max_out = max_bytes as usize;
    let mut cursor = len;
    let mut scanned = 0u64;
    let mut carry: Vec<u8> = Vec::new();
    let mut kept_rev: Vec<Vec<u8>> = Vec::new();
    let mut kept_bytes: usize = 0;

    'scan: while cursor > 0 && scanned < scan_budget {
        let start = cursor.saturating_sub(TAIL_CHUNK_BYTES as u64);
        let read_len = (cursor - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0u8; read_len];
        file.read_exact(&mut chunk)?;
        cursor = start;
        scanned = scanned.saturating_add(read_len as u64);

        if !carry.is_empty() {
            chunk.extend_from_slice(&carry);
            carry.clear();
        }

        let complete_slice: &[u8] = if cursor == 0 {
            &chunk
        } else if let Some(first_nl) = chunk.iter().position(|&b| b == b'\n') {
            carry = chunk[..first_nl].to_vec();
            &chunk[first_nl + 1..]
        } else {
            carry = chunk;
            if carry.len() as u64 > scan_budget {
                break;
            }
            continue;
        };

        for raw_line in complete_slice.rsplit(|&b| b == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() || is_skippable_giant_line(line) {
                continue;
            }
            let needed = line.len() + 1;
            if kept_bytes + needed > max_out {
                if !kept_rev.is_empty() {
                    break 'scan;
                }
                // Single non-skippable line exceeds max_out; keep scanning earlier lines if small,
                // otherwise truncate cleanly if nothing else was found.
                continue;
            }
            kept_bytes += needed;
            kept_rev.push(line.to_vec());
        }
    }

    if cursor == 0 && !carry.is_empty() && !is_skippable_giant_line(&carry) {
        let line = carry.strip_suffix(b"\r").unwrap_or(&carry);
        if !line.is_empty() && kept_bytes + line.len() < max_out {
            kept_bytes += line.len() + 1;
            kept_rev.push(line.to_vec());
        }
    }

    kept_rev.reverse();
    let mut out = String::with_capacity(kept_bytes);
    for line in kept_rev {
        out.push_str(&String::from_utf8_lossy(&line));
        out.push('\n');
    }
    Ok(out)
}

/// Dispatch JSONL parsing for the given agent, keeping only the last `limit` messages.
#[cfg(test)]
pub(crate) fn parse_transcript_for_agent(
    agent: AgentKind,
    text: &str,
    limit: usize,
) -> Vec<TranscriptMessage> {
    parse_transcript_for_agent_counted(agent, text, limit).0
}

fn parse_transcript_for_agent_counted(
    agent: AgentKind,
    text: &str,
    limit: usize,
) -> (Vec<TranscriptMessage>, usize) {
    let mut msgs = match agent {
        AgentKind::Claude => parse_claude_jsonl(text),
        AgentKind::Codex => parse_codex_jsonl(text),
        AgentKind::Agy => parse_agy_jsonl(text),
    };
    let total = msgs.len();
    if msgs.len() > limit {
        let drop_n = msgs.len() - limit;
        msgs.drain(0..drop_n);
    }
    (msgs, total)
}

// ---------------------------------------------------------------------------
// Claude Code JSONL parser
// ---------------------------------------------------------------------------

fn parse_claude_jsonl(text: &str) -> Vec<TranscriptMessage> {
    let mut out: Vec<TranscriptMessage> = Vec::new();
    let mut last_assistant_msg_id: Option<String> = None;
    let mut pending_tools: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if obj.get("isMeta").and_then(Value::as_bool) == Some(true)
            || obj.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);

        match kind {
            "user" => {
                let Some(content) = obj.get("message").and_then(|m| m.get("content")) else {
                    continue;
                };
                if let Some(clean) = extract_claude_user_text(content) {
                    last_assistant_msg_id = None;
                    pending_tools.clear();
                    push_message(
                        &mut out,
                        TranscriptMessage {
                            role: TranscriptRole::User,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: Vec::new(),
                        },
                    );
                }
            }
            "assistant" => {
                let Some(msg) = obj.get("message") else {
                    continue;
                };
                let msg_id = msg.get("id").and_then(Value::as_str).map(str::to_string);
                let (body, tools) = extract_claude_assistant_content(msg.get("content"));

                // Merge consecutive chunks emitted for the same Claude `message.id`.
                if let (Some(id), Some(prev_id)) = (&msg_id, &last_assistant_msg_id) {
                    if id == prev_id {
                        if let Some(last) = out
                            .last_mut()
                            .filter(|m| m.role == TranscriptRole::Assistant)
                        {
                            if !body.is_empty() {
                                if !last.text.is_empty() {
                                    last.text.push_str("\n\n");
                                }
                                last.text.push_str(&body);
                                last.text = truncate_utf8(&last.text, MAX_MESSAGE_BYTES);
                            }
                            merge_tools(&mut last.tools, &tools);
                            if last.ts_ms.is_none() {
                                last.ts_ms = ts_ms;
                            }
                            continue;
                        }
                    }
                }

                merge_tools(&mut pending_tools, &tools);
                if body.is_empty() {
                    continue;
                }
                let attached = std::mem::take(&mut pending_tools);
                push_message(
                    &mut out,
                    TranscriptMessage {
                        role: TranscriptRole::Assistant,
                        text: truncate_utf8(&body, MAX_MESSAGE_BYTES),
                        ts_ms,
                        tools: attached,
                    },
                );
                last_assistant_msg_id = msg_id;
            }
            _ => {}
        }
    }
    flush_pending_tools(&mut out, &mut pending_tools);
    out
}

fn extract_claude_user_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => clean_user_text(s),
        Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            let mut has_image = false;
            for b in blocks {
                let btype = b.get("type").and_then(Value::as_str).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if let Some(clean) = clean_user_text(t) {
                                parts.push(clean);
                            }
                        }
                    }
                    "image" | "input_image" => {
                        has_image = true;
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                has_image.then(|| "[图片]".to_string())
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    }
}

fn extract_claude_assistant_content(content: Option<&Value>) -> (String, Vec<String>) {
    let Some(val) = content else {
        return (String::new(), Vec::new());
    };
    match val {
        Value::String(s) => (s.trim().to_string(), Vec::new()),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            let mut tools = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            let t = t.trim();
                            if !t.is_empty() {
                                texts.push(t);
                            }
                        }
                    }
                    Some("tool_use") => {
                        if let Some(name) = b.get("name").and_then(Value::as_str) {
                            push_unique(&mut tools, name);
                        }
                    }
                    _ => {} // skip thinking / redacted_thinking / server_tool_use etc.
                }
            }
            (texts.join("\n\n"), tools)
        }
        _ => (String::new(), Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Codex CLI JSONL parser
// ---------------------------------------------------------------------------

fn parse_codex_jsonl(text: &str) -> Vec<TranscriptMessage> {
    let mut out: Vec<TranscriptMessage> = Vec::new();
    let mut pending_tools: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(payload) = obj.get("payload") else {
            continue;
        };
        let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);

        match (kind, ptype) {
            ("response_item", "message") => {
                let role = match payload.get("role").and_then(Value::as_str) {
                    Some("user") => TranscriptRole::User,
                    Some("assistant") => TranscriptRole::Assistant,
                    _ => continue, // skip developer/system
                };
                let (body, has_image) = extract_codex_content(payload.get("content"));
                if role == TranscriptRole::User {
                    pending_tools.clear();
                    if let Some(clean) = clean_codex_user_text(&body, has_image) {
                        push_message(
                            &mut out,
                            TranscriptMessage {
                                role,
                                text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: Vec::new(),
                            },
                        );
                    }
                } else {
                    let clean = strip_codex_internal_tags(&body);
                    if clean.is_empty() {
                        continue;
                    }
                    let attached = std::mem::take(&mut pending_tools);
                    push_message(
                        &mut out,
                        TranscriptMessage {
                            role,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                        },
                    );
                }
            }
            ("response_item", "custom_tool_call" | "function_call") => {
                if let Some(name) = payload.get("name").and_then(Value::as_str) {
                    push_unique(&mut pending_tools, name);
                }
            }
            ("event_msg", "user_message") => {
                pending_tools.clear();
                let msg = payload.get("message").and_then(Value::as_str).unwrap_or("");
                let has_images = payload
                    .get("images")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                    || payload
                        .get("local_images")
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty());
                if let Some(clean) = clean_codex_user_text(msg, has_images) {
                    push_message(
                        &mut out,
                        TranscriptMessage {
                            role: TranscriptRole::User,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: Vec::new(),
                        },
                    );
                }
            }
            ("event_msg", "agent_message") => {
                if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                    let clean = strip_codex_internal_tags(msg);
                    if !clean.is_empty() {
                        let attached = std::mem::take(&mut pending_tools);
                        push_message(
                            &mut out,
                            TranscriptMessage {
                                role: TranscriptRole::Assistant,
                                text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: attached,
                            },
                        );
                    }
                }
            }
            ("event_msg", "task_complete") => {
                if let Some(msg) = payload.get("last_agent_message").and_then(Value::as_str) {
                    let clean = strip_codex_internal_tags(msg);
                    if !clean.is_empty() {
                        let attached = std::mem::take(&mut pending_tools);
                        push_message(
                            &mut out,
                            TranscriptMessage {
                                role: TranscriptRole::Assistant,
                                text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: attached,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }
    flush_pending_tools(&mut out, &mut pending_tools);
    out
}

fn extract_codex_content(content: Option<&Value>) -> (String, bool) {
    let Some(val) = content else {
        return (String::new(), false);
    };
    match val {
        Value::String(s) => (s.trim().to_string(), false),
        Value::Array(items) => {
            let mut parts = Vec::new();
            let mut has_image = false;
            for item in items {
                let itype = item.get("type").and_then(Value::as_str).unwrap_or("");
                if matches!(itype, "input_text" | "output_text" | "text") {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        if let Some(clean) = strip_codex_block_if_injected(t) {
                            parts.push(clean);
                        }
                    }
                } else if matches!(itype, "input_image" | "image") {
                    has_image = true;
                }
            }
            (parts.join("\n\n"), has_image)
        }
        _ => (String::new(), false),
    }
}

fn is_codex_injected_prompt(t: &str) -> bool {
    for prefix in [
        "# AGENTS.md instructions",
        "<environment_context>",
        "<permissions instructions>",
        "<collaboration_mode>",
        "<app-context>",
        "<INSTRUCTIONS>",
        "<turn_aborted>",
        "<guardian_tool_descriptions>",
    ] {
        if t.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn strip_codex_block_if_injected(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || is_codex_injected_prompt(t) {
        return None;
    }
    clean_user_text(t)
}

fn clean_codex_user_text(raw: &str, has_image: bool) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return has_image.then(|| "[图片]".to_string());
    }
    if is_codex_injected_prompt(t) {
        return has_image.then(|| "[图片]".to_string());
    }
    clean_user_text(t).or_else(|| has_image.then(|| "[图片]".to_string()))
}

// ---------------------------------------------------------------------------
// Antigravity (AGY) transcript.jsonl parser
// ---------------------------------------------------------------------------

struct AgyParsedMsg {
    msg: TranscriptMessage,
    step_index: Option<u64>,
    needs_content_hydration: bool,
    needs_tools_hydration: bool,
}

fn agy_needs_content_hydration(obj: &Value, raw_content: &str) -> bool {
    let flagged = obj
        .get("truncated_fields")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("content")));
    flagged || raw_content.contains("<truncated ")
}

fn agy_needs_tools_hydration(obj: &Value) -> bool {
    let flagged = obj
        .get("truncated_fields")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("tool_calls")));
    if flagged {
        return true;
    }
    obj.get("tool_calls")
        .map(|v| v.to_string().contains("<truncated "))
        .unwrap_or(false)
}

fn agy_arg_str(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        if let Ok(inner) = serde_json::from_str::<String>(s) {
            let trimmed = inner.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
    }
    let trimmed = s.strip_prefix('"').unwrap_or(s).trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_agy_tool_call_text(calls: &[Value]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for tc in calls {
        let name = tc.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(args) = tc.get("args") else {
            continue;
        };
        match name {
            "exit_plan_mode" => {
                if let Some(plan) = args.get("plan").and_then(agy_arg_str) {
                    parts.push(plan);
                }
            }
            "send_message" => {
                if let Some(msg) = args.get("Message").and_then(agy_arg_str) {
                    let rec = args
                        .get("Recipient")
                        .and_then(agy_arg_str)
                        .map(|r| super::util::short_id(&r))
                        .unwrap_or_default();
                    if rec.is_empty() {
                        parts.push(msg);
                    } else {
                        parts.push(format!("[发送给 {rec}]\n{msg}"));
                    }
                }
            }
            "post_message" => {
                if let Some(txt) = args.get("text").and_then(agy_arg_str) {
                    parts.push(format!("[群组消息]\n{txt}"));
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn extract_agy_interagent_message(raw: &str) -> Option<String> {
    let body = raw
        .find("<SYSTEM_MESSAGE>")
        .map(|i| &raw[i + "<SYSTEM_MESSAGE>".len()..])
        .unwrap_or(raw);
    let body = body
        .find("</SYSTEM_MESSAGE>")
        .map(|i| &body[..i])
        .unwrap_or(body)
        .trim();
    if !body.contains("[Message]") {
        return None;
    }
    let sender = body
        .split("sender=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("")
        .trim();
    if sender.is_empty() || sender == "system" {
        return None;
    }
    let content = body.split("content=").nth(1)?.trim();
    if content.is_empty() {
        return None;
    }
    let short_sender = super::util::short_id(sender);
    Some(format!("[来自 {short_sender} 消息]\n{content}"))
}

fn sibling_transcript_full(transcript_path: Option<&Path>) -> Option<PathBuf> {
    let p = transcript_path?;
    if p.file_name().and_then(|n| n.to_str()) == Some("transcript_full.jsonl") {
        return None;
    }
    let full = p.with_file_name("transcript_full.jsonl");
    full.is_file().then_some(full)
}

/// Zero-allocation fast pre-filter extracting the integer after `"step_index":` in a JSONL line.
fn fast_extract_step_index(line: &str) -> Option<u64> {
    let pos = line.find("\"step_index\"")?;
    let rest = line[pos + "\"step_index\"".len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    rest[..digits_len].parse::<u64>().ok()
}

/// Stream-scan `transcript_full.jsonl` once to fetch full entries for the requested `step_index`es.
fn read_agy_full_steps(full_path: &Path, needed: &HashSet<u64>) -> HashMap<u64, Value> {
    let mut found = HashMap::with_capacity(needed.len());
    let Ok(file) = File::open(full_path) else {
        return found;
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let Some(step_idx) = fast_extract_step_index(&line) else {
            continue;
        };
        if !needed.contains(&step_idx) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if obj.get("step_index").and_then(Value::as_u64) != Some(step_idx) {
            continue;
        }
        found.insert(step_idx, obj);
        if found.len() == needed.len() {
            break;
        }
    }
    found
}

fn push_agy_message(out: &mut Vec<AgyParsedMsg>, item: AgyParsedMsg) {
    if let Some(last) = out.last_mut() {
        let same_text = last.msg.role == item.msg.role
            && (last.msg.text == item.msg.text
                || normalize_ws_for_dedup(&last.msg.text)
                    == normalize_ws_for_dedup(&item.msg.text));
        let allow_dedup = match item.msg.role {
            TranscriptRole::Assistant => true,
            TranscriptRole::User => item.step_index.is_some() && last.step_index == item.step_index,
        };
        if same_text && allow_dedup {
            merge_tools(&mut last.msg.tools, &item.msg.tools);
            if last.msg.ts_ms.is_none() {
                last.msg.ts_ms = item.msg.ts_ms;
            }
            return;
        }
    }
    out.push(item);
}

fn parse_agy_jsonl(text: &str) -> Vec<TranscriptMessage> {
    parse_agy_jsonl_with_hydration_counted(text, None, usize::MAX).0
}

/// Parse compact AGY `transcript.jsonl`, keep the trailing `limit` messages, and hydrate any
/// truncated `content` or `tool_calls` fields from sibling `transcript_full.jsonl` by `step_index`.
#[cfg(test)]
pub(crate) fn parse_agy_jsonl_with_hydration(
    text: &str,
    transcript_path: Option<&Path>,
    limit: usize,
) -> Vec<TranscriptMessage> {
    parse_agy_jsonl_with_hydration_counted(text, transcript_path, limit).0
}

fn parse_agy_jsonl_with_hydration_counted(
    text: &str,
    transcript_path: Option<&Path>,
    limit: usize,
) -> (Vec<TranscriptMessage>, usize) {
    let mut items: Vec<AgyParsedMsg> = Vec::new();
    let mut pending_tools: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let source = obj.get("source").and_then(Value::as_str).unwrap_or("");
        let step_index = obj.get("step_index").and_then(Value::as_u64);
        let ts_ms = obj
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);

        match kind {
            "USER_INPUT" => {
                if source == "SYSTEM" {
                    continue;
                }
                pending_tools.clear();
                let raw = obj.get("content").and_then(Value::as_str).unwrap_or("");
                let has_media = obj
                    .get("media")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty());
                let clean =
                    clean_agy_user_text(raw).or_else(|| has_media.then(|| "[图片]".to_string()));
                if let Some(clean) = clean {
                    let needs_content_hydration = step_index.is_some()
                        && !raw.is_empty()
                        && agy_needs_content_hydration(&obj, raw);
                    push_agy_message(
                        &mut items,
                        AgyParsedMsg {
                            msg: TranscriptMessage {
                                role: TranscriptRole::User,
                                text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: Vec::new(),
                            },
                            step_index,
                            needs_content_hydration,
                            needs_tools_hydration: false,
                        },
                    );
                }
            }
            "PLANNER_RESPONSE" => {
                let calls = obj.get("tool_calls").and_then(Value::as_array);
                if let Some(calls) = calls {
                    for tc in calls {
                        if let Some(name) = tc.get("name").and_then(Value::as_str) {
                            push_unique(&mut pending_tools, name);
                        }
                    }
                }
                // Deliberately ignore `thinking`; surface visible `content`, or structured
                // user-visible payloads inside `exit_plan_mode` / `send_message` / `post_message`.
                let content_opt = obj
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if let Some(content) = content_opt {
                    let attached = std::mem::take(&mut pending_tools);
                    let needs_content_hydration =
                        step_index.is_some() && agy_needs_content_hydration(&obj, content);
                    push_agy_message(
                        &mut items,
                        AgyParsedMsg {
                            msg: TranscriptMessage {
                                role: TranscriptRole::Assistant,
                                text: truncate_utf8(content, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: attached,
                            },
                            step_index,
                            needs_content_hydration,
                            needs_tools_hydration: false,
                        },
                    );
                } else if let Some(calls) = calls {
                    if let Some(tc_text) = extract_agy_tool_call_text(calls) {
                        let attached = std::mem::take(&mut pending_tools);
                        let needs_tools_hydration =
                            step_index.is_some() && agy_needs_tools_hydration(&obj);
                        push_agy_message(
                            &mut items,
                            AgyParsedMsg {
                                msg: TranscriptMessage {
                                    role: TranscriptRole::Assistant,
                                    text: truncate_utf8(&tc_text, MAX_MESSAGE_BYTES),
                                    ts_ms,
                                    tools: attached,
                                },
                                step_index,
                                needs_content_hydration: false,
                                needs_tools_hydration,
                            },
                        );
                    }
                }
            }
            "SYSTEM_MESSAGE" => {
                if let Some(raw) = obj.get("content").and_then(Value::as_str) {
                    if let Some(msg_text) = extract_agy_interagent_message(raw) {
                        let needs_content_hydration =
                            step_index.is_some() && agy_needs_content_hydration(&obj, raw);
                        push_agy_message(
                            &mut items,
                            AgyParsedMsg {
                                msg: TranscriptMessage {
                                    role: TranscriptRole::User,
                                    text: truncate_utf8(&msg_text, MAX_MESSAGE_BYTES),
                                    ts_ms,
                                    tools: Vec::new(),
                                },
                                step_index,
                                needs_content_hydration,
                                needs_tools_hydration: false,
                            },
                        );
                    }
                }
            }
            "ERROR_MESSAGE" => {
                let err = obj
                    .get("error")
                    .and_then(Value::as_str)
                    .or_else(|| obj.get("content").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if let Some(err_text) = err {
                    let attached = std::mem::take(&mut pending_tools);
                    items.push(AgyParsedMsg {
                        msg: TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: truncate_utf8(&format!("[错误] {err_text}"), MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                        },
                        step_index,
                        needs_content_hydration: false,
                        needs_tools_hydration: false,
                    });
                }
            }
            _ => {} // skip GENERIC tool outputs, internal status, etc.
        }
    }

    if !pending_tools.is_empty() {
        let attached = std::mem::take(&mut pending_tools);
        let ts_ms = items.last().and_then(|it| it.msg.ts_ms);
        items.push(AgyParsedMsg {
            msg: TranscriptMessage {
                role: TranscriptRole::Assistant,
                text: String::new(),
                ts_ms,
                tools: attached,
            },
            step_index: None,
            needs_content_hydration: false,
            needs_tools_hydration: false,
        });
    }

    let total_parsed = items.len();
    if items.len() > limit {
        let drop_n = items.len() - limit;
        items.drain(0..drop_n);
    }

    let needed: HashSet<u64> = items
        .iter()
        .filter(|it| it.needs_content_hydration || it.needs_tools_hydration)
        .filter_map(|it| it.step_index)
        .collect();

    if !needed.is_empty() {
        if let Some(full_path) = sibling_transcript_full(transcript_path) {
            let full_map = read_agy_full_steps(&full_path, &needed);
            for it in &mut items {
                let Some(step_idx) = it.step_index else {
                    continue;
                };
                let Some(full_obj) = full_map.get(&step_idx) else {
                    continue;
                };
                if it.needs_content_hydration {
                    if let Some(full_raw) = full_obj.get("content").and_then(Value::as_str) {
                        match it.msg.role {
                            TranscriptRole::User => {
                                if full_obj.get("type").and_then(Value::as_str)
                                    == Some("SYSTEM_MESSAGE")
                                {
                                    if let Some(clean) = extract_agy_interagent_message(full_raw) {
                                        it.msg.text = truncate_utf8(&clean, MAX_MESSAGE_BYTES);
                                    }
                                } else if let Some(clean) = clean_agy_user_text(full_raw) {
                                    it.msg.text = truncate_utf8(&clean, MAX_MESSAGE_BYTES);
                                }
                            }
                            TranscriptRole::Assistant => {
                                let trimmed = full_raw.trim();
                                if !trimmed.is_empty() {
                                    it.msg.text = truncate_utf8(trimmed, MAX_MESSAGE_BYTES);
                                }
                            }
                        }
                    }
                } else if it.needs_tools_hydration {
                    if let Some(calls) = full_obj.get("tool_calls").and_then(Value::as_array) {
                        if let Some(tc_text) = extract_agy_tool_call_text(calls) {
                            it.msg.text = truncate_utf8(&tc_text, MAX_MESSAGE_BYTES);
                        }
                    }
                }
            }
        }
    }

    (items.into_iter().map(|it| it.msg).collect(), total_parsed)
}

fn clean_agy_user_text(raw: &str) -> Option<String> {
    let mut t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with("<SYSTEM_MESSAGE>")
        || t.starts_with("<CONTEXT_SUMMARY>")
        || t.starts_with("The following is a <SYSTEM_MESSAGE>")
    {
        return None;
    }
    if let Some(start) = t.find("<USER_REQUEST>") {
        let after = &t[start + "<USER_REQUEST>".len()..];
        t = match after.find("</USER_REQUEST>") {
            Some(end) => after[..end].trim(),
            None => after.trim(),
        };
    }
    clean_user_text(t)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn strip_xml_tag_block(input: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut s = input.to_string();
    while let Some(start) = s.find(&open) {
        if let Some(rel_end) = s[start + open.len()..].find(&close) {
            let end = start + open.len() + rel_end + close.len();
            s.replace_range(start..end, "");
        } else {
            s.truncate(start);
            break;
        }
    }
    s
}

fn clean_user_text(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }
    for tag in [
        "local-command-caveat",
        "system-reminder",
        "task-notification",
        "local-command-stdout",
        "command-name",
        "command-message",
        "command-args",
        "guardian_tool_descriptions",
    ] {
        if s.contains(&format!("<{tag}>")) {
            s = strip_xml_tag_block(&s, tag);
        }
    }
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    for sys_prefix in [
        "<local-command-caveat>",
        "<system-reminder>",
        "<task-notification>",
        "<local-command-stdout>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<guardian_tool_descriptions>",
    ] {
        if t.starts_with(sys_prefix) {
            return None;
        }
    }
    Some(t.to_string())
}

/// Strip internal Codex memory citation XML blocks (`<oai-mem-citation>...</oai-mem-citation>`)
/// and collapse excess blank lines left behind.
pub(crate) fn strip_codex_internal_tags(raw: &str) -> String {
    let s = strip_xml_tag_block(raw, "oai-mem-citation");
    // Collapse 3+ consecutive newlines down to 2 and trim surrounding whitespace.
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.trim().lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line.trim_end());
        }
    }
    out.trim().to_string()
}

fn normalize_ws_for_dedup(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flush_pending_tools(out: &mut Vec<TranscriptMessage>, pending_tools: &mut Vec<String>) {
    if pending_tools.is_empty() {
        return;
    }
    let attached = std::mem::take(pending_tools);
    let ts_ms = out.last().and_then(|m| m.ts_ms);
    out.push(TranscriptMessage {
        role: TranscriptRole::Assistant,
        text: String::new(),
        ts_ms,
        tools: attached,
    });
}

fn push_message(out: &mut Vec<TranscriptMessage>, msg: TranscriptMessage) {
    // Avoid consecutive duplicates (e.g. Codex emitting both response_item and event_msg,
    // even when whitespace or trailing blank lines differ after stripping internal tags),
    // without suppressing genuinely separate identical User prompts (> 2s apart).
    if let Some(last) = out.last_mut() {
        let same_text = last.role == msg.role
            && (last.text == msg.text
                || normalize_ws_for_dedup(&last.text) == normalize_ws_for_dedup(&msg.text));
        let allow_dedup = match msg.role {
            TranscriptRole::Assistant => true,
            TranscriptRole::User => match (last.ts_ms, msg.ts_ms) {
                (Some(a), Some(b)) => a.abs_diff(b) <= 2_000,
                _ => true,
            },
        };
        if same_text && allow_dedup {
            merge_tools(&mut last.tools, &msg.tools);
            if last.ts_ms.is_none() {
                last.ts_ms = msg.ts_ms;
            }
            return;
        }
    }
    out.push(msg);
}

fn push_unique(list: &mut Vec<String>, item: &str) {
    let item = item.trim();
    if item.is_empty() {
        return;
    }
    if !list.iter().any(|s| s == item) {
        list.push(item.to_string());
    }
}

fn merge_tools(dst: &mut Vec<String>, src: &[String]) {
    for s in src {
        push_unique(dst, s);
    }
}

/// Truncate `s` to at most `max_bytes` on a valid UTF-8 character boundary.
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let suffix = "\n…(truncated)";
    let budget = max_bytes.saturating_sub(suffix.len());
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(suffix);
    out
}

/// Parse RFC-3339 / ISO-8601 timestamps like `"2026-11-21T04:18:51.474Z"` or
/// `"2026-11-21T12:18:51+08:00"` into epoch milliseconds without external crates.
pub(crate) fn parse_iso8601_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut ms: i64 = 0;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits_len = after_dot.bytes().take_while(|c| c.is_ascii_digit()).count();
        let frac = &after_dot[..digits_len];
        ms = match frac.len() {
            0 => 0,
            1 => frac.parse::<i64>().ok()? * 100,
            2 => frac.parse::<i64>().ok()? * 10,
            _ => frac[..3].parse::<i64>().ok()?,
        };
        rest = &after_dot[digits_len..];
    }

    let tz_offset_secs: i64 = if rest.is_empty() || rest == "Z" || rest == "z" {
        0
    } else if rest.len() >= 6 && (rest.starts_with('+') || rest.starts_with('-')) {
        let sign = if rest.starts_with('+') { 1 } else { -1 };
        let tzh: i64 = rest[1..3].parse().ok()?;
        let tzm: i64 = rest[4..6].parse().ok()?;
        sign * (tzh * 3600 + tzm * 60)
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - tz_offset_secs;
    if secs < 0 {
        return None;
    }
    Some((secs as u64) * 1000 + (ms as u64))
}

/// Howard Hinnant's civil-to-days algorithm (days since 1970-01-01).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + (day as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601_utc_and_offset() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2026-11-21T04:18:51.474Z"),
            Some(1_795_234_731_474)
        );
        assert_eq!(
            parse_iso8601_ms("2026-11-21T12:18:51.474+08:00"),
            Some(1_795_234_731_474)
        );
        assert_eq!(parse_iso8601_ms("not-a-date"), None);
    }

    #[test]
    fn parses_claude_jsonl_merges_chunks_extracts_tools_and_skips_bad_lines() {
        let jsonl = r####"
not valid json at all
{"type":"last-prompt","lastPrompt":"ignore me"}
{"type":"user","timestamp":"2026-11-20T08:00:00.000Z","message":{"role":"user","content":"请帮我排查编译错误"}}
{"type":"assistant","timestamp":"2026-11-20T08:00:02.000Z","message":{"id":"msg_1","role":"assistant","content":[{"type":"thinking","thinking":"secret thoughts"}]}}
{"type":"assistant","timestamp":"2026-11-20T08:00:03.000Z","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","name":"Read"},{"type":"tool_use","name":"Bash"}]}}
{"type":"user","timestamp":"2026-11-20T08:00:04.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"assistant","timestamp":"2026-11-20T08:00:05.000Z","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"### 结论\n\n已修复 `foo.rs` 缺失字段。"}]}}
{"type":"assistant","timestamp":"2026-11-20T08:00:06.000Z","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"单测全部通过。"}]}}
"####;
        let msgs = parse_transcript_for_agent(AgentKind::Claude, jsonl, 12);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[0].text, "请帮我排查编译错误");
        assert!(msgs[0].ts_ms.is_some());

        assert_eq!(msgs[1].role, TranscriptRole::Assistant);
        assert!(msgs[1].text.contains("### 结论"));
        assert!(msgs[1].text.contains("单测全部通过。"));
        assert!(!msgs[1].text.contains("secret thoughts"));
        assert_eq!(msgs[1].tools, vec!["Read", "Bash"]);
    }

    #[test]
    fn parses_codex_jsonl_extracts_messages_tools_and_dedups_event_msg() {
        let jsonl = r#"
{"type":"session_meta","timestamp":"2026-11-21T04:10:00.000Z","payload":{"id":"s1"}}
{"type":"response_item","timestamp":"2026-11-21T04:10:01.000Z","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system rules"}]}}
{"type":"response_item","timestamp":"2026-11-21T04:10:02.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd=/tmp</environment_context>"}]}}
{"type":"response_item","timestamp":"2026-11-21T04:10:03.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"检查一下服务端口"}]}}
{"type":"event_msg","timestamp":"2026-11-21T04:10:03.100Z","payload":{"type":"user_message","message":"检查一下服务端口"}}
{"type":"response_item","timestamp":"2026-11-21T04:10:04.000Z","payload":{"type":"custom_tool_call","name":"exec","call_id":"c1"}}
{"type":"response_item","timestamp":"2026-11-21T04:10:05.000Z","payload":{"type":"reasoning","summary":[],"encrypted_content":"abc"}}
{"type":"response_item","timestamp":"2026-11-21T04:10:06.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"端口 **7878** 正在监听。"}]}}
{"type":"event_msg","timestamp":"2026-11-21T04:10:06.100Z","payload":{"type":"task_complete","last_agent_message":"端口 **7878** 正在监听。"}}
"#;
        let msgs = parse_transcript_for_agent(AgentKind::Codex, jsonl, 12);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[0].text, "检查一下服务端口");
        assert_eq!(msgs[1].role, TranscriptRole::Assistant);
        assert_eq!(msgs[1].text, "端口 **7878** 正在监听。");
        assert_eq!(msgs[1].tools, vec!["exec"]);
    }

    #[test]
    fn parses_agy_jsonl_strips_user_request_skips_thinking_and_system() {
        let jsonl = r####"
{"step_index":1,"source":"SYSTEM","type":"SYSTEM_MESSAGE","created_at":"2026-11-20T12:00:00Z","content":"<SYSTEM_MESSAGE>hidden</SYSTEM_MESSAGE>"}
{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-20T12:00:01Z","content":"<USER_REQUEST>\n总结一下改动\n</USER_REQUEST>"}
{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-20T12:00:02Z","thinking":"internal chain of thought","tool_calls":[{"name":"read_file"},{"name":"run_command"}]}
{"step_index":4,"source":"MODEL","type":"GENERIC","created_at":"2026-11-20T12:00:03Z","content":"tool output"}
{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-20T12:00:05Z","thinking":"more thinking","content":"## 改动总结\n\n- 新增了 transcript 接口"}
"####;
        let msgs = parse_transcript_for_agent(AgentKind::Agy, jsonl, 12);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[0].text, "总结一下改动");
        assert_eq!(msgs[1].role, TranscriptRole::Assistant);
        assert_eq!(msgs[1].text, "## 改动总结\n\n- 新增了 transcript 接口");
        assert_eq!(msgs[1].tools, vec!["read_file", "run_command"]);
    }

    #[test]
    fn reads_tail_of_large_file_safely_and_truncates_oversized_messages() {
        let dir = std::env::temp_dir().join(format!("baton-tr-{}", baton_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.jsonl");
        let mut content = String::new();
        // Write > 8.5 MiB of old lines so tail slicing (8 MiB) drops early lines + partial line.
        let filler = "x".repeat(2000);
        for i in 0..4500 {
            content.push_str(&format!(
                r#"{{"type":"user","timestamp":"2026-11-20T00:00:00Z","message":{{"role":"user","content":"old-{i} {filler}"}}}}"#
            ));
            content.push('\n');
        }
        let huge = "中".repeat(25_000); // 75,000 bytes > 64 KiB
        content.push_str(&format!(
            r#"{{"type":"assistant","timestamp":"2026-11-20T01:00:00Z","message":{{"id":"m_last","role":"assistant","content":[{{"type":"text","text":"{huge}"}}]}}}}"#
        ));
        content.push('\n');
        std::fs::write(&file, &content).unwrap();

        let tail = read_tail_utf8(&file, MAX_TRANSCRIPT_READ_BYTES).unwrap();
        assert!(tail.len() <= MAX_TRANSCRIPT_READ_BYTES as usize);
        let msgs = parse_transcript_for_agent(AgentKind::Claude, &tail, 5);
        assert_eq!(msgs.len(), 5);
        let last = msgs.last().unwrap();
        assert_eq!(last.role, TranscriptRole::Assistant);
        assert!(last.text.len() <= MAX_MESSAGE_BYTES);
        assert!(last.text.ends_with("…(truncated)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_claude_and_agy_subagent_transcript_paths() {
        let home =
            std::env::temp_dir().join(format!("baton-subtr-{}", baton_proto::id::new_id("T")));
        let cwd = "/Users/demo/work/app-a";
        let enc = encode_claude_project_dir(cwd);
        let cl_sub = home
            .join(".claude/projects")
            .join(enc)
            .join("root-uuid/subagents/agent-a123.jsonl");
        std::fs::create_dir_all(cl_sub.parent().unwrap()).unwrap();
        std::fs::write(&cl_sub, "{}\n").unwrap();

        let cl_rec = SessionRecord {
            session_id: SessionId::new("root-uuid/a123"),
            agent: AgentKind::Claude,
            pane: None,
            pane_resolution: baton_proto::PaneResolution::Unknown,
            cwd: Some(cwd.into()),
            git_root: None,
            model: None,
            effort: None,
            state: baton_proto::SessionState::Idle,
            state_since_ms: 0,
            last_event_id: None,
            last_event_ms: 0,
            last_message: None,
            last_error: None,
            parent_session_id: Some(SessionId::new("root-uuid")),
            is_subagent: true,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: baton_proto::Confidence::Exact,
            tmux_target: None,
            tmux_label: None,
            discovered_only: false,
            transcript_path: None,
            metrics: None,
        };
        assert_eq!(resolve_transcript_path_in(&home, &cl_rec), Some(cl_sub));

        let ag_sub = home.join(
            ".gemini/antigravity-cli/brain/sub-conv-9/.system_generated/logs/transcript.jsonl",
        );
        std::fs::create_dir_all(ag_sub.parent().unwrap()).unwrap();
        std::fs::write(&ag_sub, "{}\n").unwrap();
        let mut ag_rec = cl_rec;
        ag_rec.agent = AgentKind::Agy;
        ag_rec.session_id = SessionId::new("sub-conv-9");
        ag_rec.discovered_only = true;
        assert_eq!(resolve_transcript_path_in(&home, &ag_rec), Some(ag_sub));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_strips_oai_mem_citation_and_dedups_adjacent_assistant_preserving_tools() {
        let jsonl = r#"
{"type":"response_item","timestamp":"2026-11-23T02:00:00.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"检查一下 ros 节点"}]}}
{"type":"response_item","timestamp":"2026-11-23T02:00:01.000Z","payload":{"type":"custom_tool_call","name":"exec_command","call_id":"c1"}}
{"type":"response_item","timestamp":"2026-11-23T02:00:02.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"已经确认节点正常启动，日志无报错。\n\n<oai-mem-citation>\n<citation_entries>\nMEMORY.md:12-20|note=[ros launch]\n</citation_entries>\n<thread_ids>\n01a1fc4d\n</thread_ids>\n</oai-mem-citation>\n"}]}}
{"type":"event_msg","timestamp":"2026-11-23T02:00:02.100Z","payload":{"type":"agent_message","message":"已经确认节点正常启动，日志无报错。"}}
{"type":"event_msg","timestamp":"2026-11-23T02:00:02.200Z","payload":{"type":"task_complete","last_agent_message":"已经确认节点正常启动，日志无报错。"}}
"#;
        let msgs = parse_transcript_for_agent(AgentKind::Codex, jsonl, 12);
        assert_eq!(
            msgs.len(),
            2,
            "duplicate assistant card not merged: {msgs:?}"
        );
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[1].role, TranscriptRole::Assistant);
        assert_eq!(msgs[1].text, "已经确认节点正常启动，日志无报错。");
        assert!(!msgs[1].text.contains("oai-mem-citation"));
        assert!(!msgs[1].text.contains("citation_entries"));
        assert_eq!(msgs[1].tools, vec!["exec_command"]);
    }

    #[test]
    fn agy_hydrates_truncated_content_from_sibling_transcript_full_and_degrades_gracefully() {
        let dir = std::env::temp_dir().join(format!(
            "baton-agy-hydrate-{}",
            baton_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let compact_path = dir.join("transcript.jsonl");
        let full_path = dir.join("transcript_full.jsonl");

        let full_user = "请完整评审整个系统的架构设计与并发安全性，包含所有边界细节。";
        let full_reply = format!(
            "## 架构评审结论\n\n{}\n\n**最终结论**：全部模块均已通过验证，参考 [design.md](file:///repo/design.md)。",
            "详细分析段落内容，包含完整 Markdown 链接与列表。\n".repeat(40)
        );
        let compact_jsonl = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "step_index": 1,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "created_at": "2026-11-23T03:00:00Z",
                "content": "<USER_REQUEST>\n请完整评审...<truncated 120 bytes>\n</USER_REQUEST>",
                "truncated_fields": ["content"]
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-23T03:00:01Z",
                "tool_calls": [{"name": "read_file"}]
            }),
            serde_json::json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-23T03:00:05Z",
                "content": "## 架构评审结论\n\n<truncated 3704 bytes>\n\n**最终结论**：全部模块",
                "truncated_fields": ["content"]
            }),
        );
        let full_jsonl = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "step_index": 1,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "created_at": "2026-11-23T03:00:00Z",
                "content": format!("<USER_REQUEST>\n{full_user}\n</USER_REQUEST>")
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "GENERIC",
                "created_at": "2026-11-23T03:00:02Z",
                "content": "giant tool output ignored"
            }),
            serde_json::json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-23T03:00:05Z",
                "content": full_reply
            }),
        );
        std::fs::write(&compact_path, &compact_jsonl).unwrap();
        std::fs::write(&full_path, &full_jsonl).unwrap();

        // 1. With sibling transcript_full.jsonl present: 100% hydrated, zero `<truncated ` remnants!
        let msgs = parse_agy_jsonl_with_hydration(&compact_jsonl, Some(&compact_path), 24);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[0].text, full_user);
        assert!(!msgs[0].text.contains("<truncated "));
        assert_eq!(msgs[1].role, TranscriptRole::Assistant);
        assert_eq!(msgs[1].text, full_reply);
        assert!(!msgs[1].text.contains("<truncated "));
        assert_eq!(msgs[1].tools, vec!["read_file"]);

        // 2. When sibling transcript_full.jsonl is missing: degrades gracefully without panicking.
        std::fs::remove_file(&full_path).unwrap();
        let degraded = parse_agy_jsonl_with_hydration(&compact_jsonl, Some(&compact_path), 24);
        assert_eq!(degraded.len(), 2);
        assert!(degraded[1].text.contains("<truncated 3704 bytes>"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_giant_compacted_line_in_tail_and_keeps_adjacent_user_turns_and_tool_calls() {
        let dir =
            std::env::temp_dir().join(format!("baton-giant-tail-{}", baton_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("giant.jsonl");
        let giant_blob = "z".repeat(3 * 1024 * 1024);
        let content = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-23T01:00:00Z",
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "<system-reminder>skip me</system-reminder>"},
                    {"type": "text", "text": "continue"}
                ]}
            }),
            serde_json::json!({
                "type": "compacted",
                "data": giant_blob
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-23T01:05:00Z",
                "message": {"role": "user", "content": "continue"}
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-23T01:06:00Z",
                "message": {"role": "user", "content": [{"type": "image", "source": {}}]}
            }),
        );
        std::fs::write(&file, &content).unwrap();
        let tail = read_tail_utf8(&file, MAX_TRANSCRIPT_READ_BYTES).unwrap();
        let msgs = parse_transcript_for_agent(AgentKind::Claude, &tail, 10);
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        assert_eq!(msgs[0].text, "continue");
        assert_eq!(msgs[1].text, "continue");
        assert_eq!(msgs[2].text, "[图片]");

        // AGY tool_calls hydration + inter-agent [Message] + ERROR_MESSAGE
        let agy_dir = dir.join("agy");
        std::fs::create_dir_all(&agy_dir).unwrap();
        let agy_compact = agy_dir.join("transcript.jsonl");
        let agy_full = agy_dir.join("transcript_full.jsonl");
        let compact_text = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "step_index": 0,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "created_at": "2026-11-23T02:00:00Z",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-23T02:00:00Z sender=worker-abc123 priority=HIGH content=done verifying\n</SYSTEM_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-23T02:00:05Z",
                "tool_calls": [{"name": "exit_plan_mode", "args": {"plan": "\"# Preview...<truncated 50 bytes>"}}],
                "truncated_fields": ["tool_calls"]
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "SYSTEM",
                "type": "ERROR_MESSAGE",
                "created_at": "2026-11-23T02:00:10Z",
                "error": "HTTP 400 failed"
            }),
        );
        let full_text = format!(
            "{}\n",
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-23T02:00:05Z",
                "tool_calls": [{"name": "exit_plan_mode", "args": {"plan": "\"# Full Hydrated Plan\""}}]
            })
        );
        std::fs::write(&agy_compact, &compact_text).unwrap();
        std::fs::write(&agy_full, &full_text).unwrap();
        let agy_msgs = parse_agy_jsonl_with_hydration(&compact_text, Some(&agy_compact), 10);
        assert_eq!(agy_msgs.len(), 3, "{agy_msgs:?}");
        assert!(agy_msgs[0].text.contains("done verifying"));
        assert_eq!(agy_msgs[1].text, "# Full Hydrated Plan");
        assert!(agy_msgs[2].text.contains("HTTP 400 failed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_session_transcript_reports_has_more_and_total_messages() {
        let dir =
            std::env::temp_dir().join(format!("baton-tr-more-{}", baton_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("conv.jsonl");
        let mut content = String::new();
        for i in 1..=5 {
            content.push_str(&format!(
                r#"{{"type":"user","timestamp":"2026-11-24T00:00:0{i}Z","message":{{"role":"user","content":"q-{i}"}}}}"#
            ));
            content.push('\n');
        }
        std::fs::write(&file, &content).unwrap();

        let rec = SessionRecord {
            session_id: SessionId::new("tr-more-1"),
            agent: AgentKind::Claude,
            pane: None,
            pane_resolution: baton_proto::PaneResolution::Unknown,
            cwd: Some(dir.to_string_lossy().into_owned()),
            git_root: None,
            model: None,
            effort: None,
            state: baton_proto::SessionState::Idle,
            state_since_ms: 0,
            last_event_id: None,
            last_event_ms: 0,
            last_message: None,
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: baton_proto::Confidence::Exact,
            tmux_target: None,
            tmux_label: None,
            discovered_only: false,
            transcript_path: Some(file.to_string_lossy().into_owned()),
            metrics: None,
        };

        let partial = load_session_transcript(&rec, 2);
        assert_eq!(partial.messages.len(), 2);
        assert_eq!(partial.total_messages, 5);
        assert!(partial.has_more);
        assert_eq!(partial.messages[0].text, "q-4");
        assert_eq!(partial.messages[1].text, "q-5");

        let full = load_session_transcript(&rec, 10);
        assert_eq!(full.messages.len(), 5);
        assert_eq!(full.total_messages, 5);
        assert!(!full.has_more);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
