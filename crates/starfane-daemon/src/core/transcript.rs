//! Structured transcript JSONL reader for Claude Code, Codex CLI, and Antigravity (AGY).
//!
//! Reads at most the trailing 8 MiB of a session's transcript JSONL file, extracts the
//! last `limit` clean user/assistant messages (truncating each message at 64 KiB on a UTF-8
//! character boundary), hydrates truncated AGY entries from sibling `transcript_full.jsonl`
//! by `step_index`, and skips malformed lines without ever panicking.

use super::metrics::{
    encode_claude_project_dir, file_identity, merge_metrics, resolve_codex_transcript_by_id_in,
    TelemetryFold,
};
use super::{lock, Core};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use starfane_proto::{AgentKind, ApiError, SessionId, SessionMetrics, SessionRecord};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Hard cap on trailing bytes kept by legacy `read_tail_utf8` helper (8 MiB).
#[cfg(test)]
pub const MAX_TRANSCRIPT_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Hard cap on backward scan distance when skipping giant non-message JSONL lines (32 MiB).
#[cfg(test)]
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 32 * 1024 * 1024;
/// Backward read chunk size for `read_tail_utf8`.
#[cfg(test)]
const TAIL_CHUNK_BYTES: usize = 512 * 1024;
/// Hard cap on each individual message body (64 KiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Default number of messages returned by `GET /api/sessions/{id}/transcript`.
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 60;
/// Maximum allowed `limit` query value (safety fuse for `?all=1` / cursor paging).
pub const MAX_TRANSCRIPT_LIMIT: usize = 5_000;
/// Maximum response text payload budget per transcript page (16 MiB safety fuse).
pub const MAX_TRANSCRIPT_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Lines exceeding 1 MiB matching `GENERIC`/`compacted` (or >4 MiB any line) are skipped in O(1) RAM.
const STREAM_SKIP_GIANT_THRESHOLD: usize = 1024 * 1024;
const STREAM_HARD_LINE_LIMIT: usize = 4 * 1024 * 1024;
/// Leading bytes inspected once when a streaming line crosses `STREAM_SKIP_GIANT_THRESHOLD`
/// so top-level `"type":"GENERIC"` / `"type":"compacted"` checks never re-scan multi-MiB buffers.
const GIANT_LINE_HEADER_PEEK_BYTES: usize = 2048;
/// Default minimum in-memory indexed transcript files (LRU-evicted); dynamically expands via
/// `ensure_min_capacity` up to `MAX_DYNAMIC_CACHED_TRANSCRIPTS` when live session counts grow.
const MAX_CACHED_TRANSCRIPTS: usize = 512;
const MAX_DYNAMIC_CACHED_TRANSCRIPTS: usize = 2048;
/// Maximum cached paginated `TranscriptResponse` snapshots (LRU-evicted).
const MAX_CACHED_PAGES: usize = 256;
/// Soft memory ceiling across all cached response pages (32 MiB).
const MAX_PAGE_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Cursor pagination & full-load request parameters for `GET /api/sessions/{id}/transcript`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptQuery {
    pub limit: usize,
    pub before: Option<usize>,
    pub all: bool,
}

impl TranscriptQuery {
    pub fn tail(limit: usize) -> Self {
        Self {
            limit,
            before: None,
            all: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptRole {
    User,
    Assistant,
    System,
}

/// Semantic category for assistant turns so Web UI can collapse non-formal
/// intermediate outputs while keeping formal replies fully expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptMessageKind {
    /// Internal plan handoff (e.g. AGY `exit_plan_mode`).
    Plan,
    /// Inter-agent / swarm tool message (e.g. AGY `send_message` / `post_message`).
    AgentMessage,
    /// Mid-turn intermediate narration/commentary before final answer
    /// (Codex `commentary`, Claude `tool_use`, AGY `PLANNER_RESPONSE` with simultaneous `tool_calls`).
    Intermediate,
    /// Formal final reply to the user.
    Final,
}

/// Semantic category for assistant turns that delegated work to a subagent or
/// Swarm worker so Web UI can visually distinguish delegated handoffs from
/// completed stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationKind {
    Subagent,
    Swarm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TranscriptMessageKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationKind>,
}

/// Content-aware watermark for a transcript index, detecting in-place mutations
/// (synthetic trailing pending-tools replacement, Claude same-`msg_id` appends,
/// or late AGY `transcript_full.jsonl` hydration) even when `total_messages` stays flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptWatermark {
    pub total_messages: usize,
    pub last_turn_hash: u64,
    pub has_pending_tools: bool,
}

/// Compute a deterministic 64-bit FNV-1a hash over all semantic fields of `TranscriptMessage`.
pub fn hash_transcript_message(msg: &TranscriptMessage) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    #[inline]
    fn fnv_bytes(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h ^= u64::from(b);
            *h = h.wrapping_mul(FNV_PRIME);
        }
    }

    let mut h = FNV_OFFSET_BASIS;
    let role_tag = match msg.role {
        TranscriptRole::User => 1u8,
        TranscriptRole::Assistant => 2u8,
        TranscriptRole::System => 3u8,
    };
    fnv_bytes(&mut h, &[role_tag]);

    let kind_tag = match msg.kind {
        None => 0u8,
        Some(TranscriptMessageKind::Plan) => 1u8,
        Some(TranscriptMessageKind::AgentMessage) => 2u8,
        Some(TranscriptMessageKind::Intermediate) => 3u8,
        Some(TranscriptMessageKind::Final) => 4u8,
    };
    fnv_bytes(&mut h, &[kind_tag]);

    let del_tag = match msg.delegation {
        None => 0u8,
        Some(DelegationKind::Subagent) => 1u8,
        Some(DelegationKind::Swarm) => 2u8,
    };
    fnv_bytes(&mut h, &[del_tag]);

    match msg.ts_ms {
        None => fnv_bytes(&mut h, &[0u8]),
        Some(ts) => {
            fnv_bytes(&mut h, &[1u8]);
            fnv_bytes(&mut h, &ts.to_le_bytes());
        }
    }

    fnv_bytes(&mut h, &(msg.tools.len() as u64).to_le_bytes());
    for tool in &msg.tools {
        fnv_bytes(&mut h, &(tool.len() as u64).to_le_bytes());
        fnv_bytes(&mut h, tool.as_bytes());
    }

    fnv_bytes(&mut h, &(msg.text.len() as u64).to_le_bytes());
    fnv_bytes(&mut h, msg.text.as_bytes());
    h
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
    /// 0-based index of `messages[0]` within the full transcript.
    #[serde(default)]
    pub start_seq: usize,
    /// Next cursor to pass as `before=<next_before>` to fetch older messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<usize>,
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
            start_seq: 0,
            next_before: None,
            metrics,
        }
    }
}

// ---------------------------------------------------------------------------
// Append-only `TranscriptIndex` + sparse `AgyFullIndex` + single-pass fold
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct FullStepSpan {
    offset: u64,
    len: usize,
}

#[derive(Debug, Clone)]
struct AgyFullIndex {
    dev: u64,
    ino: u64,
    mtime_ns: u128,
    file_len: u64,
    scanned_offset: u64,
    spans: HashMap<u64, FullStepSpan>,
}

impl AgyFullIndex {
    fn new(dev: u64, ino: u64) -> Self {
        Self {
            dev,
            ino,
            mtime_ns: 0,
            file_len: 0,
            scanned_offset: 0,
            spans: HashMap::new(),
        }
    }

    fn sync(&mut self, full_path: &Path) {
        let Some((dev, ino, mtime_ns, file_len)) = file_identity(full_path) else {
            return;
        };
        if self.dev != dev
            || self.ino != ino
            || file_len < self.scanned_offset
            || (file_len == self.file_len && mtime_ns != self.mtime_ns)
        {
            *self = Self::new(dev, ino);
        } else if self.file_len == file_len && self.mtime_ns == mtime_ns {
            return;
        }
        let Ok(mut file) = File::open(full_path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.scanned_offset)).is_err() {
            return;
        }
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut offset = self.scanned_offset;
        let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            match read_bounded_jsonl_line(&mut reader, &mut line_buf, STREAM_HARD_LINE_LIMIT) {
                Ok(BoundedLine::Eof) => break,
                Ok(BoundedLine::Skipped {
                    bytes_consumed,
                    terminated,
                }) => {
                    if !terminated {
                        break;
                    }
                    // Even for giant lines, record step_index only if small enough for hydration.
                    offset = offset.saturating_add(bytes_consumed as u64);
                }
                Ok(BoundedLine::Line {
                    bytes_consumed,
                    terminated,
                }) => {
                    if !terminated && offset + (bytes_consumed as u64) == file_len {
                        // Allow valid trailing line without newline; otherwise wait for flush.
                        let s = String::from_utf8_lossy(&line_buf);
                        if serde_json::from_str::<Value>(s.trim()).is_err() {
                            break;
                        }
                    }
                    let line_str = String::from_utf8_lossy(&line_buf);
                    if let Some(step_idx) = fast_extract_step_index(&line_str) {
                        self.spans.insert(
                            step_idx,
                            FullStepSpan {
                                offset,
                                len: line_buf.len(),
                            },
                        );
                    }
                    offset = offset.saturating_add(bytes_consumed as u64);
                }
                Err(_) => break,
            }
        }
        self.scanned_offset = offset;
        self.mtime_ns = mtime_ns;
        self.file_len = file_len;
    }

    fn read_step(&self, full_path: &Path, step_idx: u64) -> Option<Value> {
        let span = self.spans.get(&step_idx)?;
        if span.len == 0 || span.len > STREAM_HARD_LINE_LIMIT {
            return None;
        }
        let mut file = File::open(full_path).ok()?;
        file.seek(SeekFrom::Start(span.offset)).ok()?;
        let mut buf = vec![0u8; span.len];
        file.read_exact(&mut buf).ok()?;
        let s = String::from_utf8_lossy(&buf);
        let obj = serde_json::from_str::<Value>(s.trim()).ok()?;
        (obj.get("step_index").and_then(Value::as_u64) == Some(step_idx)).then_some(obj)
    }
}

#[derive(Debug, Clone)]
struct IndexedTurn {
    msg: TranscriptMessage,
    step_index: Option<u64>,
    needs_content_hydration: bool,
    needs_tools_hydration: bool,
    hydrated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptIndex {
    agent: AgentKind,
    dev: u64,
    ino: u64,
    mtime_ns: u128,
    file_len: u64,
    scanned_offset: u64,
    turns: Vec<IndexedTurn>,
    last_claude_msg_id: Option<String>,
    pending_tools: Vec<String>,
    pending_delegation: Option<DelegationKind>,
    telemetry: TelemetryFold,
    agy_full: Option<AgyFullIndex>,
    cached_watermark: Option<((u64, u64, u128), TranscriptWatermark)>,
    #[cfg(test)]
    delta_bytes_scanned: u64,
}

enum BoundedLine {
    Eof,
    Line {
        bytes_consumed: usize,
        terminated: bool,
    },
    Skipped {
        bytes_consumed: usize,
        terminated: bool,
    },
}

#[inline]
fn has_skippable_giant_type_header(buf: &[u8]) -> bool {
    let head = &buf[..buf.len().min(GIANT_LINE_HEADER_PEEK_BYTES)];
    contains_subslice(head, b"\"type\":\"GENERIC\"")
        || contains_subslice(head, b"\"type\": \"GENERIC\"")
        || contains_subslice(head, b"\"type\":\"compacted\"")
        || contains_subslice(head, b"\"type\": \"compacted\"")
}

/// Stream a single `\n`-delimited JSONL line without pulling giant `GENERIC`/`compacted`
/// blobs (>1 MiB) or absurd >4 MiB lines onto the heap.
fn read_bounded_jsonl_line<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    hard_limit: usize,
) -> std::io::Result<BoundedLine> {
    out.clear();
    let mut consumed = 0usize;
    let mut skipping = false;
    let mut checked_giant_header = false;
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            return Ok(if consumed == 0 {
                BoundedLine::Eof
            } else if skipping {
                BoundedLine::Skipped {
                    bytes_consumed: consumed,
                    terminated: false,
                }
            } else {
                if out.ends_with(b"\r") {
                    out.pop();
                }
                BoundedLine::Line {
                    bytes_consumed: consumed,
                    terminated: false,
                }
            });
        }
        if let Some(nl_pos) = avail.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if !skipping {
                if out.len() + nl_pos > hard_limit {
                    skipping = true;
                    out.clear();
                } else {
                    out.extend_from_slice(&avail[..nl_pos]);
                    if out.ends_with(b"\r") {
                        out.pop();
                    }
                    if is_skippable_giant_line(out) {
                        skipping = true;
                        out.clear();
                    }
                }
            }
            reader.consume(take);
            consumed += take;
            return Ok(if skipping {
                BoundedLine::Skipped {
                    bytes_consumed: consumed,
                    terminated: true,
                }
            } else {
                BoundedLine::Line {
                    bytes_consumed: consumed,
                    terminated: true,
                }
            });
        } else {
            let chunk_len = avail.len();
            if !skipping {
                let next_len = out.len() + chunk_len;
                if next_len > hard_limit {
                    skipping = true;
                    out.clear();
                } else if next_len > STREAM_SKIP_GIANT_THRESHOLD && !checked_giant_header {
                    if out.len() < GIANT_LINE_HEADER_PEEK_BYTES {
                        let need = (GIANT_LINE_HEADER_PEEK_BYTES - out.len()).min(chunk_len);
                        out.extend_from_slice(&avail[..need]);
                        checked_giant_header = true;
                        if has_skippable_giant_type_header(out) {
                            skipping = true;
                            out.clear();
                        } else if need < chunk_len {
                            out.extend_from_slice(&avail[need..]);
                        }
                    } else {
                        checked_giant_header = true;
                        if has_skippable_giant_type_header(out) {
                            skipping = true;
                            out.clear();
                        } else {
                            out.extend_from_slice(avail);
                        }
                    }
                } else {
                    out.extend_from_slice(avail);
                }
            }
            reader.consume(chunk_len);
            consumed += chunk_len;
        }
    }
}

impl TranscriptIndex {
    pub(crate) fn new(agent: AgentKind, dev: u64, ino: u64) -> Self {
        Self {
            agent,
            dev,
            ino,
            mtime_ns: 0,
            file_len: 0,
            scanned_offset: 0,
            turns: Vec::new(),
            last_claude_msg_id: None,
            pending_tools: Vec::new(),
            pending_delegation: None,
            telemetry: TelemetryFold::new(agent),
            agy_full: None,
            cached_watermark: None,
            #[cfg(test)]
            delta_bytes_scanned: 0,
        }
    }

    /// Incrementally scan only newly appended bytes of `path` in O(delta), updating both
    /// `turns` and `telemetry` in a single JSONL pass.
    pub(crate) fn sync_file(&mut self, path: &Path) -> bool {
        let Some((dev, ino, mtime_ns, file_len)) = file_identity(path) else {
            return false;
        };
        if self.dev != dev
            || self.ino != ino
            || file_len < self.scanned_offset
            || (file_len == self.file_len && mtime_ns != self.mtime_ns)
        {
            *self = Self::new(self.agent, dev, ino);
        } else if self.file_len == file_len && self.mtime_ns == mtime_ns {
            #[cfg(test)]
            {
                self.delta_bytes_scanned = 0;
            }
            return true;
        }
        let Ok(mut file) = File::open(path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.scanned_offset)).is_err() {
            return false;
        }
        let start_offset = self.scanned_offset;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut offset = self.scanned_offset;
        let mut line_buf: Vec<u8> = Vec::with_capacity(8 * 1024);

        loop {
            match read_bounded_jsonl_line(&mut reader, &mut line_buf, STREAM_HARD_LINE_LIMIT) {
                Ok(BoundedLine::Eof) => break,
                Ok(BoundedLine::Skipped {
                    bytes_consumed,
                    terminated,
                }) => {
                    if !terminated {
                        break;
                    }
                    offset = offset.saturating_add(bytes_consumed as u64);
                }
                Ok(BoundedLine::Line {
                    bytes_consumed,
                    terminated,
                }) => {
                    let s = String::from_utf8_lossy(&line_buf);
                    let trimmed = s.trim();
                    if trimmed.starts_with('{') {
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(obj) => {
                                self.telemetry.feed_obj(&obj);
                                self.feed_turn_obj(&obj);
                                offset = offset.saturating_add(bytes_consumed as u64);
                            }
                            Err(_) => {
                                if terminated {
                                    // Malformed complete line: skip safely.
                                    offset = offset.saturating_add(bytes_consumed as u64);
                                } else {
                                    // Possibly half-written live trailing line: retry next sync.
                                    break;
                                }
                            }
                        }
                    } else if terminated {
                        offset = offset.saturating_add(bytes_consumed as u64);
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        #[cfg(test)]
        {
            self.delta_bytes_scanned = offset.saturating_sub(start_offset);
        }
        if offset != start_offset {
            self.cached_watermark = None;
        }
        self.scanned_offset = offset;
        self.mtime_ns = mtime_ns;
        self.file_len = file_len;
        true
    }

    fn feed_turn_obj(&mut self, obj: &Value) {
        match self.agent {
            AgentKind::Claude => self.feed_claude_turn(obj),
            AgentKind::Codex => self.feed_codex_turn(obj),
            AgentKind::Agy => self.feed_agy_turn(obj),
            AgentKind::Shell => {}
        }
    }

    fn push_std_turn(&mut self, msg: TranscriptMessage) {
        if let Some(last) = self.turns.last_mut() {
            let same_text = last.msg.role == msg.role
                && (last.msg.text == msg.text
                    || normalize_ws_for_dedup(&last.msg.text) == normalize_ws_for_dedup(&msg.text));
            let allow_dedup = match msg.role {
                TranscriptRole::Assistant => true,
                TranscriptRole::User | TranscriptRole::System => {
                    match (last.msg.ts_ms, msg.ts_ms) {
                        (Some(a), Some(b)) => a.abs_diff(b) <= 2_000,
                        _ => true,
                    }
                }
            };
            if same_text && allow_dedup {
                merge_tools(&mut last.msg.tools, &msg.tools);
                if last.msg.ts_ms.is_none() {
                    last.msg.ts_ms = msg.ts_ms;
                }
                if msg.kind.is_some()
                    && (last.msg.kind.is_none() || msg.kind == Some(TranscriptMessageKind::Final))
                {
                    last.msg.kind = msg.kind;
                }
                last.msg.delegation = merge_delegation(last.msg.delegation, msg.delegation);
                return;
            }
        }
        self.turns.push(IndexedTurn {
            msg,
            step_index: None,
            needs_content_hydration: false,
            needs_tools_hydration: false,
            hydrated: true,
        });
    }

    fn feed_claude_turn(&mut self, obj: &Value) {
        if obj.get("isMeta").and_then(Value::as_bool) == Some(true)
            || obj.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);

        match kind {
            "user" => {
                let Some(content) = obj.get("message").and_then(|m| m.get("content")) else {
                    return;
                };
                if let Some(clean) = extract_claude_user_text(content) {
                    self.last_claude_msg_id = None;
                    self.pending_tools.clear();
                    self.pending_delegation = None;
                    self.push_std_turn(TranscriptMessage {
                        role: TranscriptRole::User,
                        text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                        ts_ms,
                        tools: Vec::new(),
                        kind: None,
                        delegation: None,
                    });
                }
            }
            "assistant" => {
                let Some(msg) = obj.get("message") else {
                    return;
                };
                let msg_id = msg.get("id").and_then(Value::as_str).map(str::to_string);
                let stop_reason = msg.get("stop_reason").and_then(Value::as_str);
                let (body, tools, turn_delegation) =
                    extract_claude_assistant_content(msg.get("content"));
                let is_tool_turn = !tools.is_empty() || stop_reason == Some("tool_use");
                let is_final_turn =
                    matches!(stop_reason, Some("end_turn" | "stop_sequence")) && tools.is_empty();
                let chunk_kind = if is_tool_turn {
                    Some(TranscriptMessageKind::Intermediate)
                } else if is_final_turn {
                    Some(TranscriptMessageKind::Final)
                } else {
                    None
                };

                if let (Some(id), Some(prev_id)) = (&msg_id, &self.last_claude_msg_id) {
                    if id == prev_id {
                        if let Some(last) = self
                            .turns
                            .last_mut()
                            .filter(|t| t.msg.role == TranscriptRole::Assistant)
                        {
                            if !body.is_empty() {
                                if !last.msg.text.is_empty() {
                                    last.msg.text.push_str("\n\n");
                                }
                                last.msg.text.push_str(&body);
                                last.msg.text = truncate_utf8(&last.msg.text, MAX_MESSAGE_BYTES);
                            }
                            merge_tools(&mut last.msg.tools, &tools);
                            last.msg.delegation =
                                merge_delegation(last.msg.delegation, turn_delegation);
                            if turn_delegation.is_some() {
                                self.pending_delegation =
                                    merge_delegation(self.pending_delegation, turn_delegation);
                            }
                            if last.msg.ts_ms.is_none() {
                                last.msg.ts_ms = ts_ms;
                            }
                            if is_tool_turn {
                                last.msg.kind = Some(TranscriptMessageKind::Intermediate);
                            } else if is_final_turn
                                && last.msg.kind != Some(TranscriptMessageKind::Intermediate)
                            {
                                last.msg.kind = Some(TranscriptMessageKind::Final);
                            }
                            return;
                        }
                    }
                }

                merge_tools(&mut self.pending_tools, &tools);
                self.pending_delegation =
                    merge_delegation(self.pending_delegation, turn_delegation);
                if body.is_empty() {
                    return;
                }
                let attached = std::mem::take(&mut self.pending_tools);
                let delegation = self.pending_delegation.take();
                if turn_delegation.is_some() {
                    self.pending_delegation = turn_delegation;
                }
                self.push_std_turn(TranscriptMessage {
                    role: TranscriptRole::Assistant,
                    text: truncate_utf8(&body, MAX_MESSAGE_BYTES),
                    ts_ms,
                    tools: attached,
                    kind: chunk_kind,
                    delegation,
                });
                self.last_claude_msg_id = msg_id;
            }
            _ => {}
        }
    }

    fn feed_codex_turn(&mut self, obj: &Value) {
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(payload) = obj.get("payload") else {
            return;
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
                    _ => return,
                };
                let (body, has_image) = extract_codex_content(payload.get("content"));
                if role == TranscriptRole::User {
                    self.pending_tools.clear();
                    self.pending_delegation = None;
                    if let Some(clean) = clean_codex_user_text(&body, has_image) {
                        self.push_std_turn(TranscriptMessage {
                            role,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: Vec::new(),
                            kind: None,
                            delegation: None,
                        });
                    }
                } else {
                    let clean = strip_codex_internal_tags(&body);
                    if clean.is_empty() {
                        return;
                    }
                    let attached = std::mem::take(&mut self.pending_tools);
                    let delegation = self.pending_delegation.take();
                    self.push_std_turn(TranscriptMessage {
                        role,
                        text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                        ts_ms,
                        tools: attached,
                        kind: codex_phase_kind(payload),
                        delegation,
                    });
                }
            }
            ("response_item", "custom_tool_call" | "function_call") => {
                if let Some(name) = payload.get("name").and_then(Value::as_str) {
                    push_unique(&mut self.pending_tools, name);
                    self.pending_delegation =
                        merge_delegation(self.pending_delegation, classify_codex_tool_name(name));
                }
            }
            ("event_msg", "user_message") => {
                self.pending_tools.clear();
                self.pending_delegation = None;
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
                    self.push_std_turn(TranscriptMessage {
                        role: TranscriptRole::User,
                        text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                        ts_ms,
                        tools: Vec::new(),
                        kind: None,
                        delegation: None,
                    });
                }
            }
            ("event_msg", "agent_message") => {
                if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                    let clean = strip_codex_internal_tags(msg);
                    if !clean.is_empty() {
                        let attached = std::mem::take(&mut self.pending_tools);
                        let delegation = self.pending_delegation.take();
                        self.push_std_turn(TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                            kind: codex_phase_kind(payload),
                            delegation,
                        });
                    }
                }
            }
            ("event_msg", "task_complete") => {
                if let Some(msg) = payload.get("last_agent_message").and_then(Value::as_str) {
                    let clean = strip_codex_internal_tags(msg);
                    if !clean.is_empty() {
                        let attached = std::mem::take(&mut self.pending_tools);
                        let delegation = self.pending_delegation.take();
                        self.push_std_turn(TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                            kind: Some(TranscriptMessageKind::Final),
                            delegation,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn push_agy_turn(&mut self, item: IndexedTurn) {
        if let Some(last) = self.turns.last_mut() {
            let same_text = last.msg.role == item.msg.role
                && (last.msg.text == item.msg.text
                    || normalize_ws_for_dedup(&last.msg.text)
                        == normalize_ws_for_dedup(&item.msg.text));
            let allow_dedup = match item.msg.role {
                TranscriptRole::Assistant => true,
                TranscriptRole::User | TranscriptRole::System => {
                    item.step_index.is_some() && last.step_index == item.step_index
                }
            };
            if same_text && allow_dedup {
                merge_tools(&mut last.msg.tools, &item.msg.tools);
                if last.msg.ts_ms.is_none() {
                    last.msg.ts_ms = item.msg.ts_ms;
                }
                if item.msg.kind.is_some()
                    && (last.msg.kind.is_none()
                        || item.msg.kind == Some(TranscriptMessageKind::Final))
                {
                    last.msg.kind = item.msg.kind;
                }
                last.msg.delegation = merge_delegation(last.msg.delegation, item.msg.delegation);
                return;
            }
        }
        self.turns.push(item);
    }

    fn feed_agy_turn(&mut self, obj: &Value) {
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
                    return;
                }
                self.pending_tools.clear();
                self.pending_delegation = None;
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
                        && agy_needs_content_hydration(obj, raw);
                    self.push_agy_turn(IndexedTurn {
                        msg: TranscriptMessage {
                            role: TranscriptRole::User,
                            text: truncate_utf8(&clean, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: Vec::new(),
                            kind: None,
                            delegation: None,
                        },
                        step_index,
                        needs_content_hydration,
                        needs_tools_hydration: false,
                        hydrated: !needs_content_hydration,
                    });
                }
            }
            "PLANNER_RESPONSE" => {
                let calls = obj.get("tool_calls").and_then(Value::as_array);
                let has_own_calls = calls.is_some_and(|c| !c.is_empty());
                let mut step_delegation = None;
                let mut step_has_done_post = false;
                if let Some(calls) = calls {
                    for tc in calls {
                        if let Some(name) = tc.get("name").and_then(Value::as_str) {
                            push_unique(&mut self.pending_tools, name);
                            if name == "post_message"
                                && tc
                                    .get("args")
                                    .and_then(|a| a.get("text"))
                                    .and_then(agy_arg_str)
                                    .is_some_and(|txt| agy_post_message_is_done(&txt))
                            {
                                step_has_done_post = true;
                            }
                        }
                        step_delegation =
                            merge_delegation(step_delegation, classify_agy_tool_call(tc));
                    }
                    if step_has_done_post && step_delegation.is_none() {
                        self.pending_delegation = None;
                    } else {
                        self.pending_delegation =
                            merge_delegation(self.pending_delegation, step_delegation);
                    }
                }
                let content_opt = obj
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if let Some(content) = content_opt {
                    let attached = std::mem::take(&mut self.pending_tools);
                    let delegation = self.pending_delegation.take();
                    let needs_content_hydration =
                        step_index.is_some() && agy_needs_content_hydration(obj, content);
                    let turn_kind = if has_own_calls {
                        TranscriptMessageKind::Intermediate
                    } else {
                        TranscriptMessageKind::Final
                    };
                    self.push_agy_turn(IndexedTurn {
                        msg: TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: truncate_utf8(content, MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                            kind: Some(turn_kind),
                            delegation,
                        },
                        step_index,
                        needs_content_hydration,
                        needs_tools_hydration: false,
                        hydrated: !needs_content_hydration,
                    });
                } else if let Some(calls) = calls {
                    if let Some((tc_text, tc_kind)) = extract_agy_tool_call_text(calls) {
                        let attached = std::mem::take(&mut self.pending_tools);
                        let delegation = self.pending_delegation.take();
                        if tc_kind == TranscriptMessageKind::AgentMessage && delegation.is_some() {
                            // Keep delegation armed so the immediate user-facing explanation
                            // right after a collapsed `post_message("@worker ...")` / `send_message`
                            // card is also highlighted as delegated.
                            self.pending_delegation = delegation;
                        }
                        let needs_tools_hydration =
                            step_index.is_some() && agy_needs_tools_hydration(obj);
                        self.push_agy_turn(IndexedTurn {
                            msg: TranscriptMessage {
                                role: TranscriptRole::Assistant,
                                text: truncate_utf8(&tc_text, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: attached,
                                kind: Some(tc_kind),
                                delegation,
                            },
                            step_index,
                            needs_content_hydration: false,
                            needs_tools_hydration,
                            hydrated: !needs_tools_hydration,
                        });
                    }
                }
            }
            "SYSTEM_MESSAGE" => {
                if let Some(raw) = obj.get("content").and_then(Value::as_str) {
                    if let Some(msg_text) = extract_agy_interagent_message(raw) {
                        // A swarm / background-task wakeup starts a new post-wakeup turn so
                        // pre-wakeup `swarm_wait` / `swarm(add)` state never leaks onto final
                        // completion summaries after wakeup.
                        self.pending_tools.clear();
                        self.pending_delegation = None;
                        let needs_content_hydration =
                            step_index.is_some() && agy_needs_content_hydration(obj, raw);
                        self.push_agy_turn(IndexedTurn {
                            msg: TranscriptMessage {
                                role: TranscriptRole::System,
                                text: truncate_utf8(&msg_text, MAX_MESSAGE_BYTES),
                                ts_ms,
                                tools: Vec::new(),
                                kind: None,
                                delegation: None,
                            },
                            step_index,
                            needs_content_hydration,
                            needs_tools_hydration: false,
                            hydrated: !needs_content_hydration,
                        });
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
                    let attached = std::mem::take(&mut self.pending_tools);
                    self.pending_delegation = None;
                    self.turns.push(IndexedTurn {
                        msg: TranscriptMessage {
                            role: TranscriptRole::Assistant,
                            text: truncate_utf8(&format!("[错误] {err_text}"), MAX_MESSAGE_BYTES),
                            ts_ms,
                            tools: attached,
                            kind: None,
                            delegation: None,
                        },
                        step_index,
                        needs_content_hydration: false,
                        needs_tools_hydration: false,
                        hydrated: true,
                    });
                }
            }
            _ => {}
        }
    }

    fn hydrate_range(&mut self, compact_path: &Path, start: usize, end: usize) {
        if self.agent != AgentKind::Agy || start >= end {
            return;
        }
        let any_unhydrated = self.turns[start..end]
            .iter()
            .any(|t| !t.hydrated && (t.needs_content_hydration || t.needs_tools_hydration));
        if !any_unhydrated {
            return;
        }
        let Some(full_path) = sibling_transcript_full(Some(compact_path)) else {
            return;
        };
        let full_idx = self.agy_full.get_or_insert_with(|| AgyFullIndex::new(0, 0));
        full_idx.sync(&full_path);
        for it in &mut self.turns[start..end] {
            if it.hydrated {
                continue;
            }
            let Some(step_idx) = it.step_index else {
                it.hydrated = true;
                continue;
            };
            let Some(full_obj) = full_idx.read_step(&full_path, step_idx) else {
                continue;
            };
            if it.needs_content_hydration {
                if let Some(full_raw) = full_obj.get("content").and_then(Value::as_str) {
                    match it.msg.role {
                        TranscriptRole::System => {
                            if let Some(clean) = extract_agy_interagent_message(full_raw) {
                                it.msg.text = truncate_utf8(&clean, MAX_MESSAGE_BYTES);
                            }
                        }
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
                    if let Some((tc_text, tc_kind)) = extract_agy_tool_call_text(calls) {
                        it.msg.text = truncate_utf8(&tc_text, MAX_MESSAGE_BYTES);
                        it.msg.kind = Some(tc_kind);
                    }
                    let hydrated_del = calls.iter().fold(None, |acc, tc| {
                        merge_delegation(acc, classify_agy_tool_call(tc))
                    });
                    it.msg.delegation = merge_delegation(it.msg.delegation, hydrated_del);
                }
            }
            it.hydrated = true;
            self.cached_watermark = None;
        }
    }

    fn trailing_pending_tools_message(&self) -> TranscriptMessage {
        TranscriptMessage {
            role: TranscriptRole::Assistant,
            text: String::new(),
            ts_ms: self.turns.last().and_then(|t| t.msg.ts_ms),
            tools: self.pending_tools.clone(),
            kind: None,
            delegation: self.pending_delegation,
        }
    }

    fn slice_page_with_parsed_metrics(
        &mut self,
        path: &Path,
        query: TranscriptQuery,
        session_metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> (TranscriptResponse, Option<SessionMetrics>) {
        let has_trailing_tools = !self.pending_tools.is_empty();
        let total = self.turns.len() + usize::from(has_trailing_tools);
        let end_excl = query.before.map(|b| b.min(total)).unwrap_or(total);
        let max_count = if query.all {
            MAX_TRANSCRIPT_LIMIT
        } else {
            query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT)
        };
        let min_start = end_excl.saturating_sub(max_count);

        // First hydrate turns in `[min_start..real_end)` so byte-budget accounting sees final sizes.
        let real_end = end_excl.min(self.turns.len());
        let real_min_start = min_start.min(real_end);
        self.hydrate_range(path, real_min_start, real_end);

        // Walk backwards applying the 16 MiB response safety fuse while keeping at least 1 message.
        let mut kept_rev: Vec<TranscriptMessage> = Vec::with_capacity(end_excl - min_start);
        let mut bytes_used = 0usize;
        let mut cur = end_excl;
        while cur > min_start {
            cur -= 1;
            let msg = if cur < self.turns.len() {
                self.turns[cur].msg.clone()
            } else {
                self.trailing_pending_tools_message()
            };
            let est_bytes = msg.text.len() + 64;
            if !kept_rev.is_empty() && bytes_used + est_bytes > MAX_TRANSCRIPT_RESPONSE_BYTES {
                cur += 1;
                break;
            }
            bytes_used += est_bytes;
            kept_rev.push(msg);
        }
        kept_rev.reverse();
        let start_seq = cur;
        let has_more = start_seq > 0;

        let (parsed_metrics, _, _) = self.telemetry.snapshot(now_ms);
        let metrics = merge_transcript_metrics(session_metrics, parsed_metrics.clone());

        (
            TranscriptResponse {
                source: "transcript".to_string(),
                transcript_path: Some(path.to_string_lossy().into_owned()),
                messages: kept_rev,
                has_more,
                total_messages: total,
                start_seq,
                next_before: has_more.then_some(start_seq),
                metrics,
            },
            parsed_metrics,
        )
    }

    pub fn total_messages(&self) -> usize {
        self.turns.len() + usize::from(!self.pending_tools.is_empty())
    }

    /// Return the content hash of message at 0-based `idx`, hydrating AGY turns first if needed,
    /// or hashing the synthetic trailing pending-tools message when `idx == self.turns.len()`.
    pub(crate) fn turn_hash_at(&mut self, path: &Path, idx: usize) -> Option<u64> {
        if idx < self.turns.len() {
            if self.agent == AgentKind::Agy {
                self.hydrate_range(path, idx, idx + 1);
            }
            return Some(hash_transcript_message(&self.turns[idx].msg));
        }
        if idx == self.turns.len() && !self.pending_tools.is_empty() {
            return Some(hash_transcript_message(
                &self.trailing_pending_tools_message(),
            ));
        }
        None
    }

    /// Compute the current `TranscriptWatermark` for this indexed transcript, returning the
    /// cached watermark in O(1) when neither `scanned_offset` nor any unhydrated AGY sibling
    /// `transcript_full.jsonl` file signature changed.
    pub(crate) fn watermark(&mut self, path: &Path) -> TranscriptWatermark {
        let last_needs_agy_full = self.agent == AgentKind::Agy
            && self.pending_tools.is_empty()
            && self.turns.last().is_some_and(|t| {
                !t.hydrated && (t.needs_content_hydration || t.needs_tools_hydration)
            });
        let (full_len, full_mtime_ns) = if last_needs_agy_full {
            sibling_transcript_full(Some(path))
                .and_then(|fp| file_identity(&fp))
                .map(|(_, _, fmt, flen)| (flen, fmt))
                .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let wm_key = (self.scanned_offset, full_len, full_mtime_ns);
        if let Some((cached_key, wm)) = self.cached_watermark {
            if cached_key == wm_key {
                return wm;
            }
        }
        let total_messages = self.total_messages();
        let has_pending_tools = !self.pending_tools.is_empty();
        let last_turn_hash = if total_messages > 0 {
            self.turn_hash_at(path, total_messages - 1).unwrap_or(0)
        } else {
            0
        };
        let post_needs_agy_full = self.agent == AgentKind::Agy
            && self.pending_tools.is_empty()
            && self.turns.last().is_some_and(|t| {
                !t.hydrated && (t.needs_content_hydration || t.needs_tools_hydration)
            });
        let final_key = if post_needs_agy_full {
            wm_key
        } else {
            (self.scanned_offset, 0, 0)
        };
        let wm = TranscriptWatermark {
            total_messages,
            last_turn_hash,
            has_pending_tools,
        };
        self.cached_watermark = Some((final_key, wm));
        wm
    }

    /// Return deduplicated AGY `step_index` values within the page selected by `query` that
    /// still need hydration from `transcript_full.jsonl` (after attempting hydration from any
    /// already-cached local sibling `transcript_full.jsonl`), prioritized newest-first and
    /// capped at `max_steps` (returned sorted ascending for deterministic fetching).
    pub fn missing_agy_hydration_steps(
        &mut self,
        compact_path: &Path,
        query: TranscriptQuery,
        max_steps: usize,
    ) -> Vec<usize> {
        if self.agent != AgentKind::Agy || max_steps == 0 {
            return Vec::new();
        }
        let total = self.total_messages();
        let end_excl = query.before.map(|b| b.min(total)).unwrap_or(total);
        let max_count = if query.all {
            MAX_TRANSCRIPT_LIMIT
        } else {
            query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT)
        };
        let min_start = end_excl.saturating_sub(max_count);
        let real_end = end_excl.min(self.turns.len());
        let real_min_start = min_start.min(real_end);

        // First hydrate from whatever is already present in local sibling `transcript_full.jsonl`.
        self.hydrate_range(compact_path, real_min_start, real_end);

        let mut missing: Vec<usize> = Vec::new();
        for it in self.turns[real_min_start..real_end].iter().rev() {
            if it.hydrated || (!it.needs_content_hydration && !it.needs_tools_hydration) {
                continue;
            }
            if let Some(step_idx) = it.step_index.and_then(|v| usize::try_from(v).ok()) {
                if !missing.contains(&step_idx) {
                    missing.push(step_idx);
                    if missing.len() >= max_steps {
                        break;
                    }
                }
            }
        }
        missing.sort_unstable();
        missing
    }

    pub(crate) fn slice_page(
        &mut self,
        path: &Path,
        query: TranscriptQuery,
        session_metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> TranscriptResponse {
        self.slice_page_with_parsed_metrics(path, query, session_metrics, now_ms)
            .0
    }

    pub(crate) fn telemetry_snapshot(
        &self,
        now_ms: u64,
    ) -> (Option<SessionMetrics>, Option<String>, Option<String>) {
        self.telemetry.snapshot(now_ms)
    }
}

fn merge_transcript_metrics(
    session_metrics: Option<SessionMetrics>,
    parsed_metrics: Option<SessionMetrics>,
) -> Option<SessionMetrics> {
    match (session_metrics, parsed_metrics) {
        (Some(mut agy), Some(parsed)) if agy.source == "agy_db" => {
            agy.context_window_tokens = agy.context_window_tokens.or(parsed.context_window_tokens);
            agy.context_used_tokens = agy.context_used_tokens.or(parsed.context_used_tokens);
            if let (Some(u), Some(w)) = (agy.context_used_tokens, agy.context_window_tokens) {
                let pct = ((u as f64 / w as f64) * 100.0).clamp(0.0, 100.0);
                if pct.is_finite() {
                    agy.context_used_pct = Some(((pct * 10.0).round() / 10.0) as f32);
                }
            } else if agy.context_used_pct.is_none() {
                agy.context_used_pct = parsed.context_used_pct;
            }
            agy.total_input_tokens = match (agy.total_input_tokens, parsed.total_input_tokens) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            agy.total_output_tokens = match (agy.total_output_tokens, parsed.total_output_tokens) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            if agy.tps.is_none() {
                agy.tps = parsed.tps;
                agy.last_output_tokens = agy.last_output_tokens.or(parsed.last_output_tokens);
                agy.last_duration_ms = agy.last_duration_ms.or(parsed.last_duration_ms);
            }
            if agy.recent_turns.is_empty() && !parsed.recent_turns.is_empty() {
                agy.recent_turns = parsed.recent_turns;
            }
            Some(agy)
        }
        (Some(agy), None) if agy.source == "agy_db" => Some(agy),
        (sess, parsed) => {
            let status_like = sess.filter(|m| m.source != "cc_switch");
            merge_metrics(parsed, status_like)
        }
    }
}

fn canonicalize_cache_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn estimate_response_bytes(resp: &TranscriptResponse) -> usize {
    let mut bytes = 128usize;
    for m in &resp.messages {
        bytes = bytes.saturating_add(m.text.len() + 64);
        for t in &m.tools {
            bytes = bytes.saturating_add(t.len() + 16);
        }
    }
    bytes
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PageCacheKey {
    path: PathBuf,
    agent: AgentKind,
    dev: u64,
    ino: u64,
    file_len: u64,
    mtime_ns: u128,
    full_file_len: u64,
    full_mtime_ns: u128,
    all: bool,
    limit: usize,
    before: Option<usize>,
}

#[derive(Debug, Clone)]
struct CachedIndex {
    index: TranscriptIndex,
    last_used: u64,
}

#[derive(Debug, Clone)]
struct CachedPage {
    response: TranscriptResponse,
    parsed_metrics: Option<SessionMetrics>,
    approx_bytes: usize,
    last_used: u64,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TranscriptCacheStats {
    page_hits: usize,
    page_misses: usize,
    index_evictions: usize,
    page_evictions: usize,
}

/// Bounded LRU in-memory cache of append-only `TranscriptIndex`es AND paginated
/// `TranscriptResponse` snapshots (lives outside `Mutex<State>`).
#[derive(Debug)]
pub struct TranscriptIndexCache {
    entries: HashMap<PathBuf, CachedIndex>,
    canonical_paths: HashMap<PathBuf, PathBuf>,
    pages: HashMap<PageCacheKey, CachedPage>,
    page_bytes: usize,
    access_seq: u64,
    max_indices: usize,
    max_pages: usize,
    max_page_bytes: usize,
    #[cfg(test)]
    stats: TranscriptCacheStats,
}

impl Default for TranscriptIndexCache {
    fn default() -> Self {
        Self::with_capacity(
            MAX_CACHED_TRANSCRIPTS,
            MAX_CACHED_PAGES,
            MAX_PAGE_CACHE_BYTES,
        )
    }
}

impl TranscriptIndexCache {
    pub(crate) fn with_capacity(
        max_indices: usize,
        max_pages: usize,
        max_page_bytes: usize,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            canonical_paths: HashMap::new(),
            pages: HashMap::new(),
            page_bytes: 0,
            access_seq: 0,
            max_indices: max_indices.max(1),
            max_pages: max_pages.max(1),
            max_page_bytes: max_page_bytes.max(1024),
            #[cfg(test)]
            stats: TranscriptCacheStats::default(),
        }
    }

    /// Ensure `max_indices` stays above the active working set of live sessions + subagents
    /// (bounded by `MAX_DYNAMIC_CACHED_TRANSCRIPTS`) so periodic sequential discovery scans
    /// never suffer cyclic Belady/LRU cache thrashing.
    pub(crate) fn ensure_min_capacity(&mut self, required_indices: usize) {
        let target = required_indices.clamp(1, MAX_DYNAMIC_CACHED_TRANSCRIPTS);
        if target > self.max_indices {
            self.max_indices = target;
        }
    }

    fn resolve_canonical_path(&mut self, path: &Path) -> PathBuf {
        if let Some(cached) = self.canonical_paths.get(path) {
            return cached.clone();
        }
        let canon = canonicalize_cache_path(path);
        let max_canon = self.max_indices.saturating_mul(2).max(128);
        if self.canonical_paths.len() >= max_canon {
            self.canonical_paths.clear();
        }
        self.canonical_paths
            .insert(path.to_path_buf(), canon.clone());
        canon
    }

    fn next_seq(&mut self) -> u64 {
        self.access_seq = self.access_seq.wrapping_add(1);
        self.access_seq
    }

    fn evict_lru_index_if_full(&mut self, incoming: &Path) {
        if self.entries.contains_key(incoming) || self.entries.len() < self.max_indices {
            return;
        }
        if let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&victim);
            self.purge_pages_for_path(&victim, None);
            #[cfg(test)]
            {
                self.stats.index_evictions += 1;
            }
        }
    }

    fn purge_pages_for_path(&mut self, path: &Path, keep_version: Option<(u64, u128, u64, u128)>) {
        let mut reclaimed = 0usize;
        self.pages.retain(|k, v| {
            let keep = k.path != path
                || keep_version.is_some_and(|(fl, mt, ffl, fmt)| {
                    k.file_len == fl
                        && k.mtime_ns == mt
                        && k.full_file_len == ffl
                        && k.full_mtime_ns == fmt
                });
            if !keep {
                reclaimed = reclaimed.saturating_add(v.approx_bytes);
            }
            keep
        });
        self.page_bytes = self.page_bytes.saturating_sub(reclaimed);
    }

    fn insert_page(
        &mut self,
        key: PageCacheKey,
        mut response: TranscriptResponse,
        parsed_metrics: Option<SessionMetrics>,
    ) {
        let approx_bytes = estimate_response_bytes(&response);
        if approx_bytes > self.max_page_bytes {
            return;
        }
        // Reclaim stale snapshots for the same file version before inserting.
        self.purge_pages_for_path(
            &key.path,
            Some((
                key.file_len,
                key.mtime_ns,
                key.full_file_len,
                key.full_mtime_ns,
            )),
        );
        if let Some(old) = self.pages.remove(&key) {
            self.page_bytes = self.page_bytes.saturating_sub(old.approx_bytes);
        }
        while !self.pages.is_empty()
            && (self.pages.len() >= self.max_pages
                || self.page_bytes.saturating_add(approx_bytes) > self.max_page_bytes)
        {
            let Some(victim) = self
                .pages
                .iter()
                .min_by_key(|(_, p)| p.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = self.pages.remove(&victim) {
                self.page_bytes = self.page_bytes.saturating_sub(removed.approx_bytes);
                #[cfg(test)]
                {
                    self.stats.page_evictions += 1;
                }
            }
        }
        response.metrics = None;
        let seq = self.next_seq();
        self.page_bytes = self.page_bytes.saturating_add(approx_bytes);
        self.pages.insert(
            key,
            CachedPage {
                response,
                parsed_metrics,
                approx_bytes,
                last_used: seq,
            },
        );
    }

    fn get_or_sync_canonical(
        &mut self,
        agent: AgentKind,
        key_path: &Path,
    ) -> Option<&mut TranscriptIndex> {
        self.evict_lru_index_if_full(key_path);
        let seq = self.next_seq();
        let entry = self
            .entries
            .entry(key_path.to_path_buf())
            .or_insert_with(|| CachedIndex {
                index: TranscriptIndex::new(agent, 0, 0),
                last_used: seq,
            });
        entry.last_used = seq;
        if entry.index.agent != agent {
            entry.index = TranscriptIndex::new(agent, 0, 0);
        }
        if !entry.index.sync_file(key_path) {
            return None;
        }
        Some(&mut entry.index)
    }

    pub(crate) fn get_or_sync(
        &mut self,
        agent: AgentKind,
        path: &Path,
    ) -> Option<&mut TranscriptIndex> {
        let mut key_path = self.resolve_canonical_path(path);
        if !key_path.is_file() {
            self.canonical_paths.remove(path);
            key_path = canonicalize_cache_path(path);
            if key_path.is_file() {
                self.canonical_paths
                    .insert(path.to_path_buf(), key_path.clone());
            }
        }
        self.get_or_sync_canonical(agent, &key_path)
    }

    /// Return a cached `TranscriptResponse` page in sub-millisecond time when
    /// `(canonical_path, dev, ino, file_len, mtime_ns, full_identity, query)` is unchanged;
    /// otherwise incrementally sync `TranscriptIndex` in O(delta), slice the page, and cache it.
    pub fn get_page_or_sync(
        &mut self,
        agent: AgentKind,
        path: &Path,
        query: TranscriptQuery,
        session_metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> Option<TranscriptResponse> {
        let mut key_path = self.resolve_canonical_path(path);
        let (dev, ino, mtime_ns, file_len) = match file_identity(&key_path) {
            Some(id) => id,
            None => {
                self.canonical_paths.remove(path);
                key_path = canonicalize_cache_path(path);
                let id = file_identity(&key_path)?;
                self.canonical_paths
                    .insert(path.to_path_buf(), key_path.clone());
                id
            }
        };
        let (full_file_len, full_mtime_ns) = if agent == AgentKind::Agy {
            sibling_transcript_full(Some(&key_path))
                .and_then(|fp| file_identity(&fp))
                .map(|(_, _, fmt, flen)| (flen, fmt))
                .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let norm_limit = if query.all {
            MAX_TRANSCRIPT_LIMIT
        } else {
            query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT)
        };
        let page_key = PageCacheKey {
            path: key_path.clone(),
            agent,
            dev,
            ino,
            file_len,
            mtime_ns,
            full_file_len,
            full_mtime_ns,
            all: query.all,
            limit: norm_limit,
            before: query.before,
        };

        if self.pages.contains_key(&page_key) {
            let seq = self.next_seq();
            if let Some(idx_entry) = self.entries.get_mut(&key_path) {
                idx_entry.last_used = seq;
            }
            if let Some(page) = self.pages.get_mut(&page_key) {
                page.last_used = seq;
                let mut resp = page.response.clone();
                resp.transcript_path = Some(path.to_string_lossy().into_owned());
                resp.metrics =
                    merge_transcript_metrics(session_metrics, page.parsed_metrics.clone());
                #[cfg(test)]
                {
                    self.stats.page_hits += 1;
                }
                return Some(resp);
            }
        }

        #[cfg(test)]
        {
            self.stats.page_misses += 1;
        }
        let idx = self.get_or_sync_canonical(agent, &key_path)?;
        let (mut resp, parsed_metrics) =
            idx.slice_page_with_parsed_metrics(&key_path, query, session_metrics, now_ms);
        resp.transcript_path = Some(path.to_string_lossy().into_owned());
        self.insert_page(page_key, resp.clone(), parsed_metrics);
        Some(resp)
    }

    pub fn sync_and_telemetry(
        &mut self,
        agent: AgentKind,
        path: &Path,
        now_ms: u64,
    ) -> (Option<SessionMetrics>, Option<String>, Option<String>) {
        let Some(idx) = self.get_or_sync(agent, path) else {
            return (None, None, None);
        };
        idx.telemetry_snapshot(now_ms)
    }

    /// Return the total indexed turn count for `path` (syncing any newly appended bytes first).
    pub fn total_messages_for_path(&mut self, agent: AgentKind, path: &Path) -> Option<usize> {
        let idx = self.get_or_sync(agent, path)?;
        Some(idx.total_messages())
    }

    /// Return the content-aware `TranscriptWatermark` for `path` (syncing newly appended bytes first).
    pub fn watermark_for_path(
        &mut self,
        agent: AgentKind,
        path: &Path,
    ) -> Option<TranscriptWatermark> {
        let key_path = self.resolve_canonical_path(path);
        let idx = self.get_or_sync(agent, path)?;
        Some(idx.watermark(&key_path))
    }

    /// Return the content hash of turn `idx` for `path` (hydrating AGY turns from sibling
    /// `transcript_full.jsonl` if needed).
    pub fn turn_hash_at_for_path(
        &mut self,
        agent: AgentKind,
        path: &Path,
        idx: usize,
    ) -> Option<u64> {
        let key_path = self.resolve_canonical_path(path);
        let index = self.get_or_sync(agent, path)?;
        index.turn_hash_at(&key_path, idx)
    }

    /// Return AGY `step_index` values in the slice selected by `query` that are not yet hydrated
    /// locally in sibling `transcript_full.jsonl`, capped at `max_steps`.
    pub fn missing_agy_hydration_steps(
        &mut self,
        agent: AgentKind,
        path: &Path,
        query: TranscriptQuery,
        max_steps: usize,
    ) -> Vec<usize> {
        if agent != AgentKind::Agy || max_steps == 0 {
            return Vec::new();
        }
        let key_path = self.resolve_canonical_path(path);
        let Some(idx) = self.get_or_sync(agent, path) else {
            return Vec::new();
        };
        idx.missing_agy_hydration_steps(&key_path, query, max_steps)
    }

    /// Purge cached response pages for `path` (preserving the parsed `TranscriptIndex` so
    /// incremental hydration does not need to re-scan `transcript.jsonl`).
    pub fn invalidate_pages(&mut self, path: &Path) {
        let key_path = self.resolve_canonical_path(path);
        self.purge_pages_for_path(&key_path, None);
    }

    /// Purge both cached response pages AND the `TranscriptIndex` entry for `path`
    /// (used after atomic prefix backfill or full mirror replacement).
    pub fn invalidate_path(&mut self, path: &Path) {
        let prev_canon = self.canonical_paths.remove(path);
        let key_path = canonicalize_cache_path(path);
        if let Some(prev) = prev_canon {
            if prev != key_path {
                self.entries.remove(&prev);
                self.purge_pages_for_path(&prev, None);
            }
        }
        self.entries.remove(&key_path);
        self.purge_pages_for_path(&key_path, None);
    }
}

impl Core {
    /// Read and paginate structured conversation messages for `sid` via the O(delta)
    /// `TranscriptIndexCache` + bounded LRU page cache (never blocks `Mutex<State>` on disk IO).
    pub async fn session_transcript(
        &self,
        sid: &SessionId,
        query: TranscriptQuery,
    ) -> Result<TranscriptResponse, ApiError> {
        let (session, claimed_by_other) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(sid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("session {sid}")))?;
            let stolen = s.discovered_only
                && !s.is_subagent
                && s.transcript_path
                    .as_deref()
                    .is_some_and(|tp| super::discover::is_transcript_claimed_by_other(&st, &s, tp));
            (s, stolen)
        };
        if session.agent.is_shell() || claimed_by_other {
            return Ok(TranscriptResponse::none(None));
        }
        if !session.is_local_host() {
            let hid = starfane_proto::HostId::new(session.effective_host_id());
            if let Some(provider) = self.inner.remote_nodes.get(&hid).cloned() {
                let remote_sid = sid
                    .as_str()
                    .strip_prefix(&format!("{}:", hid.as_str()))
                    .map(SessionId::new)
                    .unwrap_or_else(|| sid.clone());
                return match provider.fetch_transcript(&remote_sid, &query).await {
                    Ok(resp) => Ok(resp),
                    Err(e) if e.code == starfane_proto::ErrorCode::NotFound => {
                        Ok(TranscriptResponse::none(session.metrics.clone()))
                    }
                    Err(e) => Err(e),
                };
            }
            return Ok(TranscriptResponse::none(session.metrics.clone()));
        }
        let core = self.clone();
        let home_dir = self.effective_home_dir();
        tokio::task::spawn_blocking(move || {
            let Some(path) = resolve_transcript_path_with_home(home_dir.as_deref(), &session)
            else {
                return TranscriptResponse::none(session.metrics.clone());
            };
            if core.is_legacy_transcript_pre_birth(&session, &path) {
                return TranscriptResponse::none(None);
            }
            let now_ms = starfane_proto::id::now_ms();
            let mut tc = lock(&core.inner.transcript_index);
            tc.get_page_or_sync(session.agent, &path, query, session.metrics.clone(), now_ms)
                .unwrap_or_else(|| TranscriptResponse::none(session.metrics.clone()))
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))
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
/// and parse up to `limit` trailing messages using a single-pass `TranscriptIndex`.
pub(crate) fn load_session_transcript(session: &SessionRecord, limit: usize) -> TranscriptResponse {
    let Some(path) = resolve_transcript_path(session) else {
        return TranscriptResponse::none(session.metrics.clone());
    };
    let mut idx = TranscriptIndex::new(session.agent, 0, 0);
    if !idx.sync_file(&path) {
        return TranscriptResponse::none(session.metrics.clone());
    }
    let now_ms = starfane_proto::id::now_ms();
    idx.slice_page(
        &path,
        TranscriptQuery::tail(limit),
        session.metrics.clone(),
        now_ms,
    )
}

/// Locate the transcript JSONL file for `session` using strictly exact identity matches:
/// 1. Persisted `session.transcript_path` reported by hooks or exact PID/resume mapping.
/// 2. For non-legacy sessions (`!discovered_only`), deterministic exact path by `session_id`:
///    - AGY: `~/.gemini/antigravity-cli/brain/<session_id>/.system_generated/logs/transcript.jsonl`
///    - Claude: `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`
pub(crate) fn resolve_transcript_path(session: &SessionRecord) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_transcript_path_with_home(home.as_deref(), session)
}

pub(crate) fn resolve_transcript_path_with_home(
    home: Option<&Path>,
    session: &SessionRecord,
) -> Option<PathBuf> {
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

    resolve_transcript_path_in(home?, session)
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
        AgentKind::Shell => None,
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_skippable_giant_line(line: &[u8]) -> bool {
    line.len() > STREAM_SKIP_GIANT_THRESHOLD && has_skippable_giant_type_header(line)
}

/// Read complete trailing JSONL lines totaling at most `max_bytes`, scanning backwards in
/// 512 KiB chunks up to 16 MiB and skipping >1 MiB `compacted` / `GENERIC` lines so huge
/// snapshots or tool blobs never blank out recent turns.
#[cfg(test)]
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
    let mut idx = TranscriptIndex::new(agent, 0, 0);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
            idx.feed_turn_obj(&obj);
        }
    }
    let dummy = Path::new("");
    idx.slice_page(dummy, TranscriptQuery::tail(limit), None, 0)
        .messages
}

// ---------------------------------------------------------------------------
// Claude Code helpers
// ---------------------------------------------------------------------------

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

fn extract_claude_assistant_content(
    content: Option<&Value>,
) -> (String, Vec<String>, Option<DelegationKind>) {
    let Some(val) = content else {
        return (String::new(), Vec::new(), None);
    };
    match val {
        Value::String(s) => (s.trim().to_string(), Vec::new(), None),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            let mut tools = Vec::new();
            let mut delegation = None;
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
                            delegation =
                                merge_delegation(delegation, classify_claude_tool_name(name));
                        }
                    }
                    _ => {} // skip thinking / redacted_thinking / server_tool_use etc.
                }
            }
            (texts.join("\n\n"), tools, delegation)
        }
        _ => (String::new(), Vec::new(), None),
    }
}

fn merge_delegation(
    a: Option<DelegationKind>,
    b: Option<DelegationKind>,
) -> Option<DelegationKind> {
    match (a, b) {
        (Some(DelegationKind::Swarm), _) | (_, Some(DelegationKind::Swarm)) => {
            Some(DelegationKind::Swarm)
        }
        (Some(DelegationKind::Subagent), _) | (_, Some(DelegationKind::Subagent)) => {
            Some(DelegationKind::Subagent)
        }
        (None, None) => None,
    }
}

fn is_starfane_mcp_delegation_tool(name: &str) -> bool {
    let leaf = name
        .trim()
        .rsplit("__")
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or(name)
        .rsplit(':')
        .next()
        .unwrap_or(name);
    matches!(leaf, "spawn_session" | "create_handoff" | "start_pipeline")
}

fn classify_claude_tool_name(name: &str) -> Option<DelegationKind> {
    match name.trim() {
        "TeamCreate" => Some(DelegationKind::Swarm),
        "Agent" | "Task" | "SendMessage" => Some(DelegationKind::Subagent),
        other if is_starfane_mcp_delegation_tool(other) => Some(DelegationKind::Subagent),
        _ => None,
    }
}

fn classify_codex_tool_name(name: &str) -> Option<DelegationKind> {
    match name.trim() {
        "spawn_agent" | "send_input" | "wait" | "wait_agent" | "resume_agent" => {
            Some(DelegationKind::Subagent)
        }
        other if is_starfane_mcp_delegation_tool(other) => Some(DelegationKind::Subagent),
        _ => None,
    }
}

fn agy_post_message_is_done(text: &str) -> bool {
    let trimmed = text.trim_start();
    let rest = if trimmed.starts_with('@') {
        trimmed
            .split_once(char::is_whitespace)
            .map(|(_, after)| after.trim_start())
            .unwrap_or("")
    } else {
        trimmed
    };
    rest.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[done]"))
}

fn classify_agy_tool_call(tc: &Value) -> Option<DelegationKind> {
    let name = tc.get("name").and_then(Value::as_str)?.trim();
    match name {
        "swarm" => {
            let action = tc
                .get("args")
                .and_then(|a| a.get("action"))
                .and_then(agy_arg_str)?;
            (action == "add").then_some(DelegationKind::Swarm)
        }
        "swarm_wait" => Some(DelegationKind::Swarm),
        "post_message" => {
            let is_done = tc
                .get("args")
                .and_then(|a| a.get("text"))
                .and_then(agy_arg_str)
                .is_some_and(|txt| agy_post_message_is_done(&txt));
            (!is_done).then_some(DelegationKind::Swarm)
        }
        "invoke_subagent" | "run_workflow" | "send_message" => Some(DelegationKind::Subagent),
        other if is_starfane_mcp_delegation_tool(other) => Some(DelegationKind::Subagent),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Codex CLI helpers
// ---------------------------------------------------------------------------

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
// Antigravity (AGY) helpers
// ---------------------------------------------------------------------------

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

fn codex_phase_kind(payload: &Value) -> Option<TranscriptMessageKind> {
    match payload.get("phase").and_then(Value::as_str) {
        Some("commentary") => Some(TranscriptMessageKind::Intermediate),
        Some("final_answer") => Some(TranscriptMessageKind::Final),
        _ => None,
    }
}

fn extract_agy_tool_call_text(calls: &[Value]) -> Option<(String, TranscriptMessageKind)> {
    let mut parts: Vec<String> = Vec::new();
    let mut has_plan = false;
    for tc in calls {
        let name = tc.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(args) = tc.get("args") else {
            continue;
        };
        match name {
            "exit_plan_mode" => {
                if let Some(plan) = args.get("plan").and_then(agy_arg_str) {
                    has_plan = true;
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
        let kind = if has_plan {
            TranscriptMessageKind::Plan
        } else {
            TranscriptMessageKind::AgentMessage
        };
        Some((parts.join("\n\n"), kind))
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
    if let Some(task_id) = sender.rsplit('/').next().filter(|s| s.starts_with("task-")) {
        return Some(format!("[后台任务 {task_id}]\n{content}"));
    }
    if content.starts_with("Task id \"") {
        return Some(format!("[后台任务]\n{content}"));
    }
    if content.starts_with("[swarm]") {
        return Some(format!("[Swarm 通知]\n{content}"));
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
pub(crate) fn fast_extract_step_index(line: &str) -> Option<u64> {
    let pos = line.find("\"step_index\"")?;
    let rest = line[pos + "\"step_index\"".len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    rest[..digits_len].parse::<u64>().ok()
}

/// Parse compact AGY `transcript.jsonl`, keep the trailing `limit` messages, and hydrate any
/// truncated `content` or `tool_calls` fields from sibling `transcript_full.jsonl` by `step_index`.
#[cfg(test)]
pub(crate) fn parse_agy_jsonl_with_hydration(
    text: &str,
    transcript_path: Option<&Path>,
    limit: usize,
) -> Vec<TranscriptMessage> {
    let mut idx = TranscriptIndex::new(AgentKind::Agy, 0, 0);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
            idx.feed_turn_obj(&obj);
        }
    }
    let path = transcript_path.unwrap_or_else(|| Path::new(""));
    idx.slice_page(path, TranscriptQuery::tail(limit), None, 0)
        .messages
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
        let dir =
            std::env::temp_dir().join(format!("starfane-tr-{}", starfane_proto::id::new_id("T")));
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
        let home = std::env::temp_dir().join(format!(
            "starfane-subtr-{}",
            starfane_proto::id::new_id("T")
        ));
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
            pane_resolution: starfane_proto::PaneResolution::Unknown,
            cwd: Some(cwd.into()),
            git_root: None,
            model: None,
            effort: None,
            state: starfane_proto::SessionState::Idle,
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
            confidence: starfane_proto::Confidence::Exact,
            tmux_target: None,
            tmux_label: None,
            discovered_only: false,
            transcript_path: None,
            metrics: None,
            host_id: None,
            host_display_name: None,
            mux_kind: None,
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
            "starfane-agy-hydrate-{}",
            starfane_proto::id::new_id("T")
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
        let dir = std::env::temp_dir().join(format!(
            "starfane-giant-tail-{}",
            starfane_proto::id::new_id("T")
        ));
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
        assert_eq!(agy_msgs[0].role, TranscriptRole::System);
        assert!(agy_msgs[0].text.contains("done verifying"));
        assert_eq!(agy_msgs[1].role, TranscriptRole::Assistant);
        assert_eq!(agy_msgs[1].text, "# Full Hydrated Plan");
        assert_eq!(agy_msgs[1].kind, Some(TranscriptMessageKind::Plan));
        assert_eq!(agy_msgs[2].role, TranscriptRole::Assistant);
        assert!(agy_msgs[2].text.contains("HTTP 400 failed"));
        assert_eq!(agy_msgs[2].kind, None);

        // Task id background completion & [swarm] notifications stay System, only USER_INPUT is User.
        let sys_jsonl = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "step_index": 0,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "created_at": "2026-11-24T11:28:32Z",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-24T11:28:32Z sender=86fb4e44-53af-4234-977f-c96b9e91e3a1/task-2865 priority=HIGH content=Task id \"86fb4e44-53af-4234-977f-c96b9e91e3a1/task-2865\" finished with result: ok\n</SYSTEM_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 1,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "created_at": "2026-11-24T11:29:00Z",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-24T11:29:00Z sender=14036bd2-a40d-480a-97de-a307ce75577a priority=HIGH content=[swarm] new board message #154\n</SYSTEM_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "created_at": "2026-11-24T11:30:00Z",
                "content": "<USER_REQUEST>\n真正的用户提问\n</USER_REQUEST>"
            }),
        );
        let sys_msgs = parse_agy_jsonl_with_hydration(&sys_jsonl, None, 10);
        assert_eq!(sys_msgs.len(), 3, "{sys_msgs:?}");
        assert_eq!(sys_msgs[0].role, TranscriptRole::System);
        assert!(sys_msgs[0].text.starts_with("[后台任务 task-2865]\n"));
        assert_eq!(sys_msgs[1].role, TranscriptRole::System);
        assert!(sys_msgs[1].text.starts_with("[Swarm 通知]\n"));
        assert_eq!(sys_msgs[2].role, TranscriptRole::User);
        assert_eq!(sys_msgs[2].text, "真正的用户提问");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_session_transcript_reports_has_more_and_total_messages() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tr-more-{}",
            starfane_proto::id::new_id("T")
        ));
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
            pane_resolution: starfane_proto::PaneResolution::Unknown,
            cwd: Some(dir.to_string_lossy().into_owned()),
            git_root: None,
            model: None,
            effort: None,
            state: starfane_proto::SessionState::Idle,
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
            confidence: starfane_proto::Confidence::Exact,
            tmux_target: None,
            tmux_label: None,
            discovered_only: false,
            transcript_path: Some(file.to_string_lossy().into_owned()),
            metrics: None,
            host_id: None,
            host_display_name: None,
            mux_kind: None,
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
        assert_eq!(full.start_seq, 0);
        assert!(full.next_before.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_index_incremental_append_and_cursor_pagination_skip_giant_lines() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tr-idx-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.jsonl");
        let mut content = String::new();
        // Inject a >1.2 MiB giant compacted line right in the middle of history!
        for i in 0..45 {
            if i == 20 {
                let giant = "x".repeat(1_200_000);
                content.push_str(&format!(r#"{{"type":"compacted","blob":"{giant}"}}"#));
                content.push('\n');
            }
            content.push_str(&format!(
                r#"{{"type":"user","timestamp":"2026-11-24T00:00:{:02}Z","message":{{"role":"user","content":"turn-{i}"}}}}"#,
                i % 60
            ));
            content.push('\n');
        }
        std::fs::write(&file, &content).unwrap();

        let mut cache = TranscriptIndexCache::default();
        let idx = cache.get_or_sync(AgentKind::Claude, &file).unwrap();
        assert_eq!(idx.turns.len(), 45);
        assert!(idx.delta_bytes_scanned > 1_200_000);

        // Page 1: tail 20 -> turns 25..45
        let p1 = idx.slice_page(&file, TranscriptQuery::tail(20), None, 1000);
        assert_eq!(p1.total_messages, 45);
        assert_eq!(p1.messages.len(), 20);
        assert_eq!(p1.start_seq, 25);
        assert_eq!(p1.next_before, Some(25));
        assert!(p1.has_more);
        assert_eq!(p1.messages[0].text, "turn-25");
        assert_eq!(p1.messages[19].text, "turn-44");

        // Page 2: before=25, limit=20 -> turns 5..25
        let p2 = idx.slice_page(
            &file,
            TranscriptQuery {
                limit: 20,
                before: p1.next_before,
                all: false,
            },
            None,
            1000,
        );
        assert_eq!(p2.messages.len(), 20);
        assert_eq!(p2.start_seq, 5);
        assert_eq!(p2.next_before, Some(5));
        assert!(p2.has_more);
        assert_eq!(p2.messages[0].text, "turn-5");
        assert_eq!(p2.messages[19].text, "turn-24");

        // Page 3: before=5, limit=20 -> turns 0..5, reaches beginning!
        let p3 = idx.slice_page(
            &file,
            TranscriptQuery {
                limit: 20,
                before: p2.next_before,
                all: false,
            },
            None,
            1000,
        );
        assert_eq!(p3.messages.len(), 5);
        assert_eq!(p3.start_seq, 0);
        assert!(p3.next_before.is_none());
        assert!(!p3.has_more);
        assert_eq!(p3.messages[0].text, "turn-0");
        assert_eq!(p3.messages[4].text, "turn-4");

        // Append 2 new lines and verify O(delta) incremental scan!
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-11-24T00:01:01Z","message":{{"role":"user","content":"turn-45"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-11-24T00:01:02Z","message":{{"role":"user","content":"turn-46"}}}}"#
        )
        .unwrap();
        drop(f);

        let idx2 = cache.get_or_sync(AgentKind::Claude, &file).unwrap();
        assert_eq!(idx2.turns.len(), 47);
        assert!(
            idx2.delta_bytes_scanned < 500,
            "expected O(delta) scan, got {} bytes",
            idx2.delta_bytes_scanned
        );

        let all_resp = idx2.slice_page(
            &file,
            TranscriptQuery {
                limit: 60,
                before: None,
                all: true,
            },
            None,
            1000,
        );
        assert_eq!(all_resp.total_messages, 47);
        assert_eq!(all_resp.messages.len(), 47);
        assert!(!all_resp.has_more);
        assert_eq!(all_resp.messages[0].text, "turn-0");
        assert_eq!(all_resp.messages[46].text, "turn-46");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifies_plan_agent_intermediate_and_final_turns_across_agents() {
        // 1. AGY: silent tool step -> mid commentary+tools -> exit_plan_mode -> send_message -> formal final reply
        let agy_jsonl = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            serde_json::json!({
                "step_index": 0,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:00:01Z",
                "tool_calls": [{"name": "read_file", "args": {"Path": "/tmp/a.rs"}}]
            }),
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:00:02Z",
                "content": "Let me inspect the other module too.",
                "tool_calls": [{"name": "run_command", "args": {"CommandLine": "ls"}}]
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:00:03Z",
                "tool_calls": [{"name": "exit_plan_mode", "args": {"plan": "\"## Goal\\nLong plan text\""}}]
            }),
            serde_json::json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:00:04Z",
                "tool_calls": [{"name": "send_message", "args": {"Recipient": "worker-1", "Message": "\"Please check logs\""}}]
            }),
            serde_json::json!({
                "step_index": 4,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:00:05Z",
                "content": "All changes are complete and verified."
            }),
        );
        let agy_msgs = parse_agy_jsonl_with_hydration(&agy_jsonl, None, 20);
        assert_eq!(agy_msgs.len(), 4, "{agy_msgs:?}");
        assert_eq!(agy_msgs[0].kind, Some(TranscriptMessageKind::Intermediate));
        assert!(agy_msgs[0].tools.contains(&"read_file".to_string()));
        assert_eq!(agy_msgs[1].kind, Some(TranscriptMessageKind::Plan));
        assert_eq!(agy_msgs[2].kind, Some(TranscriptMessageKind::AgentMessage));
        assert_eq!(agy_msgs[3].kind, Some(TranscriptMessageKind::Final));

        // Ensure earlier pending_tools alone never demotes a tool-less final PLANNER_RESPONSE.
        let agy_final_after_tools = format!(
            "{}\n{}\n",
            serde_json::json!({
                "step_index": 10,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:01:01Z",
                "tool_calls": [{"name": "edit_file", "args": {}}]
            }),
            serde_json::json!({
                "step_index": 11,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T02:01:02Z",
                "content": "Formal summary after edits."
            }),
        );
        let agy_ft = parse_agy_jsonl_with_hydration(&agy_final_after_tools, None, 10);
        assert_eq!(agy_ft.len(), 1);
        assert_eq!(agy_ft[0].kind, Some(TranscriptMessageKind::Final));
        assert_eq!(agy_ft[0].tools, vec!["edit_file"]);

        // 2. Codex: commentary -> final_answer -> task_complete upgrade
        let codex_jsonl = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-25T02:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "commentary",
                    "content": [{"type": "output_text", "text": "Checking workspace files first..."}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-25T02:00:02Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "Done implementing fix."}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-25T02:00:03Z",
                "payload": {
                    "type": "task_complete",
                    "last_agent_message": "Done implementing fix."
                }
            }),
        );
        let codex_msgs = parse_transcript_for_agent(AgentKind::Codex, &codex_jsonl, 10);
        assert_eq!(codex_msgs.len(), 2, "{codex_msgs:?}");
        assert_eq!(
            codex_msgs[0].kind,
            Some(TranscriptMessageKind::Intermediate)
        );
        assert_eq!(codex_msgs[1].kind, Some(TranscriptMessageKind::Final));

        // Legacy Codex rollout without `phase`: task_complete upgrades to Final.
        let codex_legacy = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-25T02:00:01Z",
                "payload": {"type": "agent_message", "message": "Step 1 narration"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-25T02:00:02Z",
                "payload": {"type": "task_complete", "last_agent_message": "Step 1 narration"}
            }),
        );
        let codex_legacy_msgs = parse_transcript_for_agent(AgentKind::Codex, &codex_legacy, 10);
        assert_eq!(codex_legacy_msgs.len(), 1);
        assert_eq!(
            codex_legacy_msgs[0].kind,
            Some(TranscriptMessageKind::Final)
        );

        // 3. Claude: streaming same-id chunk with later tool_use -> Intermediate; later end_turn without tool_use -> Final
        let claude_jsonl = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-25T02:00:01Z",
                "message": {
                    "id": "msg_mid",
                    "stop_reason": null,
                    "content": [{"type": "text", "text": "Inspecting module now..."}]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-25T02:00:02Z",
                "message": {
                    "id": "msg_mid",
                    "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "name": "Read", "input": {}}]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-25T02:00:03Z",
                "message": {
                    "id": "msg_final",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Here is the final answer."}]
                }
            }),
        );
        let claude_msgs = parse_transcript_for_agent(AgentKind::Claude, &claude_jsonl, 10);
        assert_eq!(claude_msgs.len(), 2, "{claude_msgs:?}");
        assert_eq!(
            claude_msgs[0].kind,
            Some(TranscriptMessageKind::Intermediate)
        );
        assert_eq!(claude_msgs[1].kind, Some(TranscriptMessageKind::Final));
    }

    #[test]
    fn page_cache_hits_on_unchanged_file_and_invalidates_on_append_rewrite_and_truncate() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tr-pagecache-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        let make_line = |i: usize, tag: &str| {
            format!(
                r#"{{"type":"user","timestamp":"2026-11-25T00:00:{:02}Z","message":{{"role":"user","content":"{tag}-{i:04}"}}}}"#,
                i % 60
            )
        };
        let mut content = String::new();
        for i in 0..300 {
            content.push_str(&make_line(i, "msg"));
            content.push('\n');
        }
        std::fs::write(&file, &content).unwrap();

        let mut cache = TranscriptIndexCache::default();
        let q_tail = TranscriptQuery::tail(60);
        let q_all = TranscriptQuery {
            limit: 60,
            before: None,
            all: true,
        };

        // 1. Cold miss populates index + page cache.
        let r1 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_tail, None, 1000)
            .unwrap();
        assert_eq!(r1.total_messages, 300);
        assert_eq!(r1.messages.len(), 60);
        assert_eq!(cache.stats.page_misses, 1);
        assert_eq!(cache.stats.page_hits, 0);

        // 2. Repeated request with identical (path, len, mtime, query) hits page cache directly.
        let r2 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_tail, None, 1001)
            .unwrap();
        assert_eq!(r2.messages, r1.messages);
        assert_eq!(cache.stats.page_hits, 1);
        assert_eq!(cache.stats.page_misses, 1);

        // 3. Different query (`all=1`) misses page cache, reuses indexed turns with 0 delta bytes.
        let r_all_1 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_all, None, 1002)
            .unwrap();
        assert_eq!(r_all_1.messages.len(), 300);
        assert_eq!(cache.stats.page_misses, 2);
        let key_path = canonicalize_cache_path(&file);
        assert_eq!(
            cache
                .entries
                .get(&key_path)
                .unwrap()
                .index
                .delta_bytes_scanned,
            0
        );

        // Second `all=1` hits page cache.
        let r_all_2 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_all, None, 1003)
            .unwrap();
        assert_eq!(r_all_2.messages.len(), 300);
        assert_eq!(cache.stats.page_hits, 2);

        // 4. Append new message -> invalidates page cache via file_len/mtime, scans only delta bytes!
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "{}", make_line(300, "msg")).unwrap();
        drop(f);

        let r3 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_tail, None, 1004)
            .unwrap();
        assert_eq!(r3.total_messages, 301);
        assert_eq!(r3.messages.last().unwrap().text, "msg-0300");
        assert_eq!(cache.stats.page_misses, 3);
        let delta = cache
            .entries
            .get(&key_path)
            .unwrap()
            .index
            .delta_bytes_scanned;
        assert!(
            delta > 0 && delta < 300,
            "expected O(delta) scan, got {delta}"
        );

        // 5. Same-length in-place rewrite with bumped mtime -> invalidates and re-parses new text!
        std::thread::sleep(std::time::Duration::from_millis(15));
        let mut rewritten = String::new();
        for i in 0..301 {
            rewritten.push_str(&make_line(i, "new"));
            rewritten.push('\n');
        }
        assert_eq!(
            rewritten.len(),
            std::fs::metadata(&file).unwrap().len() as usize
        );
        std::fs::write(&file, &rewritten).unwrap();

        let r4 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_tail, None, 1005)
            .unwrap();
        assert_eq!(r4.total_messages, 301);
        assert_eq!(r4.messages.last().unwrap().text, "new-0300");
        assert_eq!(cache.stats.page_misses, 4);

        // 6. Truncation to smaller file -> invalidates and rebuilds cleanly.
        let truncated = format!("{}\n{}\n", make_line(0, "cut"), make_line(1, "cut"));
        std::fs::write(&file, &truncated).unwrap();
        let r5 = cache
            .get_page_or_sync(AgentKind::Claude, &file, q_tail, None, 1006)
            .unwrap();
        assert_eq!(r5.total_messages, 2);
        assert_eq!(r5.messages.len(), 2);
        assert_eq!(r5.messages[1].text, "cut-0001");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_enforces_bounded_lru_capacity_for_indices_and_pages() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tr-lru-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let make_file = |name: &str, body: &str| {
            let p = dir.join(name);
            let line = serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-25T00:00:01Z",
                "message": {"role": "user", "content": body}
            });
            std::fs::write(&p, format!("{line}\n")).unwrap();
            p
        };

        let f1 = make_file("a.jsonl", "alpha");
        let f2 = make_file("b.jsonl", "beta");
        let f3 = make_file("c.jsonl", "gamma");
        let q = TranscriptQuery::tail(10);

        // Bound to max 2 indices and max 2 pages.
        let mut cache = TranscriptIndexCache::with_capacity(2, 2, 1024 * 1024);
        assert!(cache
            .get_page_or_sync(AgentKind::Claude, &f1, q, None, 1000)
            .is_some());
        assert!(cache
            .get_page_or_sync(AgentKind::Claude, &f2, q, None, 1001)
            .is_some());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.pages.len(), 2);

        // Touch f1 again so f2 becomes the LRU victim!
        assert!(cache
            .get_page_or_sync(AgentKind::Claude, &f1, q, None, 1002)
            .is_some());
        assert_eq!(cache.stats.page_hits, 1);

        // Insert f3 -> evicts f2 (LRU), preserves f1 (MRU)!
        assert!(cache
            .get_page_or_sync(AgentKind::Claude, &f3, q, None, 1003)
            .is_some());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.pages.len(), 2);
        assert_eq!(cache.stats.index_evictions, 1);

        let k1 = canonicalize_cache_path(&f1);
        let k2 = canonicalize_cache_path(&f2);
        let k3 = canonicalize_cache_path(&f3);
        assert!(
            cache.entries.contains_key(&k1),
            "MRU entry f1 must stay cached"
        );
        assert!(
            !cache.entries.contains_key(&k2),
            "LRU entry f2 must be evicted"
        );
        assert!(
            cache.entries.contains_key(&k3),
            "new entry f3 must be cached"
        );

        // Accessing f1 again is still a page cache hit!
        let r1 = cache
            .get_page_or_sync(AgentKind::Claude, &f1, q, None, 1004)
            .unwrap();
        assert_eq!(r1.messages[0].text, "alpha");
        assert_eq!(cache.stats.page_hits, 2);

        // Test page byte budget eviction with a tiny 2 KiB cap.
        let big1 = make_file("big1.jsonl", &"A".repeat(1200));
        let big2 = make_file("big2.jsonl", &"B".repeat(1200));
        let mut byte_capped = TranscriptIndexCache::with_capacity(8, 8, 2048);
        assert!(byte_capped
            .get_page_or_sync(AgentKind::Claude, &big1, q, None, 1000)
            .is_some());
        assert_eq!(byte_capped.pages.len(), 1);
        assert!(byte_capped
            .get_page_or_sync(AgentKind::Claude, &big2, q, None, 1001)
            .is_some());
        assert_eq!(byte_capped.pages.len(), 1);
        assert!(byte_capped.page_bytes <= 2048);
        assert_eq!(byte_capped.stats.page_evictions, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agy_page_cache_invalidates_when_sibling_transcript_full_updates() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-tr-agy-full-cache-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let compact = dir.join("transcript.jsonl");
        let full = dir.join("transcript_full.jsonl");
        let compact_line = serde_json::json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-11-25T03:00:00Z",
            "content": "short...<truncated 100 bytes>",
            "truncated_fields": ["content"]
        });
        std::fs::write(&compact, format!("{compact_line}\n")).unwrap();

        let mut cache = TranscriptIndexCache::default();
        let q = TranscriptQuery::tail(10);

        // Before transcript_full.jsonl exists -> degraded truncated content cached.
        let r1 = cache
            .get_page_or_sync(AgentKind::Agy, &compact, q, None, 1000)
            .unwrap();
        assert!(r1.messages[0].text.contains("<truncated 100 bytes>"));

        // Sibling transcript_full.jsonl appears -> page cache key changes and hydrates!
        let full_line = serde_json::json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-11-25T03:00:00Z",
            "content": "Full hydrated assistant response body."
        });
        std::fs::write(&full, format!("{full_line}\n")).unwrap();

        let r2 = cache
            .get_page_or_sync(AgentKind::Agy, &compact, q, None, 1001)
            .unwrap();
        assert_eq!(
            r2.messages[0].text,
            "Full hydrated assistant response body."
        );
        assert_eq!(cache.stats.page_misses, 2);

        // Next call hits hydrated page cache.
        let r3 = cache
            .get_page_or_sync(AgentKind::Agy, &compact, q, None, 1002)
            .unwrap();
        assert_eq!(
            r3.messages[0].text,
            "Full hydrated assistant response body."
        );
        assert_eq!(cache.stats.page_hits, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_agy_hydration_steps_caps_prioritizes_newest_and_clears_after_hydration() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-agy-missing-steps-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let compact = dir.join("transcript.jsonl");
        let full = dir.join("transcript_full.jsonl");

        let mut lines = String::new();
        for step in [1u64, 3, 5, 7, 9] {
            let entry = serde_json::json!({
                "step_index": step,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T03:00:00Z",
                "content": format!("step {step} <truncated 100 bytes>"),
                "truncated_fields": ["content"]
            });
            lines.push_str(&entry.to_string());
            lines.push('\n');
        }
        std::fs::write(&compact, &lines).unwrap();

        let mut cache = TranscriptIndexCache::default();
        let q = TranscriptQuery::tail(10);
        // Capped at 3 steps: picks the 3 newest (5, 7, 9), returned sorted ascending.
        assert_eq!(
            cache.missing_agy_hydration_steps(AgentKind::Agy, &compact, q, 3),
            vec![5, 7, 9]
        );

        // Write steps 7 and 9 into sibling transcript_full.jsonl -> only 1, 3, 5 remain missing.
        let mut full_lines = String::new();
        for step in [7u64, 9] {
            let entry = serde_json::json!({
                "step_index": step,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-25T03:00:00Z",
                "content": format!("FULL STEP {step}")
            });
            full_lines.push_str(&entry.to_string());
            full_lines.push('\n');
        }
        std::fs::write(&full, &full_lines).unwrap();

        assert_eq!(
            cache.missing_agy_hydration_steps(AgentKind::Agy, &compact, q, 10),
            vec![1, 3, 5]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifies_agy_delegation_rules_and_hydration_without_false_positives() {
        let agy_jsonl = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            // 1. swarm(add) + title -> Swarm on assistant reply
            serde_json::json!({
                "step_index": 1,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "created_at": "2026-11-26T01:00:00Z",
                "content": "<USER_REQUEST>\n派发任务\n</USER_REQUEST>"
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:00:01Z",
                "tool_calls": [
                    {"name": "swarm", "args": {"action": "\"title\"", "title": "\"My Swarm\""}},
                    {"name": "swarm", "args": {"action": "\"add\"", "name": "\"worker-1\""}}
                ]
            }),
            serde_json::json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:00:02Z",
                "content": "已派出 worker-1 在后台执行。"
            }),
            // 2. swarm(status/models/kill/model) -> None (zero false positives!)
            serde_json::json!({
                "step_index": 4,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "created_at": "2026-11-26T01:01:00Z",
                "content": "<USER_REQUEST>\n检查状态\n</USER_REQUEST>"
            }),
            serde_json::json!({
                "step_index": 5,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:01:01Z",
                "tool_calls": [
                    {"name": "swarm", "args": {"action": "\"status\""}},
                    {"name": "swarm", "args": {"action": "models"}},
                    {"name": "swarm", "args": {"action": "\"kill\"", "name": "\"old\""}}
                ]
            }),
            serde_json::json!({
                "step_index": 6,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:01:02Z",
                "content": "所有 worker 已空闲，任务收尾。"
            }),
            // 3. post_message("@worker ...") -> Swarm (and propagates to immediate user-facing explanation!);
            //    post_message("[done] ...") & "@coord [done]" -> None (and clears delegation!)
            serde_json::json!({
                "step_index": 7,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:02:00Z",
                "tool_calls": [
                    {"name": "post_message", "args": {"text": "\"@worker-1 请补充测试\""}}
                ]
            }),
            serde_json::json!({
                "step_index": 70,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:02:02Z",
                "content": "已通知 worker-1 补充单测。"
            }),
            serde_json::json!({
                "step_index": 8,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:02:10Z",
                "tool_calls": [
                    {"name": "post_message", "args": {"text": "\"[done] 全部收尾完成\""}}
                ]
            }),
            serde_json::json!({
                "step_index": 9,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:02:12Z",
                "tool_calls": [
                    {"name": "post_message", "args": {"text": "\"@coordinator [done] 汇报结果\""}}
                ]
            }),
            // 4. invoke_subagent -> Subagent; invoke_subagent + swarm_wait -> Swarm priority
            serde_json::json!({
                "step_index": 10,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:03:00Z",
                "tool_calls": [{"name": "invoke_subagent", "args": {"SubagentName": "\"explorer\""}}]
            }),
            serde_json::json!({
                "step_index": 11,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:03:02Z",
                "content": "子代理已启动。"
            }),
            serde_json::json!({
                "step_index": 12,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:03:10Z",
                "tool_calls": [
                    {"name": "invoke_subagent", "args": {}},
                    {"name": "swarm_wait", "args": {"since": "10"}}
                ],
                "content": "等待 Swarm 汇总。"
            }),
            // 5. Silent swarm_wait followed by SYSTEM_MESSAGE wakeup -> post-wakeup final summary stays None!
            serde_json::json!({
                "step_index": 13,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:04:00Z",
                "tool_calls": [{"name": "swarm_wait", "args": {"since": "11"}}]
            }),
            serde_json::json!({
                "step_index": 14,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "created_at": "2026-11-26T01:04:30Z",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-26T01:04:30Z sender=board-1 priority=HIGH content=[swarm] new board message #12\n</SYSTEM_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 15,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:04:35Z",
                "tool_calls": [{"name": "read_board", "args": {}}],
                "content": "最终验收完成，全部绿灯。"
            }),
            // 6. Starfane MCP cross-agent tool -> Subagent
            serde_json::json!({
                "step_index": 16,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:05:00Z",
                "tool_calls": [{"name": "mcp__starfane__spawn_session", "args": {}}],
                "content": "已通过 MCP 拉起远程会话。"
            }),
            // 7. Trailing in-flight swarm_wait with no content yet -> synthetic trailing card gets Swarm
            serde_json::json!({
                "step_index": 17,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:06:00Z",
                "tool_calls": [{"name": "swarm_wait", "args": {"since": "12"}}]
            }),
        );

        let msgs = parse_agy_jsonl_with_hydration(&agy_jsonl, None, 30);
        // Expected messages:
        // 0: User("派发任务")
        // 1: Assistant("已派出 worker-1...") -> Swarm
        // 2: User("检查状态")
        // 3: Assistant("所有 worker 已空闲...") -> None
        // 4: Assistant("[群组消息]\n@worker-1...") -> Swarm
        // 5: Assistant("已通知 worker-1 补充单测。") -> Swarm (propagated from AgentMessage!)
        // 6: Assistant("[群组消息]\n[done] 全部收尾完成") -> None
        // 7: Assistant("[群组消息]\n@coordinator [done] 汇报结果") -> None
        // 8: Assistant("子代理已启动。") -> Subagent
        // 9: Assistant("等待 Swarm 汇总。") -> Swarm
        // 10: System("[Swarm 通知]\n...") -> None
        // 11: Assistant("最终验收完成，全部绿灯。") -> None
        // 12: Assistant("已通过 MCP 拉起远程会话。") -> Subagent
        // 13: Assistant("") trailing pending_tools=["swarm_wait"] -> Swarm
        assert_eq!(msgs.len(), 14, "{msgs:?}");
        assert_eq!(msgs[0].delegation, None);
        assert_eq!(msgs[1].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[3].delegation, None);
        assert_eq!(msgs[4].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[4].kind, Some(TranscriptMessageKind::AgentMessage));
        assert_eq!(msgs[5].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[6].delegation, None);
        assert_eq!(msgs[7].delegation, None);
        assert_eq!(msgs[8].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[9].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[10].role, TranscriptRole::System);
        assert_eq!(msgs[11].delegation, None);
        assert_eq!(msgs[12].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[13].text, "");
        assert_eq!(msgs[13].delegation, Some(DelegationKind::Swarm));

        // Verify hydration from transcript_full.jsonl preserves delegation!
        let dir = std::env::temp_dir().join(format!(
            "starfane-agy-del-hyd-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let compact = dir.join("transcript.jsonl");
        let full = dir.join("transcript_full.jsonl");
        let compact_text = format!(
            "{}\n{}\n",
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:10:00Z",
                "tool_calls": [{"name": "swarm", "args": {"action": "\"add\""}}]
            }),
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:10:01Z",
                "content": "已派发...<truncated 120 bytes>",
                "truncated_fields": ["content"]
            }),
        );
        let full_text = format!(
            "{}\n",
            serde_json::json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-26T01:10:01Z",
                "content": "已派发完整的 Swarm 协作任务详情。"
            })
        );
        std::fs::write(&compact, &compact_text).unwrap();
        std::fs::write(&full, &full_text).unwrap();
        let hydrated = parse_agy_jsonl_with_hydration(&compact_text, Some(&compact), 10);
        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].text, "已派发完整的 Swarm 协作任务详情。");
        assert_eq!(hydrated[0].delegation, Some(DelegationKind::Swarm));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifies_claude_delegation_rules_including_async_background_agents() {
        let jsonl = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            // 1. Silent Agent tool_use -> tool_result -> async reply ("Launched background agent")
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:00:00Z",
                "message": {"role": "user", "content": "Investigate memory leak"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:00:01Z",
                "message": {
                    "id": "m1",
                    "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "name": "Agent", "input": {"run_in_background": true}}]
                }
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:00:02Z",
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "Async agent launched"}]}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:00:03Z",
                "message": {
                    "id": "m2",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "I launched a background Agent to profile the heap."}]
                }
            }),
            // 2. Streaming same-msg_id text + TeamCreate -> tool_result -> follow-up text -> BOTH Swarm
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:01:00Z",
                "message": {"role": "user", "content": "Spin up a team"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:01:01Z",
                "message": {
                    "id": "m3",
                    "stop_reason": null,
                    "content": [{"type": "text", "text": "Creating an agent team now..."}]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:01:02Z",
                "message": {
                    "id": "m3",
                    "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "name": "TeamCreate", "input": {}}]
                }
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:01:03Z",
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "team created"}]}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:01:04Z",
                "message": {
                    "id": "m4",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Team is active in background."}]
                }
            }),
            // 3. Task / SendMessage / MCP starfane tool -> Subagent
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:02:00Z",
                "message": {"role": "user", "content": "Hand off task"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:02:01Z",
                "message": {
                    "id": "m5",
                    "stop_reason": "tool_use",
                    "content": [
                        {"type": "text", "text": "Dispatching via Task & SendMessage."},
                        {"type": "tool_use", "name": "Task", "input": {}},
                        {"type": "tool_use", "name": "SendMessage", "input": {}}
                    ]
                }
            }),
            // 4. Normal Read/Bash turn after new user prompt -> None
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-26T02:03:00Z",
                "message": {"role": "user", "content": "Check git status"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-26T02:03:01Z",
                "message": {
                    "id": "m6",
                    "stop_reason": "end_turn",
                    "content": [
                        {"type": "tool_use", "name": "Read", "input": {}},
                        {"type": "tool_use", "name": "Bash", "input": {}},
                        {"type": "text", "text": "Working tree is clean."}
                    ]
                }
            }),
        );

        let msgs = parse_transcript_for_agent(AgentKind::Claude, &jsonl, 20);
        assert_eq!(msgs.len(), 9, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[1].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[2].role, TranscriptRole::User);
        assert_eq!(msgs[3].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[4].delegation, Some(DelegationKind::Swarm));
        assert_eq!(msgs[5].role, TranscriptRole::User);
        assert_eq!(msgs[6].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[7].role, TranscriptRole::User);
        assert_eq!(msgs[8].delegation, None);
    }

    #[test]
    fn classifies_codex_delegation_rules_and_mcp_tools_preserving_across_dedup() {
        let jsonl = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            // 1. spawn_agent -> function_call_output -> message + agent_message + task_complete dedup -> Subagent
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:00:00Z",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Delegate review"}]}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:00:01Z",
                "payload": {"type": "function_call", "name": "spawn_agent", "call_id": "c1"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:00:02Z",
                "payload": {"type": "function_call_output", "call_id": "c1", "output": "{\"agent_id\":\"a1\"}"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:00:03Z",
                "payload": {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Spawned worker a1 to review the PR."}]}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-26T03:00:03.100Z",
                "payload": {"type": "agent_message", "message": "Spawned worker a1 to review the PR."}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-26T03:00:03.200Z",
                "payload": {"type": "task_complete", "last_agent_message": "Spawned worker a1 to review the PR."}
            }),
            // 2. wait_agent & send_input -> Subagent
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:01:00Z",
                "payload": {"type": "function_call", "name": "send_input", "call_id": "c2"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:01:01Z",
                "payload": {"type": "function_call", "name": "wait_agent", "call_id": "c3"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-26T03:01:02Z",
                "payload": {"type": "agent_message", "message": "Waiting for worker a1 response."}
            }),
            // 3. close_agent & exec_command -> None (zero false positives!)
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:02:00Z",
                "payload": {"type": "function_call", "name": "close_agent", "call_id": "c4"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:02:01Z",
                "payload": {"type": "custom_tool_call", "name": "exec_command", "call_id": "c5"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-11-26T03:02:02Z",
                "payload": {"type": "task_complete", "last_agent_message": "Closed worker and verified tests."}
            }),
            // 4. Starfane MCP tool -> Subagent
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:03:00Z",
                "payload": {"type": "function_call", "name": "mcp__starfane__spawn_session", "call_id": "c6"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:03:01Z",
                "payload": {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Spawned Starfane session."}]}
            }),
            // 5. In-flight trailing spawn_agent -> synthetic message has Subagent
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-11-26T03:04:00Z",
                "payload": {"type": "function_call", "name": "spawn_agent", "call_id": "c7"}
            }),
        );

        let msgs = parse_transcript_for_agent(AgentKind::Codex, &jsonl, 20);
        assert_eq!(msgs.len(), 6, "{msgs:?}");
        assert_eq!(msgs[0].role, TranscriptRole::User);
        assert_eq!(msgs[1].text, "Spawned worker a1 to review the PR.");
        assert_eq!(msgs[1].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[1].kind, Some(TranscriptMessageKind::Final));
        assert_eq!(msgs[2].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[3].text, "Closed worker and verified tests.");
        assert_eq!(msgs[3].delegation, None);
        assert_eq!(msgs[4].delegation, Some(DelegationKind::Subagent));
        assert_eq!(msgs[5].text, "");
        assert_eq!(msgs[5].delegation, Some(DelegationKind::Subagent));
    }

    #[test]
    fn watermark_tracks_in_place_pending_tools_claude_merge_and_agy_hydration() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-wm-test-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 1. AGY: trailing synthetic pending_tools -> final PLANNER_RESPONSE keeps total_messages == 2
        //    while changing last_turn_hash & has_pending_tools, and late hydration changes hash again.
        let agy_compact = dir.join("transcript.jsonl");
        let agy_full = dir.join("transcript_full.jsonl");
        let u1 = r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-26T00:00:00Z","content":"run checks"}"#;
        let p_tools = r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:01Z","tool_calls":[{"name":"run_command","args":{"CommandLine":"cargo test"}}]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n")).unwrap();

        let mut cache = TranscriptIndexCache::default();
        let wm1 = cache
            .watermark_for_path(AgentKind::Agy, &agy_compact)
            .unwrap();
        assert_eq!(wm1.total_messages, 2);
        assert!(wm1.has_pending_tools);

        // Final PLANNER_RESPONSE replaces synthetic pending_tools at index 1 (still total_messages == 2),
        // initially truncated in compact transcript.jsonl.
        let p_final_trunc = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"Short preview... <truncated 200 chars>","truncated_fields":["content"]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n{p_final_trunc}\n")).unwrap();
        let wm2 = cache
            .watermark_for_path(AgentKind::Agy, &agy_compact)
            .unwrap();
        assert_eq!(wm2.total_messages, 2);
        assert!(!wm2.has_pending_tools);
        assert_ne!(wm2.last_turn_hash, wm1.last_turn_hash);

        // Late write to sibling transcript_full.jsonl hydrates step 2 without changing total_messages!
        let p_final_full = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"All 42 workspace tests passed cleanly."}"#;
        std::fs::write(&agy_full, format!("{p_final_full}\n")).unwrap();
        let wm3 = cache
            .watermark_for_path(AgentKind::Agy, &agy_compact)
            .unwrap();
        assert_eq!(wm3.total_messages, 2);
        assert!(!wm3.has_pending_tools);
        assert_ne!(wm3.last_turn_hash, wm2.last_turn_hash);

        // 2. Claude: same-msg_id in-place append mutates last_turn_hash with flat total_messages.
        let claude_path = dir.join("claude.jsonl");
        let c_user =
            r#"{"type":"user","timestamp":"2026-11-26T00:00:00Z","message":{"content":"hi"}}"#;
        let c_a1 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:01Z","message":{"id":"msg_1","stop_reason":null,"content":[{"type":"text","text":"Working on it"}]}}"#;
        std::fs::write(&claude_path, format!("{c_user}\n{c_a1}\n")).unwrap();
        let c_wm1 = cache
            .watermark_for_path(AgentKind::Claude, &claude_path)
            .unwrap();
        assert_eq!(c_wm1.total_messages, 2);

        let c_a2 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:02Z","message":{"id":"msg_1","stop_reason":"end_turn","content":[{"type":"text","text":"Done!"}]}}"#;
        std::fs::write(&claude_path, format!("{c_user}\n{c_a1}\n{c_a2}\n")).unwrap();
        let c_wm2 = cache
            .watermark_for_path(AgentKind::Claude, &claude_path)
            .unwrap();
        assert_eq!(c_wm2.total_messages, 2);
        assert_ne!(c_wm2.last_turn_hash, c_wm1.last_turn_hash);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_transcript_metrics_preserves_agy_db_ttft_and_recent_turns() {
        let agy_db = SessionMetrics {
            context_used_tokens: Some(180_000),
            context_window_tokens: None,
            context_used_pct: None,
            tps: Some(95.4),
            ttft_ms: Some(3_100),
            avg_ttft_ms: Some(2_800),
            last_output_tokens: Some(450),
            last_duration_ms: Some(4_716),
            total_input_tokens: None,
            total_output_tokens: None,
            recent_turns: vec![starfane_proto::TurnMetricSample {
                ts_ms: 5_000,
                ttft_ms: Some(3_100),
                tps: Some(95.4),
                output_tokens: Some(450),
                duration_ms: Some(4_716),
            }],
            source: "agy_db".into(),
            updated_ms: 5_000,
        };
        let parsed = SessionMetrics {
            context_used_tokens: Some(175_000),
            context_window_tokens: Some(512_000),
            context_used_pct: Some(34.2),
            tps: Some(50.0),
            ttft_ms: None,
            avg_ttft_ms: None,
            last_output_tokens: Some(200),
            last_duration_ms: Some(4_000),
            total_input_tokens: Some(900_000),
            total_output_tokens: Some(12_000),
            recent_turns: Vec::new(),
            source: "transcript".into(),
            updated_ms: 4_900,
        };
        let merged = merge_transcript_metrics(Some(agy_db), Some(parsed)).unwrap();
        assert_eq!(merged.source, "agy_db");
        assert_eq!(merged.ttft_ms, Some(3_100));
        assert_eq!(merged.avg_ttft_ms, Some(2_800));
        assert_eq!(merged.tps, Some(95.4));
        assert_eq!(merged.recent_turns.len(), 1);
        assert_eq!(merged.context_window_tokens, Some(512_000));
        assert_eq!(merged.context_used_tokens, Some(180_000));
        assert_eq!(merged.context_used_pct, Some(35.2));
        assert_eq!(merged.total_input_tokens, Some(900_000));
        assert_eq!(merged.total_output_tokens, Some(12_000));
    }

    #[test]
    fn multi_session_150_transcripts_steady_state_and_small_append_never_evict_or_reparse() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "starfane-scale-150-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Simulate 150 concurrent root + subagent transcripts (exceeding the legacy 64 cap).
        let session_count = 150usize;
        let pad = "x".repeat(4_096);
        let mut paths = Vec::with_capacity(session_count);
        for i in 0..session_count {
            let p = dir.join(format!("sess-{i:03}.jsonl"));
            let mut body = String::with_capacity(20_000);
            for step in 0..4 {
                body.push_str(&format!(
                    r#"{{"step_index":{step},"type":"PLANNER_RESPONSE","created_at":"2026-11-27T00:00:0{step}Z","content":"session {i} step {step} {pad}"}}"#
                ));
                body.push('\n');
            }
            std::fs::write(&p, &body).unwrap();
            paths.push(p);
        }

        let mut cache = TranscriptIndexCache::default();
        cache.ensure_min_capacity(session_count + 32);

        // Pass 1 (cold start): indexes all 150 files once with zero evictions.
        for p in &paths {
            let wm = cache.watermark_for_path(AgentKind::Agy, p).unwrap();
            assert_eq!(wm.total_messages, 4);
        }
        assert_eq!(cache.stats.index_evictions, 0);

        // Pass 2 & Pass 3 (steady-state 3s discovery ticks): 0 evictions, 0 bytes re-scanned.
        for _tick in 0..2 {
            for p in &paths {
                let idx = cache.get_or_sync(AgentKind::Agy, p).unwrap();
                assert_eq!(idx.delta_bytes_scanned, 0);
                let wm = idx.watermark(p);
                assert_eq!(wm.total_messages, 4);
            }
        }
        assert_eq!(cache.stats.index_evictions, 0);

        // Small append to one transcript scans ONLY the appended delta bytes, never the prefix.
        let appended = r#"{"step_index":4,"type":"PLANNER_RESPONSE","created_at":"2026-11-27T00:00:09Z","content":"incremental tail reply"}"#;
        let appended_line = format!("{appended}\n");
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&paths[42])
                .unwrap();
            f.write_all(appended_line.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        for (i, p) in paths.iter().enumerate() {
            let idx = cache.get_or_sync(AgentKind::Agy, p).unwrap();
            if i == 42 {
                assert_eq!(idx.delta_bytes_scanned, appended_line.len() as u64);
                assert_eq!(idx.watermark(p).total_messages, 5);
            } else {
                assert_eq!(idx.delta_bytes_scanned, 0);
                assert_eq!(idx.watermark(p).total_messages, 4);
            }
        }
        assert_eq!(cache.stats.index_evictions, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn giant_jsonl_lines_skip_generic_and_parse_large_messages_without_quadratic_scan() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-giant-line-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("giant.jsonl");

        let huge_blob = "B".repeat(2_200_000);
        // 2.2 MiB GENERIC line (>1 MiB skip threshold) followed by a 2.2 MiB valid assistant message (<4 MiB hard cap).
        let generic_line = format!(
            r#"{{"step_index":1,"source":"MODEL","type":"GENERIC","created_at":"2026-11-27T01:00:00Z","content":"{huge_blob}"}}"#
        );
        let assistant_line = format!(
            r#"{{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-27T01:00:02Z","content":"Valid prefix {huge_blob}"}}"#
        );
        std::fs::write(&path, format!("{generic_line}\n{assistant_line}\n")).unwrap();

        let t0 = std::time::Instant::now();
        let mut cache = TranscriptIndexCache::default();
        let resp = cache
            .get_page_or_sync(AgentKind::Agy, &path, TranscriptQuery::tail(10), None, 0)
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(resp.messages.len(), 1);
        assert!(resp.messages[0].text.starts_with("Valid prefix BBBB"));
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "giant line scan took too long: {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
