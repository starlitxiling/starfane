//! Session context-window & generation-speed (TPS) + TTFT telemetry extraction:
//! - Native AGY SQLite protobuf step-timing extractor (`<home>/.gemini/antigravity-cli/conversations/<conv_id>.db`,
//!   `step_type = 15 AND status = 3`) with `(mtime_ns, len)` DB+WAL caching (`AgyStepTimingCache`) and
//!   buffered-burst / reliability filtering.
//! - Exact transcript JSONL resolution (Claude subtree PIDs → `~/.claude/sessions/<pid>.json`,
//!   Codex `resume <uuid>` & `~/.codex/state_5.sqlite` `threads.name` + `cwd`, AGY
//!   `conversation_summaries.db` unique-cwd root conversation).
//! - Transcript JSONL metrics (Claude / Codex / AGY fallback) with `(mtime, len)` file caching +
//!   status-line context-window fallback (100% independent of `cc-switch.db`).

use super::transcript::parse_iso8601_ms;
#[cfg(test)]
use super::transcript::{read_tail_utf8, MAX_TRANSCRIPT_READ_BYTES};
use serde::Deserialize;
use serde_json::Value;
use starfane_proto::{AgentKind, SessionMetrics, SessionRecord, TurnMetricSample};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Minimum streaming generation duration (ms) for reliable sustained TPS.
pub(crate) const MIN_STREAM_TPS_DURATION_MS: u64 = 1_500;
/// Minimum output tokens for reliable sustained TPS.
pub(crate) const MIN_STREAM_TPS_OUTPUT_TOKENS: u64 = 150;
/// Minimum visible (non-reasoning) output tokens for Codex streaming TPS.
pub(crate) const MIN_CODEX_STREAM_VISIBLE_TOKENS: u64 = 80;
/// Minimum duration (ms) for fallback TPS when no reliable streaming turn exists yet.
pub(crate) const MIN_FALLBACK_TPS_DURATION_MS: u64 = 1_000;
/// Minimum output tokens for fallback TPS when no reliable streaming turn exists yet.
pub(crate) const MIN_FALLBACK_TPS_OUTPUT_TOKENS: u64 = 40;
/// Maximum single-call generation duration (ms) accepted for TPS calculation (5 minutes).
pub(crate) const MAX_TPS_DURATION_MS: u64 = 300_000;
/// Minimum plausible TTFT (ms) accepted for first-token latency reporting.
pub(crate) const MIN_TTFT_MS: u64 = 50;
/// Maximum plausible TTFT (ms) accepted for first-token latency reporting (3 minutes).
pub(crate) const MAX_TTFT_MS: u64 = 180_000;
/// Sanity upper bound on computed TPS to guard against clock skew / batched timestamps.
pub(crate) const MAX_PLAUSIBLE_TPS: f64 = 400.0;
/// Maximum number of per-turn samples retained in `SessionMetrics.recent_turns`.
pub(crate) const MAX_TURN_HISTORY: usize = 20;
/// Maximum number of AGY conversation DB caches retained in memory.
const MAX_AGY_STEP_CACHE_ENTRIES: usize = 128;
/// Clock-granularity tolerance (5s) when comparing conversation/file timestamps against OS process
/// start time (`ps -o etime=` has 1-second integer resolution).
pub(crate) const PROCESS_BIRTH_SLACK_MS: u64 = 5_000;
/// Maximum allowed skew (2m) between `~/.claude/sessions/<pid>.json`'s `startedAt` and OS process
/// birth time before treating `<pid>.json` as a stale leftover from a recycled OS PID.
pub(crate) const CLAUDE_PID_START_MAX_SKEW_MS: u64 = 120_000;
/// Maximum bytes inspected per header line when extracting a transcript's creation timestamp.
const MAX_BIRTH_HEADER_LINE_BYTES: usize = 64 * 1024;
/// Maximum non-empty leading lines inspected to locate the earliest JSONL timestamp.
const MAX_BIRTH_HEADER_LINES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DbSig {
    db: Option<(u128, u64)>,
    wal: Option<(u128, u64)>,
}

pub(crate) fn file_signature(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime_ns, meta.len()))
}

#[cfg(unix)]
pub(crate) fn file_identity(path: &Path) -> Option<(u64, u64, u128, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((meta.dev(), meta.ino(), mtime_ns, meta.len()))
}

#[cfg(not(unix))]
pub(crate) fn file_identity(path: &Path) -> Option<(u64, u64, u128, u64)> {
    let (mtime_ns, len) = file_signature(path)?;
    Some((0, 0, mtime_ns, len))
}

fn db_signature(db_path: &Path) -> DbSig {
    let mut wal_os = db_path.as_os_str().to_os_string();
    wal_os.push("-wal");
    DbSig {
        db: file_signature(db_path),
        wal: file_signature(Path::new(&wal_os)),
    }
}

fn normalize_sqlite_json_stdout(stdout: Vec<u8>) -> Option<String> {
    let s = String::from_utf8(stdout).ok()?;
    if s.trim().is_empty() {
        // `sqlite3 -json` outputs an empty stdout (not `[]`) when a SELECT returns zero rows.
        Some("[]".to_string())
    } else {
        Some(s)
    }
}

fn run_sqlite_json(db_path: &Path, sql: &str) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let out = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db_path)
        .arg(sql)
        .output()
        .ok()?;
    if out.status.success() {
        return normalize_sqlite_json_stdout(out.stdout);
    }
    // Retry via immutable URI in case the DB lives on a lock-restricted directory (e.g. AGY summaries DB).
    let uri = format!("file:{}?immutable=1", db_path.display());
    let retry = Command::new("sqlite3")
        .arg("-json")
        .arg(&uri)
        .arg(sql)
        .output()
        .ok()?;
    if !retry.status.success() {
        return None;
    }
    normalize_sqlite_json_stdout(retry.stdout)
}

// ---------------------------------------------------------------------------
// File-level `(mtime, len)` cache for JSONL transcript metrics
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct CachedFileMetrics {
    pub mtime_ns: u128,
    pub len: u64,
    pub metrics: Option<SessionMetrics>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Return transcript-derived `(metrics, model, effort)` for `path`, hitting the in-memory
/// `(mtime, len)` cache whenever the file has not changed (zero disk reads on cache hit).
#[cfg(test)]
pub(crate) fn cached_transcript_telemetry(
    cache: &mut HashMap<PathBuf, CachedFileMetrics>,
    agent: AgentKind,
    path: &Path,
    now_ms: u64,
) -> (Option<SessionMetrics>, Option<String>, Option<String>) {
    let Some((mtime_ns, len)) = file_signature(path) else {
        return (None, None, None);
    };
    if let Some(hit) = cache.get(path) {
        if hit.mtime_ns == mtime_ns && hit.len == len {
            return (hit.metrics.clone(), hit.model.clone(), hit.effort.clone());
        }
    }
    let Ok(tail) = read_tail_utf8(path, MAX_TRANSCRIPT_READ_BYTES) else {
        return (None, None, None);
    };
    let mut fold = TelemetryFold::new(agent);
    fold.feed_text(&tail);
    let (metrics, model, effort) = fold.snapshot(now_ms);
    cache.insert(
        path.to_path_buf(),
        CachedFileMetrics {
            mtime_ns,
            len,
            metrics: metrics.clone(),
            model: model.clone(),
            effort: effort.clone(),
        },
    );
    (metrics, model, effort)
}

/// Extract normalized AGY thinking effort from an AGY model identifier (e.g.
/// `"gemini-4-neon-high"` -> `"high"`, `"gemini-4-neon-xhigh"` -> `"xhigh"`,
/// `"Orchestrator"` -> `"fixed"`).
pub(crate) fn extract_agy_effort_from_model(model: &str) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("orchestrator") {
        return Some("fixed".to_string());
    }
    for (suffix, norm) in [
        ("-xhigh", "xhigh"),
        ("_xhigh", "xhigh"),
        (" (xhigh)", "xhigh"),
        ("-max", "max"),
        ("_max", "max"),
        (" (max)", "max"),
        ("-high", "high"),
        ("_high", "high"),
        (" (high)", "high"),
        ("-medium", "medium"),
        ("_medium", "medium"),
        (" (medium)", "medium"),
        ("-med", "medium"),
        ("-low", "low"),
        ("_low", "low"),
        (" (low)", "low"),
        ("-minimal", "minimal"),
        ("_minimal", "minimal"),
    ] {
        if lower.ends_with(suffix) {
            return Some(norm.to_string());
        }
    }
    None
}

/// Scan Codex rollout JSONL `turn_context` entries (keeping the latest) for `(model, effort)`.
#[cfg(test)]
pub(crate) fn extract_codex_turn_context(text: &str) -> (Option<String>, Option<String>) {
    let mut fold = TelemetryFold::new(AgentKind::Codex);
    fold.feed_text(text);
    let (_, model, effort) = fold.snapshot(0);
    (model, effort)
}

/// Parse default `model_reasoning_effort` from a Codex `config.toml` string (zero local disk IO).
pub fn parse_codex_default_effort_toml(text: &str) -> Option<String> {
    let val: toml::Value = toml::from_str(text).ok()?;
    val.get("model_reasoning_effort")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Read default `model_reasoning_effort` from `<home>/.codex/config.toml`.
pub(crate) fn read_codex_default_effort(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".codex/config.toml")).ok()?;
    parse_codex_default_effort_toml(&text)
}

/// Parse default effort level from a Claude `settings.json` string (`modelSettings.<model>.effortLevel`
/// with fallback to top-level `effortLevel`, zero local disk IO).
pub fn parse_claude_default_effort_json(text: &str, model: Option<&str>) -> Option<String> {
    let obj: Value = serde_json::from_str(text).ok()?;
    if let (Some(m), Some(ms)) = (
        model.map(str::trim).filter(|s| !s.is_empty()),
        obj.get("modelSettings").and_then(Value::as_object),
    ) {
        if let Some(eff) = ms
            .get(m)
            .and_then(|v| v.get("effortLevel"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(eff.to_ascii_lowercase());
        }
    }
    obj.get("effortLevel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Read default effort level from `<home>/.claude/settings.json` (`modelSettings.<model>.effortLevel`
/// with fallback to top-level `effortLevel`).
pub(crate) fn read_claude_default_effort(home: &Path, model: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".claude/settings.json")).ok()?;
    parse_claude_default_effort_json(&text, model)
}

/// Infer the best reasoning/thinking effort for a session from explicit flags, JSONL `turn_context`,
/// AGY model ID suffix, or agent default config files.
pub(crate) fn infer_session_effort(
    agent: AgentKind,
    model: Option<&str>,
    explicit_effort: Option<&str>,
    jsonl_effort: Option<&str>,
    home: Option<&Path>,
) -> Option<String> {
    if let Some(e) = explicit_effort.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(e.to_ascii_lowercase());
    }
    match agent {
        AgentKind::Agy => model.and_then(extract_agy_effort_from_model),
        AgentKind::Codex => jsonl_effort
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .or_else(|| home.and_then(read_codex_default_effort)),
        AgentKind::Claude => home.and_then(|h| read_claude_default_effort(h, model)),
        AgentKind::Shell => None,
    }
}
// Removed leftover tail lines after cached_transcript_metrics refactor.

// ---------------------------------------------------------------------------
// Telemetry math helpers & native AGY protobuf step-timing extractor
// ---------------------------------------------------------------------------

/// Insert a per-turn telemetry sample in chronological (`ts_ms`) order, capped at [`MAX_TURN_HISTORY`].
pub(crate) fn push_turn_sample(turns: &mut Vec<TurnMetricSample>, sample: TurnMetricSample) {
    let idx = turns.partition_point(|t| t.ts_ms <= sample.ts_ms);
    turns.insert(idx, sample);
    if turns.len() > MAX_TURN_HISTORY {
        let excess = turns.len() - MAX_TURN_HISTORY;
        turns.drain(..excess);
    }
}

/// Compute average valid TTFT (ms) across `turns` in `[MIN_TTFT_MS, MAX_TTFT_MS]`.
pub(crate) fn compute_avg_ttft_ms(turns: &[TurnMetricSample]) -> Option<u64> {
    let mut sum: u128 = 0;
    let mut count: u128 = 0;
    for t in turns {
        if let Some(ttft) = t
            .ttft_ms
            .filter(|&d| (MIN_TTFT_MS..=MAX_TTFT_MS).contains(&d))
        {
            sum = sum.saturating_add(u128::from(ttft));
            count += 1;
        }
    }
    sum.checked_div(count)
        .and_then(|avg| u64::try_from(avg).ok())
}

/// Compute reliability-filtered tokens-per-second rounded to 1 decimal place.
pub(crate) fn compute_tps_checked(
    output_tokens: u64,
    duration_ms: u64,
    min_tokens: u64,
    min_dur_ms: u64,
) -> Option<f32> {
    if output_tokens == 0
        || output_tokens < min_tokens
        || duration_ms < min_dur_ms.max(1)
        || duration_ms > MAX_TPS_DURATION_MS
    {
        return None;
    }
    let raw = (output_tokens as f64) / ((duration_ms as f64) / 1000.0);
    if !raw.is_finite() || raw <= 0.0 || raw > MAX_PLAUSIBLE_TPS {
        return None;
    }
    Some(((raw * 10.0).round() / 10.0) as f32)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AgyStepTiming {
    pub idx: i64,
    pub created_at_ms: Option<u64>,
    pub first_token_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub thinking_tokens: u64,
    pub visible_tokens: u64,
}

fn read_proto_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        let &b = buf.get(*pos)?;
        *pos += 1;
        val |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn skip_proto_field(buf: &[u8], pos: &mut usize, wire_type: u64) -> Option<()> {
    match wire_type {
        0 => {
            read_proto_varint(buf, pos)?;
            Some(())
        }
        1 => {
            if buf.len().checked_sub(*pos)? < 8 {
                return None;
            }
            *pos += 8;
            Some(())
        }
        2 => {
            let len = usize::try_from(read_proto_varint(buf, pos)?).ok()?;
            let end = pos.checked_add(len)?;
            if end > buf.len() {
                return None;
            }
            *pos = end;
            Some(())
        }
        5 => {
            if buf.len().checked_sub(*pos)? < 4 {
                return None;
            }
            *pos += 4;
            Some(())
        }
        _ => None,
    }
}

fn decode_proto_timestamp_ms(bytes: &[u8]) -> Option<Option<u64>> {
    let mut pos = 0usize;
    let mut secs: u64 = 0;
    let mut nanos: u64 = 0;
    while pos < bytes.len() {
        let tag = read_proto_varint(bytes, &mut pos)?;
        let field = tag >> 3;
        let wire_type = tag & 0x07;
        if field == 0 {
            return None;
        }
        match (field, wire_type) {
            (1, 0) => secs = read_proto_varint(bytes, &mut pos)?,
            (2, 0) => nanos = read_proto_varint(bytes, &mut pos)?,
            _ => skip_proto_field(bytes, &mut pos, wire_type)?,
        }
    }
    if secs == 0 {
        return Some(None);
    }
    let ms = secs.checked_mul(1_000)?.checked_add(nanos / 1_000_000)?;
    Some(Some(ms))
}

fn decode_proto_usage(bytes: &[u8], out: &mut AgyStepTiming) -> Option<()> {
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_proto_varint(bytes, &mut pos)?;
        let field = tag >> 3;
        let wire_type = tag & 0x07;
        if field == 0 {
            return None;
        }
        match (field, wire_type) {
            (2, 0) => out.input_tokens = read_proto_varint(bytes, &mut pos)?,
            (3, 0) => out.output_tokens = read_proto_varint(bytes, &mut pos)?,
            (5, 0) => out.cache_read_tokens = read_proto_varint(bytes, &mut pos)?,
            (9, 0) => out.thinking_tokens = read_proto_varint(bytes, &mut pos)?,
            (10, 0) => out.visible_tokens = read_proto_varint(bytes, &mut pos)?,
            _ => skip_proto_field(bytes, &mut pos, wire_type)?,
        }
    }
    Some(())
}

/// Zero-dependency pure-Rust protobuf wire decoder for AGY `steps.metadata` blobs
/// (`step_type = 15 AND status = 3`). Safely skips unknown fields in any order.
pub(crate) fn parse_agy_step_metadata_proto(bytes: &[u8]) -> Option<AgyStepTiming> {
    if bytes.is_empty() {
        return None;
    }
    let mut out = AgyStepTiming::default();
    let mut pos = 0usize;
    let mut saw_any = false;
    while pos < bytes.len() {
        let tag = read_proto_varint(bytes, &mut pos)?;
        let field = tag >> 3;
        let wire_type = tag & 0x07;
        if field == 0 {
            return None;
        }
        if wire_type == 2 {
            let len = usize::try_from(read_proto_varint(bytes, &mut pos)?).ok()?;
            let end = pos.checked_add(len)?;
            let sub = bytes.get(pos..end)?;
            pos = end;
            match field {
                1 => {
                    out.created_at_ms = decode_proto_timestamp_ms(sub)?;
                    saw_any |= out.created_at_ms.is_some();
                }
                6 => {
                    out.first_token_at_ms = decode_proto_timestamp_ms(sub)?;
                    saw_any |= out.first_token_at_ms.is_some();
                }
                7 => {
                    out.completed_at_ms = decode_proto_timestamp_ms(sub)?;
                    saw_any |= out.completed_at_ms.is_some();
                }
                9 => {
                    decode_proto_usage(sub, &mut out)?;
                    saw_any |= out.input_tokens > 0
                        || out.output_tokens > 0
                        || out.cache_read_tokens > 0
                        || out.thinking_tokens > 0
                        || out.visible_tokens > 0;
                }
                _ => {}
            }
        } else {
            skip_proto_field(bytes, &mut pos, wire_type)?;
        }
    }
    saw_any.then_some(out)
}

/// Decode an uppercase/lowercase hexadecimal string into raw bytes.
pub(crate) fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let trimmed = hex.trim();
    if trimmed.is_empty() || trimmed.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let bytes = trimmed.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Parse tab-delimited `idx\thex(metadata)` rows returned by remote `sqlite3` queries.
pub(crate) fn parse_agy_step_timing_tsv(text: &str) -> Vec<AgyStepTiming> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((idx_raw, hex_raw)) = trimmed.split_once('\t') else {
            continue;
        };
        let Ok(idx) = idx_raw.trim().parse::<i64>() else {
            continue;
        };
        let Some(bytes) = decode_hex(hex_raw.trim()) else {
            continue;
        };
        if let Some(mut timing) = parse_agy_step_metadata_proto(&bytes) {
            timing.idx = idx;
            out.push(timing);
        }
    }
    out
}

/// Convert decoded AGY `steps.metadata` timings into `SessionMetrics` (`source: "agy_db"`),
/// preserving cumulative totals & context window from `base_transcript` while recording
/// per-turn TTFT + sustained streaming TPS with buffered-burst filtering.
pub(crate) fn agy_step_timings_to_metrics(
    timings: &[AgyStepTiming],
    model: Option<&str>,
    base_transcript: Option<SessionMetrics>,
    now_ms: u64,
) -> Option<SessionMetrics> {
    if timings.is_empty() {
        return None;
    }
    let mut sorted: Vec<&AgyStepTiming> = timings.iter().collect();
    sorted.sort_by_key(|t| t.idx);

    let mut recent_turns: Vec<TurnMetricSample> = Vec::new();
    let mut reliable_tps: Option<(f32, u64, u64)> = None;
    let mut fallback_tps: Option<(f32, u64, u64)> = None;
    let mut latest_db_ctx: Option<u64> = None;
    let mut latest_ts: Option<u64> = None;
    let mut sum_in: u64 = 0;
    let mut sum_out: u64 = 0;

    for t in &sorted {
        let out = if t.output_tokens > 0 {
            t.output_tokens
        } else {
            t.thinking_tokens.saturating_add(t.visible_tokens)
        };
        let ctx = t.input_tokens.saturating_add(t.cache_read_tokens);
        if ctx > 0 {
            latest_db_ctx = Some(ctx);
            sum_in = sum_in.saturating_add(ctx);
        }
        if out > 0 {
            sum_out = sum_out.saturating_add(out);
        }

        let ttft_ms = match (t.created_at_ms, t.first_token_at_ms) {
            (Some(f1), Some(f6)) if f6 > f1 => {
                let d = f6 - f1;
                (MIN_TTFT_MS..=MAX_TTFT_MS).contains(&d).then_some(d)
            }
            _ => None,
        };
        let total_ms = match (t.created_at_ms, t.completed_at_ms) {
            (Some(f1), Some(f7)) if f7 > f1 => Some(f7 - f1),
            _ => None,
        };
        let gen_ms = match (t.first_token_at_ms, t.completed_at_ms) {
            (Some(f6), Some(f7)) if f7 > f6 => Some(f7 - f6),
            _ => None,
        };

        let is_buffered_burst = total_ms
            .is_some_and(|tot| gen_ms.is_some_and(|g| g.saturating_mul(20) < tot && out < 400));
        let stream_tps = if !is_buffered_burst {
            gen_ms.and_then(|g| {
                compute_tps_checked(
                    out,
                    g,
                    MIN_STREAM_TPS_OUTPUT_TOKENS,
                    MIN_STREAM_TPS_DURATION_MS,
                )
            })
        } else {
            None
        };

        let (turn_tps, turn_dur) = if let Some(st) = stream_tps {
            let g = gen_ms.unwrap_or(0);
            reliable_tps = Some((st, out, g));
            (Some(st), gen_ms)
        } else {
            let fb = total_ms.and_then(|tot| {
                compute_tps_checked(
                    out,
                    tot,
                    MIN_FALLBACK_TPS_OUTPUT_TOKENS,
                    MIN_FALLBACK_TPS_DURATION_MS,
                )
            });
            if reliable_tps.is_none() {
                if let Some(f_tps) = fb {
                    let tot = total_ms.unwrap_or(0);
                    fallback_tps = Some((f_tps, out, tot));
                }
            }
            (fb, total_ms.or(gen_ms))
        };

        if let Some(ts_ms) = t.completed_at_ms.or(t.created_at_ms) {
            latest_ts = Some(latest_ts.map_or(ts_ms, |cur| cur.max(ts_ms)));
            if ttft_ms.is_some() || turn_tps.is_some() || out > 0 {
                push_turn_sample(
                    &mut recent_turns,
                    TurnMetricSample {
                        ts_ms,
                        ttft_ms,
                        tps: turn_tps,
                        output_tokens: (out > 0).then_some(out),
                        duration_ms: turn_dur,
                    },
                );
            }
        }
    }

    let base_ctx_used = base_transcript.as_ref().and_then(|b| b.context_used_tokens);
    let ctx_used = latest_db_ctx.or(base_ctx_used);
    let base_ctx_window = base_transcript
        .as_ref()
        .and_then(|b| b.context_window_tokens);
    let ctx_window =
        infer_context_window(AgentKind::Agy, model, ctx_used, None).or(base_ctx_window);
    let ctx_pct = match (ctx_used, ctx_window) {
        (Some(u), Some(w)) => pct_1dp(u, w),
        _ => base_transcript.as_ref().and_then(|b| b.context_used_pct),
    };
    let total_input_tokens = base_transcript
        .as_ref()
        .and_then(|b| b.total_input_tokens)
        .or((sum_in > 0).then_some(sum_in));
    let total_output_tokens = base_transcript
        .as_ref()
        .and_then(|b| b.total_output_tokens)
        .or((sum_out > 0).then_some(sum_out));

    let chosen_tps = reliable_tps.or(fallback_tps);
    let ttft_ms = recent_turns.iter().rev().find_map(|t| t.ttft_ms);
    let avg_ttft_ms = compute_avg_ttft_ms(&recent_turns);
    let updated_ms = latest_ts
        .or_else(|| base_transcript.as_ref().map(|b| b.updated_ms))
        .unwrap_or(now_ms);

    Some(SessionMetrics {
        context_used_tokens: ctx_used,
        context_window_tokens: ctx_window,
        context_used_pct: ctx_pct,
        tps: chosen_tps.map(|x| x.0),
        ttft_ms,
        avg_ttft_ms,
        last_output_tokens: chosen_tps.map(|x| x.1),
        last_duration_ms: chosen_tps.map(|x| x.2),
        total_input_tokens,
        total_output_tokens,
        recent_turns,
        source: "agy_db".into(),
        updated_ms,
    })
}

#[derive(Debug, Clone, Deserialize)]
struct AgyStepMetaRow {
    idx: i64,
    #[serde(default)]
    meta_hex: Option<String>,
}

#[derive(Debug, Clone)]
struct AgyStepCacheEntry {
    sig: DbSig,
    timings: Vec<AgyStepTiming>,
    last_used: u64,
}

/// Bounded LRU cache (128 entries) of decoded AGY `steps.metadata` step timings keyed by `conv_id`.
/// Re-queries SQLite ONLY when `<home>/.gemini/antigravity-cli/conversations/<conv_id>.db`
/// or its `-wal` `(mtime_ns, len)` signature changes.
#[derive(Debug, Default)]
pub(crate) struct AgyStepTimingCache {
    entries: HashMap<String, AgyStepCacheEntry>,
    access_seq: u64,
}

impl AgyStepTimingCache {
    pub(crate) fn get_or_refresh_in(
        &mut self,
        home: Option<&Path>,
        conv_id: &str,
    ) -> &[AgyStepTiming] {
        let Some(home) = home else {
            return &[];
        };
        let cid = conv_id.trim();
        if cid.is_empty() || cid.contains('/') || cid.contains("..") {
            return &[];
        }
        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{cid}.db"));
        self.refresh_from_path(cid, &db_path)
    }

    pub(crate) fn refresh_from_path(&mut self, conv_id: &str, db_path: &Path) -> &[AgyStepTiming] {
        let sig = db_signature(db_path);
        if sig.db.is_none() {
            self.entries.remove(conv_id);
            return &[];
        }
        self.access_seq = self.access_seq.wrapping_add(1);
        let seq = self.access_seq;
        if let Some(hit) = self.entries.get_mut(conv_id) {
            if hit.sig == sig {
                hit.last_used = seq;
                return &self.entries[conv_id].timings;
            }
        }

        let sql = "SELECT idx, hex(metadata) AS meta_hex FROM steps WHERE step_type = 15 AND status = 3 AND metadata IS NOT NULL ORDER BY idx DESC LIMIT 32;";
        let Some(json_out) = run_sqlite_json(db_path, sql) else {
            return self
                .entries
                .get(conv_id)
                .map(|e| e.timings.as_slice())
                .unwrap_or(&[]);
        };
        let Ok(rows) = serde_json::from_str::<Vec<AgyStepMetaRow>>(&json_out) else {
            return self
                .entries
                .get(conv_id)
                .map(|e| e.timings.as_slice())
                .unwrap_or(&[]);
        };
        let mut timings = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(hex_str) = row.meta_hex.as_deref() else {
                continue;
            };
            let Some(bytes) = decode_hex(hex_str) else {
                continue;
            };
            if let Some(mut t) = parse_agy_step_metadata_proto(&bytes) {
                t.idx = row.idx;
                timings.push(t);
            }
        }
        timings.sort_by_key(|t| t.idx);

        if !self.entries.contains_key(conv_id) && self.entries.len() >= MAX_AGY_STEP_CACHE_ENTRIES {
            if let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&victim);
            }
        }
        self.entries.insert(
            conv_id.to_string(),
            AgyStepCacheEntry {
                sig,
                timings,
                last_used: seq,
            },
        );
        self.entries
            .get(conv_id)
            .map(|e| e.timings.as_slice())
            .unwrap_or(&[])
    }
}

/// Known model context window lookup (strictly documented/observed windows; unknown models → `None`).
pub(crate) fn infer_context_window(
    agent: AgentKind,
    model: Option<&str>,
    ctx_used: Option<u64>,
    codex_default_window: Option<u64>,
) -> Option<u64> {
    let m = model.unwrap_or("").trim().to_ascii_lowercase();
    match agent {
        AgentKind::Claude => {
            if m.contains("1m") || ctx_used.is_some_and(|u| u > 200_000) {
                Some(1_000_000)
            } else if !m.is_empty() || ctx_used.is_some() {
                Some(200_000)
            } else {
                None
            }
        }
        AgentKind::Codex => codex_default_window,
        AgentKind::Agy => {
            if m.starts_with("gemini-4-")
                || m.contains("gemini-4-neon")
                || m.contains("gemini-4-carbon")
                || m.contains("gemini-4-argon")
                || m.contains("orchestrator")
                || m.contains("gemini-flash-worker")
            {
                if ctx_used.is_some_and(|u| u > 512_000) {
                    Some(1_000_000)
                } else {
                    Some(512_000)
                }
            } else if m.contains("claude") {
                if m.contains("1m") || ctx_used.is_some_and(|u| u > 200_000) {
                    Some(1_000_000)
                } else {
                    Some(200_000)
                }
            } else {
                None
            }
        }
        AgentKind::Shell => None,
    }
}

// ---------------------------------------------------------------------------
// Process birth-time (`ps -o etime=`) & transcript birth-time guards
// ---------------------------------------------------------------------------

/// Parse POSIX `ps -o etime=` elapsed wall-clock string (`[[dd-]hh:]mm:ss` or `ss`) into seconds.
/// Locale-independent and timezone-independent on both macOS (BSD `ps`) and Linux (`procps`).
pub(crate) fn parse_ps_etime_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (days, hms) = match trimmed.split_once('-') {
        Some((d, rest)) => (d.trim().parse::<u64>().ok()?, rest.trim()),
        None => (0u64, trimmed),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let hms_secs = match parts.as_slice() {
        [ss] => ss.parse::<u64>().ok()?,
        [mm, ss] => {
            let m = mm.parse::<u64>().ok()?;
            let s = ss.parse::<u64>().ok()?;
            if s >= 60 {
                return None;
            }
            m.checked_mul(60)?.checked_add(s)?
        }
        [hh, mm, ss] => {
            let h = hh.parse::<u64>().ok()?;
            let m = mm.parse::<u64>().ok()?;
            let s = ss.parse::<u64>().ok()?;
            if m >= 60 || s >= 60 {
                return None;
            }
            h.checked_mul(3600)?
                .checked_add(m.checked_mul(60)?)?
                .checked_add(s)?
        }
        _ => return None,
    };
    days.checked_mul(86_400)?.checked_add(hms_secs)
}

/// Pure parser for `LC_ALL=C ps -o pid=,etime= -p ...` stdout, mapping `pid -> birth_ms`.
pub(crate) fn parse_ps_pid_etimes(output: &str, now_ms: u64) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    for line in output.lines() {
        let mut cols = line.split_whitespace();
        let Some(pid) = cols.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(etime_raw) = cols.next() else {
            continue;
        };
        if let Some(elapsed_secs) = parse_ps_etime_secs(etime_raw) {
            out.insert(
                pid,
                now_ms.saturating_sub(elapsed_secs.saturating_mul(1000)),
            );
        }
    }
    out
}

/// Query OS process start timestamps (epoch ms) for `pids` via a single `LC_ALL=C ps -o pid=,etime=`.
pub(crate) fn query_pids_birth_ms(pids: &[u32], now_ms: u64) -> HashMap<u32, u64> {
    let mut uniq: Vec<u32> = pids.iter().copied().filter(|&p| p > 0).collect();
    if uniq.is_empty() {
        return HashMap::new();
    }
    uniq.sort_unstable();
    uniq.dedup();
    let pid_arg = uniq
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "pid=,etime=", "-p", &pid_arg])
        .output()
    else {
        return HashMap::new();
    };
    if out.stdout.is_empty() {
        return HashMap::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_ps_pid_etimes(&stdout, now_ms)
}

/// Normalize a SQLite/JSON integer timestamp (Unix seconds or milliseconds) into epoch ms.
pub(crate) fn epoch_i64_to_ms(raw: i64) -> Option<u64> {
    if raw <= 0 {
        return None;
    }
    let val = raw as u64;
    if val < 10_000_000_000 {
        Some(val.saturating_mul(1000))
    } else {
        Some(val)
    }
}

pub(crate) fn file_mtime_ms(path: &Path) -> Option<u64> {
    let (mtime_ns, _) = file_signature(path)?;
    (mtime_ns > 0).then_some((mtime_ns / 1_000_000) as u64)
}

/// True when `ts_ms` occurred at or after `min_birth_ms` (allowing `PROCESS_BIRTH_SLACK_MS`
/// for 1s `ps -o etime=` rounding).
pub(crate) fn is_post_birth_timestamp(ts_ms: u64, min_birth_ms: u64) -> bool {
    ts_ms.saturating_add(PROCESS_BIRTH_SLACK_MS) >= min_birth_ms
}

fn read_capped_header_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<bool> {
    buf.clear();
    let mut saw_any = false;
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            return Ok(saw_any);
        }
        saw_any = true;
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            if buf.len() < max_bytes {
                let take_copy = pos.min(max_bytes - buf.len());
                buf.extend_from_slice(&avail[..take_copy]);
            }
            reader.consume(pos + 1);
            return Ok(true);
        }
        let n = avail.len();
        if buf.len() < max_bytes {
            let take_copy = n.min(max_bytes - buf.len());
            buf.extend_from_slice(&avail[..take_copy]);
        }
        reader.consume(n);
    }
}

/// Extract the birth/creation timestamp (epoch ms) of a transcript JSONL file:
/// 1. Scans up to the first `MAX_BIRTH_HEADER_LINES` JSONL entries for `"created_at"` (AGY),
///    top-level `"timestamp"` (Claude/Codex), or `"payload.timestamp"` (Codex `session_meta`).
/// 2. Falls back to filesystem `created()` (birthtime) or `modified()` metadata if header lines
///    omit timestamps.
pub(crate) fn transcript_birth_ms(path: &Path) -> Option<u64> {
    if let Ok(file) = std::fs::File::open(path) {
        let mut reader = BufReader::with_capacity(8 * 1024, file);
        let mut line_buf = Vec::with_capacity(2048);
        let mut inspected = 0usize;
        while inspected < MAX_BIRTH_HEADER_LINES {
            match read_capped_header_line(&mut reader, &mut line_buf, MAX_BIRTH_HEADER_LINE_BYTES) {
                Ok(false) => break,
                Ok(true) => {
                    let s = String::from_utf8_lossy(&line_buf);
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    inspected += 1;
                    if !trimmed.starts_with('{') {
                        continue;
                    }
                    let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
                        continue;
                    };
                    let ts = obj
                        .get("created_at")
                        .and_then(Value::as_str)
                        .and_then(parse_iso8601_ms)
                        .or_else(|| {
                            obj.get("timestamp")
                                .and_then(Value::as_str)
                                .and_then(parse_iso8601_ms)
                        })
                        .or_else(|| {
                            obj.get("payload")
                                .and_then(|p| p.get("timestamp"))
                                .and_then(Value::as_str)
                                .and_then(parse_iso8601_ms)
                        });
                    if ts.is_some() {
                        return ts;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    meta.created()
        .ok()
        .or_else(|| meta.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .filter(|&ms| ms > 0)
}

fn same_canonical_or_norm_dir(a: &str, b: &str) -> bool {
    let na = a.trim().trim_end_matches('/');
    let nb = b.trim().trim_end_matches('/');
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
    match (std::fs::canonicalize(na), std::fs::canonicalize(nb)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Exact existing-pane session/transcript resolvers (Claude, Codex, AGY)
// ---------------------------------------------------------------------------

/// Exact Claude subtree PIDs → `(session_id, transcript_path)` via `~/.claude/sessions/<pid>.json`,
/// verified against OS PID birth timestamps so recycled PIDs never attach stale dead sessions.
pub(crate) fn resolve_claude_transcript_by_pids(pids: &[u32]) -> Option<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let births = query_pids_birth_ms(pids, starfane_proto::id::now_ms());
    let no_claimed = HashSet::new();
    resolve_claude_transcript_by_pids_in(&home, pids, &births, &no_claimed)
}

pub(crate) fn resolve_claude_transcript_by_pids_in(
    home: &Path,
    pids: &[u32],
    pid_births: &HashMap<u32, u64>,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    for &pid in pids {
        if let Some(hit) = resolve_claude_transcript_by_pid_verified(
            home,
            pid,
            pid_births.get(&pid).copied(),
            claimed_ids,
        ) {
            return Some(hit);
        }
    }
    None
}

#[cfg(test)]
fn resolve_claude_transcript_by_pid_in(home: &Path, pid: u32) -> Option<(String, PathBuf)> {
    let no_claimed = HashSet::new();
    resolve_claude_transcript_by_pid_verified(home, pid, None, &no_claimed)
}

fn resolve_claude_transcript_by_pid_verified(
    home: &Path,
    pid: u32,
    os_pid_birth_ms: Option<u64>,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    let sess_file = home.join(format!(".claude/sessions/{pid}.json"));
    let text = std::fs::read_to_string(&sess_file).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if let Some(recorded_pid) = v.get("pid").and_then(Value::as_u64) {
        if recorded_pid != u64::from(pid) {
            return None;
        }
    }
    let sid = v.get("sessionId").and_then(Value::as_str)?.trim();
    if sid.is_empty() || sid.contains('/') || sid.contains("..") || claimed_ids.contains(sid) {
        return None;
    }
    if let Some(os_birth_ms) = os_pid_birth_ms {
        if let Some(started_ms) = v
            .get("startedAt")
            .and_then(Value::as_i64)
            .and_then(epoch_i64_to_ms)
        {
            if started_ms.abs_diff(os_birth_ms) > CLAUDE_PID_START_MAX_SKEW_MS {
                return None;
            }
        } else if let Some(mtime_ms) = file_mtime_ms(&sess_file) {
            if mtime_ms.saturating_add(86_400_000) < os_birth_ms {
                return None;
            }
        }
    }
    let projects_dir = home.join(".claude/projects");
    if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
        let enc = encode_claude_project_dir(cwd);
        let direct = projects_dir.join(enc).join(format!("{sid}.jsonl"));
        if direct.is_file() {
            return Some((sid.to_string(), direct));
        }
    }
    let target_name = format!("{sid}.jsonl");
    let entries = std::fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let cand = entry.path().join(&target_name);
        if cand.is_file() {
            return Some((sid.to_string(), cand));
        }
    }
    None
}

/// Exact Codex resume UUID → rollout JSONL resolution under `<home>/.codex/sessions/YYYY/MM/DD/*<uuid>.jsonl`.
pub(crate) fn resolve_codex_transcript_by_id_opt_home(
    home: Option<&Path>,
    uuid: &str,
) -> Option<PathBuf> {
    resolve_codex_transcript_by_id_in(home?, uuid)
}

pub(crate) fn resolve_codex_transcript_by_id_in(home: &Path, uuid: &str) -> Option<PathBuf> {
    let uuid = uuid.trim();
    if uuid.len() < 16 || uuid.contains('/') || uuid.contains("..") {
        return None;
    }
    let suffix = format!("{uuid}.jsonl");
    let root = home.join(".codex/sessions");
    let years = std::fs::read_dir(&root).ok()?;
    for y in years.flatten() {
        let Ok(months) = std::fs::read_dir(y.path()) else {
            continue;
        };
        for m in months.flatten() {
            let Ok(days) = std::fs::read_dir(m.path()) else {
                continue;
            };
            for d in days.flatten() {
                let Ok(files) = std::fs::read_dir(d.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    if f.file_name().to_string_lossy().ends_with(&suffix) && f.path().is_file() {
                        return Some(f.path());
                    }
                }
            }
        }
    }
    None
}

/// Cached read-only snapshot of `~/.codex/state_5.sqlite` `threads` table.
#[derive(Debug, Clone, Deserialize)]
pub struct CodexThreadRow {
    pub id: String,
    #[serde(default)]
    pub rollout_path: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Default)]
pub(crate) struct CodexThreadCache {
    sig: Option<DbSig>,
    db_path: Option<PathBuf>,
    rows: Vec<CodexThreadRow>,
}

impl CodexThreadCache {
    pub(crate) fn refresh_if_needed_in(&mut self, home: Option<&Path>) {
        let Some(home) = home else {
            return;
        };
        let db_path = home.join(".codex/state_5.sqlite");
        let sig = db_signature(&db_path);
        if sig.db.is_none() {
            if self.db_path.as_deref() != Some(db_path.as_path()) {
                self.rows.clear();
                self.sig = None;
                self.db_path = Some(db_path);
            }
            return;
        }
        if self.db_path.as_deref() == Some(db_path.as_path()) && self.sig.as_ref() == Some(&sig) {
            return;
        }
        let sql = "SELECT id, rollout_path, cwd, name, COALESCE(archived, 0) AS archived, COALESCE(created_at, 0) AS created_at, COALESCE(updated_at, 0) AS updated_at FROM threads WHERE rollout_path IS NOT NULL AND rollout_path != '' ORDER BY archived ASC, updated_at DESC LIMIT 2000;";
        let Some(json_out) = run_sqlite_json(&db_path, sql) else {
            return;
        };
        let Ok(rows) = serde_json::from_str::<Vec<CodexThreadRow>>(&json_out) else {
            return;
        };
        self.rows = rows;
        self.sig = Some(sig);
        self.db_path = Some(db_path);
    }

    /// Match a Codex pane against `state_5.sqlite` by its explicit live bottom status-bar title +
    /// `cwd`, without requiring `unique_in_cwd` or `min_birth_ms` (supports multi-pane same-CWD
    /// layouts and interactive TUI `/resume` of older threads). Blank panes return `None`.
    pub(crate) fn match_pane_by_title(
        &self,
        cwd: &str,
        tail: &str,
        claimed_ids: &HashSet<String>,
    ) -> Option<(String, PathBuf)> {
        match_codex_thread_with_birth(&self.rows, cwd, tail, claimed_ids, None)
    }

    #[allow(dead_code)]
    pub(crate) fn match_pane_with_birth(
        &self,
        cwd: &str,
        tail: &str,
        claimed_ids: &HashSet<String>,
        min_birth_ms: Option<u64>,
        unique_in_cwd: bool,
    ) -> Option<(String, PathBuf)> {
        if !unique_in_cwd {
            return None;
        }
        match_codex_thread_with_birth(&self.rows, cwd, tail, claimed_ids, min_birth_ms)
    }
}

fn is_codex_model_token(tok: &str) -> bool {
    let t = tok.to_ascii_lowercase();
    let valid_chars = !t.is_empty()
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'/' | b':'));
    valid_chars
        && (t.starts_with("gpt-")
            || t.starts_with("codex-")
            || t.starts_with("chatgpt-")
            || t == "o1"
            || t.starts_with("o1-")
            || t == "o3"
            || t.starts_with("o3-")
            || t == "o4"
            || t.starts_with("o4-"))
}

fn normalize_codex_effort_token(tok: &str) -> Option<&'static str> {
    match tok.to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        "minimal" => Some("minimal"),
        _ => None,
    }
}

/// Locate the bottom-most active Codex status-bar line (e.g. `GPT-6.1-Sol high · ~/Work/foo · <title>`
/// or blank prompt `GPT-6.1-Sol low · ~/Work/foo`) within the last 8 non-empty lines of `tail`.
///
/// Stopping at the bottom-most status-bar line prevents stale status bars left in scrollback from an
/// exited previous Codex process from leaking titles or models into a freshly started blank prompt.
fn find_codex_status_bar_line(tail: &str) -> Option<&str> {
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let bottom = if lines.len() > 8 {
        &lines[lines.len() - 8..]
    } else {
        &lines[..]
    };
    for line in bottom.iter().rev() {
        if !line.contains(" · ") {
            continue;
        }
        let mut parts = line.split(" · ");
        let seg0 = parts.next().unwrap_or("").trim();
        let seg1 = parts.next().unwrap_or("").trim();
        let first_tok = seg0.split_whitespace().next().unwrap_or("");
        if is_codex_model_token(first_tok)
            || seg1.starts_with("~/")
            || (seg1.starts_with('/') && !seg0.starts_with('←') && !seg0.starts_with('?'))
        {
            return Some(line);
        }
    }
    None
}

/// Extract Codex bottom status-bar thread title (3rd ` · ` segment of the active model/path/title line).
/// Returns `None` immediately when the active bottom-most status bar is a blank prompt (`<model> · <cwd>`)
/// or only displays status chrome (`N% context left`, `← for agents`, `? for shortcuts`).
pub(crate) fn extract_codex_status_title(tail: &str) -> Option<String> {
    let line = find_codex_status_bar_line(tail)?;
    let parts: Vec<&str> = line.split(" · ").collect();
    if parts.len() < 3 {
        return None;
    }
    // Strip right-aligned status text separated by multiple spaces (e.g. `Goal stalled ...`).
    let raw = parts[2].split("  ").next().unwrap_or("").trim();
    let rl = raw.to_ascii_lowercase();
    if raw.is_empty()
        || rl.ends_with("% context left")
        || rl.ends_with("% left")
        || rl.starts_with("← for agents")
        || rl.starts_with("? for shortcuts")
        || rl == "main [default]"
    {
        return None;
    }
    Some(raw.to_string())
}

/// Parse Codex's active bottom status bar (`GPT-6.1-Sol high · ~/Work/foo · ...` or
/// `GPT-6.1-Sol low · ~/Work/foo`) into normalized `(model, effort)`.
/// Intended strictly as a fallback when verified transcript / CLI args / spawn metadata omit them.
pub fn parse_codex_status_line_model_effort(tail: &str) -> (Option<String>, Option<String>) {
    let Some(line) = find_codex_status_bar_line(tail) else {
        return (None, None);
    };
    let seg0 = line.split(" · ").next().unwrap_or("").trim();
    let toks: Vec<&str> = seg0.split_whitespace().collect();
    let Some(&first) = toks.first() else {
        return (None, None);
    };
    if !is_codex_model_token(first) {
        return (None, None);
    }
    let model = Some(first.to_ascii_lowercase());
    let effort = match (toks.get(1).copied(), toks.get(2).copied()) {
        (Some(t1), _) if normalize_codex_effort_token(t1).is_some() => {
            normalize_codex_effort_token(t1).map(str::to_string)
        }
        (Some(t1), Some(t2))
            if t1.eq_ignore_ascii_case("extra") && t2.eq_ignore_ascii_case("high") =>
        {
            Some("xhigh".to_string())
        }
        _ => None,
    };
    (model, effort)
}

/// Strictly match a Codex pane against `state_5.sqlite` threads using its status-line title + `cwd`,
/// skipping any thread id in `claimed_ids` and enforcing `min_birth_ms` + uniqueness in `cwd`.
/// Never guesses on blank/new panes (no title hint → `None`), never matches across unrelated `cwd`s,
/// and never returns an ambiguous multi-match.
#[cfg(test)]
pub(crate) fn match_codex_thread(
    rows: &[CodexThreadRow],
    cwd: &str,
    tail: &str,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    match_codex_thread_with_birth(rows, cwd, tail, claimed_ids, None)
}

pub(crate) fn match_codex_thread_with_birth(
    rows: &[CodexThreadRow],
    cwd: &str,
    tail: &str,
    claimed_ids: &HashSet<String>,
    min_birth_ms: Option<u64>,
) -> Option<(String, PathBuf)> {
    let hint = extract_codex_status_title(tail)?;
    let hint_clean = hint.trim_end_matches(['…', '.']).trim();
    if hint_clean.is_empty() {
        return None;
    }

    let resolve_row = |r: &CodexThreadRow| -> Option<(String, PathBuf)> {
        if claimed_ids.contains(r.id.trim()) {
            return None;
        }
        if !same_canonical_or_norm_dir(&r.cwd, cwd) {
            return None;
        }
        let p = PathBuf::from(r.rollout_path.as_deref()?);
        if !p.is_file() {
            return None;
        }
        if let Some(birth_ms) = min_birth_ms {
            let updated_ms = epoch_i64_to_ms(r.updated_at).or_else(|| file_mtime_ms(&p))?;
            if !is_post_birth_timestamp(updated_ms, birth_ms) {
                return None;
            }
            let created_ms = epoch_i64_to_ms(r.created_at).or_else(|| transcript_birth_ms(&p))?;
            if !is_post_birth_timestamp(created_ms, birth_ms) {
                return None;
            }
        }
        Some((r.id.clone(), p))
    };

    // Pass 1: exact `name == hint` AND matching `cwd` (must be unambiguous).
    let mut exact_hits = Vec::new();
    for r in rows {
        let nm = r.name.as_deref().unwrap_or("").trim();
        if !nm.is_empty() && nm == hint {
            if let Some(hit) = resolve_row(r) {
                exact_hits.push(hit);
                if exact_hits.len() > 1 {
                    return None;
                }
            }
        }
    }
    if exact_hits.len() == 1 {
        return exact_hits.pop();
    }

    // Pass 2: truncated title prefix match (`name.starts_with(hint_clean)`) ONLY when hint >= 8 chars
    // + matching `cwd`, and strictly unambiguous (`len() == 1`).
    if hint_clean.chars().count() >= 8 {
        let mut prefix_hits = Vec::new();
        for r in rows {
            let nm = r.name.as_deref().unwrap_or("").trim();
            if nm.starts_with(hint_clean) {
                if let Some(hit) = resolve_row(r) {
                    prefix_hits.push(hit);
                    if prefix_hits.len() > 1 {
                        return None;
                    }
                }
            }
        }
        if prefix_hits.len() == 1 {
            return prefix_hits.pop();
        }
    }
    None
}

/// Cached read-only snapshot of `~/.gemini/antigravity-cli/conversation_summaries.db` root conversations.
#[derive(Debug, Clone, Deserialize)]
pub struct AgyConvRow {
    pub conversation_id: String,
    #[serde(default)]
    pub workspace_uris: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub last_modified_time: String,
    #[serde(default)]
    pub last_user_input_time: String,
    #[serde(default)]
    pub step_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyChildRow {
    pub conversation_id: String,
    #[serde(default)]
    pub parent_conversation_id: String,
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub killed: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct AgyDiscoveredChild {
    pub conversation_id: String,
    pub agent_name: Option<String>,
    pub state: starfane_proto::SessionState,
    pub transcript_path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct AgyConvCache {
    sig: Option<DbSig>,
    db_path: Option<PathBuf>,
    rows: Vec<AgyConvRow>,
    children: Vec<AgyChildRow>,
}

impl AgyConvCache {
    pub(crate) fn refresh_if_needed_in(&mut self, home: Option<&Path>) {
        let Some(home) = home else {
            return;
        };
        let db_path = home.join(".gemini/antigravity-cli/conversation_summaries.db");
        let sig = db_signature(&db_path);
        if sig.db.is_none() {
            if self.db_path.as_deref() != Some(db_path.as_path()) {
                self.rows.clear();
                self.children.clear();
                self.sig = None;
                self.db_path = Some(db_path);
            }
            return;
        }
        if self.db_path.as_deref() == Some(db_path.as_path()) && self.sig.as_ref() == Some(&sig) {
            return;
        }
        let sql = "SELECT conversation_id, COALESCE(workspace_uris, '') AS workspace_uris, COALESCE(status, '') AS status, COALESCE(last_modified_time, '') AS last_modified_time, COALESCE(last_user_input_time, '') AS last_user_input_time, COALESCE(step_count, 0) AS step_count FROM conversation_summaries WHERE nesting_depth = 0 AND COALESCE(killed, 0) = 0 ORDER BY last_modified_time DESC LIMIT 120;";
        let Some(json_out) = run_sqlite_json(&db_path, sql) else {
            return;
        };
        let Ok(rows) = serde_json::from_str::<Vec<AgyConvRow>>(&json_out) else {
            return;
        };
        let child_sql = "SELECT conversation_id, COALESCE(parent_conversation_id, '') AS parent_conversation_id, COALESCE(agent_name, '') AS agent_name, COALESCE(status, '') AS status, COALESCE(killed, 0) AS killed FROM conversation_summaries WHERE nesting_depth = 1 AND parent_conversation_id != '' ORDER BY last_modified_time DESC LIMIT 120;";
        let children = run_sqlite_json(&db_path, child_sql)
            .and_then(|s| serde_json::from_str::<Vec<AgyChildRow>>(&s).ok())
            .unwrap_or_default();
        self.rows = rows;
        self.children = children;
        self.sig = Some(sig);
        self.db_path = Some(db_path);
    }

    pub(crate) fn is_known_root(&self, id: &str) -> bool {
        self.rows.iter().any(|r| r.conversation_id == id)
    }

    pub(crate) fn root_is_idle(&self, id: &str) -> bool {
        self.rows
            .iter()
            .find(|r| r.conversation_id == id)
            .is_some_and(|r| r.status.contains("IDLE") && !r.status.contains("RUNNING"))
    }

    /// Match an unlocked discovered AGY pane to an unclaimed root conversation ONLY when:
    /// 1. It is the sole unresolved AGY pane in `cwd` (`unique_in_cwd == true`),
    /// 2. The conversation was created AND modified at/after `min_birth_ms` (never attaching
    ///    historical conversations that predate the pane's agent process!), AND
    /// 3. Exactly ONE post-birth unclaimed conversation exists in `cwd` (zero ambiguity).
    pub(crate) fn match_unique_cwd_excluding_in(
        &self,
        home: Option<&Path>,
        cwd: &str,
        unique_in_cwd: bool,
        claimed_ids: &HashSet<String>,
        min_birth_ms: Option<u64>,
    ) -> Option<(String, PathBuf)> {
        if !unique_in_cwd {
            return None;
        }
        match_agy_conversation_with_birth_in(home?, &self.rows, cwd, claimed_ids, min_birth_ms)
    }

    /// Return Swarm / subagent child conversations whose `parent_conversation_id == parent_id`
    /// and whose `transcript.jsonl` exists on disk.
    pub(crate) fn children_of_in(
        &self,
        home: Option<&Path>,
        parent_id: &str,
    ) -> Vec<AgyDiscoveredChild> {
        let Some(home) = home else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in &self.children {
            if c.parent_conversation_id != parent_id {
                continue;
            }
            let sid = c.conversation_id.trim();
            if sid.is_empty() || sid.contains('/') || sid.contains("..") {
                continue;
            }
            let tr = home
                .join(".gemini/antigravity-cli/brain")
                .join(sid)
                .join(".system_generated/logs/transcript.jsonl");
            if !tr.is_file() {
                continue;
            }
            let state = if c.killed != 0 {
                starfane_proto::SessionState::Ended
            } else if c.status.contains("RUNNING") {
                starfane_proto::SessionState::Working
            } else {
                starfane_proto::SessionState::Idle
            };
            let agent_name = {
                let n = c.agent_name.trim();
                (!n.is_empty()).then(|| n.to_string())
            };
            out.push(AgyDiscoveredChild {
                conversation_id: sid.to_string(),
                agent_name,
                state,
                transcript_path: tr,
            });
        }
        out
    }
}

/// Resolve an AGY `conversation_id` to its `transcript.jsonl` (or `transcript_full.jsonl`) under `home`.
pub(crate) fn resolve_agy_transcript_by_id_in(home: &Path, conv_id: &str) -> Option<PathBuf> {
    let cid = conv_id.trim();
    if cid.is_empty() || cid.contains('/') || cid.contains("..") {
        return None;
    }
    let logs = home
        .join(".gemini/antigravity-cli/brain")
        .join(cid)
        .join(".system_generated/logs");
    let compact = logs.join("transcript.jsonl");
    if compact.is_file() {
        return Some(compact);
    }
    let full = logs.join("transcript_full.jsonl");
    full.is_file().then_some(full)
}

pub(crate) fn resolve_agy_transcript_by_id_opt_home(
    home: Option<&Path>,
    conv_id: &str,
) -> Option<PathBuf> {
    resolve_agy_transcript_by_id_in(home?, conv_id)
}

/// Pure parser for `lsof -n -P -Fpn -p <pids>` output: extracts all open
/// `~/.gemini/antigravity-cli/presence/<uuid>.lock` stems per PID (deduped in order).
pub(crate) fn parse_lsof_agy_presence_all(output: &str) -> HashMap<u32, Vec<String>> {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();
    let mut current_pid: Option<u32> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.trim().parse::<u32>().ok();
        } else if let Some(path_str) = line.strip_prefix('n') {
            let Some(pid) = current_pid else { continue };
            if !path_str.contains("/presence/") || !path_str.ends_with(".lock") {
                continue;
            }
            let Some(stem) = Path::new(path_str.trim())
                .file_stem()
                .and_then(|s| s.to_str())
            else {
                continue;
            };
            if stem.len() >= 16
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                let entry = map.entry(pid).or_default();
                if !entry.iter().any(|s| s == stem) {
                    entry.push(stem.to_string());
                }
            }
        }
    }
    map
}

/// First open `presence/<uuid>.lock` per PID (test helper).
#[cfg(test)]
pub(crate) fn parse_lsof_agy_presence(output: &str) -> HashMap<u32, String> {
    parse_lsof_agy_presence_all(output)
        .into_iter()
        .filter_map(|(pid, mut cids)| (!cids.is_empty()).then(|| (pid, cids.remove(0))))
        .collect()
}

/// Query open `presence/<uuid>.lock` descriptors for the given PIDs via a single fast `lsof`.
pub(crate) fn query_lsof_agy_presence_all(pids: &[u32]) -> HashMap<u32, Vec<String>> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let mut uniq: Vec<u32> = pids.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    let pid_arg = uniq
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = Command::new("lsof")
        .args(["-n", "-P", "-Fpn", "-p", &pid_arg])
        .output()
    else {
        return HashMap::new();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_lsof_agy_presence_all(&stdout)
}

const AGY_PRESENCE_TTL: Duration = Duration::from_secs(30);
const AGY_PRESENCE_MISS_TTL: Duration = Duration::from_secs(3);

/// Lightweight in-memory TTL cache for `pid -> AGY conversation_id` via `presence/<uuid>.lock`.
/// Caches resolved PIDs for 30s and misses for 3s so newly-started panes resolve promptly.
#[derive(Debug, Default)]
pub(crate) struct AgyPresenceCache {
    by_pid: HashMap<u32, (Vec<String>, Instant)>,
}

impl AgyPresenceCache {
    /// Return PIDs from `candidates` that are missing or expired in the cache.
    pub(crate) fn stale_pids(&self, candidates: &[u32], now: Instant) -> Vec<u32> {
        let mut stale = Vec::new();
        for &pid in candidates {
            let fresh = self.by_pid.get(&pid).is_some_and(|(cids, ts)| {
                let ttl = if cids.is_empty() {
                    AGY_PRESENCE_MISS_TTL
                } else {
                    AGY_PRESENCE_TTL
                };
                now.saturating_duration_since(*ts) < ttl
            });
            if !fresh {
                stale.push(pid);
            }
        }
        stale.sort_unstable();
        stale.dedup();
        stale
    }

    /// Merge single-lock `lsof` results for `queried_pids` into the cache (test helper).
    #[cfg(test)]
    pub(crate) fn update(
        &mut self,
        queried_pids: &[u32],
        found: &HashMap<u32, String>,
        now: Instant,
    ) {
        for &pid in queried_pids {
            let cids = found.get(&pid).cloned().into_iter().collect();
            self.by_pid.insert(pid, (cids, now));
        }
        self.prune(now);
    }

    /// Merge multi-lock `lsof` results for `queried_pids` into the cache.
    pub(crate) fn update_all(
        &mut self,
        queried_pids: &[u32],
        found: &HashMap<u32, Vec<String>>,
        now: Instant,
    ) {
        for &pid in queried_pids {
            let cids = found.get(&pid).cloned().unwrap_or_default();
            self.by_pid.insert(pid, (cids, now));
        }
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        if self.by_pid.len() > 256 {
            self.by_pid
                .retain(|_, (_, ts)| now.saturating_duration_since(*ts) < AGY_PRESENCE_TTL * 4);
        }
    }

    /// Look up the first matching `conversation_id` across a pane's `subtree_pids`.
    pub(crate) fn find_for_pids(&self, pids: &[u32]) -> Option<String> {
        for pid in pids {
            if let Some((cids, _)) = self.by_pid.get(pid) {
                if let Some(first) = cids.first() {
                    return Some(first.clone());
                }
            }
        }
        None
    }

    /// Prefer a lock whose UUID is a known `nesting_depth = 0` root conversation over a child lock.
    pub(crate) fn find_for_pids_preferring_roots(
        &self,
        pids: &[u32],
        roots: &AgyConvCache,
    ) -> Option<String> {
        for pid in pids {
            if let Some((cids, _)) = self.by_pid.get(pid) {
                if let Some(root_cid) = cids.iter().find(|cid| roots.is_known_root(cid)) {
                    return Some(root_cid.clone());
                }
            }
        }
        self.find_for_pids(pids)
    }
}

#[cfg(test)]
fn match_agy_conversation_in(
    home: &Path,
    rows: &[AgyConvRow],
    cwd: &str,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    match_agy_conversation_with_birth_in(home, rows, cwd, claimed_ids, None)
}

pub(crate) fn match_agy_conversation_with_birth_in(
    home: &Path,
    rows: &[AgyConvRow],
    cwd: &str,
    claimed_ids: &HashSet<String>,
    min_birth_ms: Option<u64>,
) -> Option<(String, PathBuf)> {
    let mut uri_candidates = vec![format!("file://{}", cwd.trim_end_matches('/'))];
    if let Ok(canon) = std::fs::canonicalize(cwd) {
        let canon_uri = format!("file://{}", canon.to_string_lossy().trim_end_matches('/'));
        if !uri_candidates.contains(&canon_uri) {
            uri_candidates.push(canon_uri);
        }
    }

    let mut matches: Vec<(String, PathBuf)> = Vec::new();
    for r in rows {
        let sid = r.conversation_id.trim();
        if sid.is_empty() || sid.contains('/') || sid.contains("..") || claimed_ids.contains(sid) {
            continue;
        }
        // Check exact URI match inside JSON array string to avoid `/Users/demo` matching `/Users/demo/work/app-b`.
        let uri_matched = uri_candidates.iter().any(|u| {
            let quoted = format!("\"{u}\"");
            let quoted_slash = format!("\"{u}/\"");
            r.workspace_uris.contains(&quoted) || r.workspace_uris.contains(&quoted_slash)
        });
        if !uri_matched {
            continue;
        }
        let Some(tr) = resolve_agy_transcript_by_id_in(home, sid) else {
            continue;
        };
        if let Some(birth_ms) = min_birth_ms {
            let Some((_, len)) = file_signature(&tr) else {
                continue;
            };
            if r.step_count <= 0 && len == 0 {
                continue;
            }
            if let Some(db_mod_ms) = parse_iso8601_ms(&r.last_modified_time) {
                if !is_post_birth_timestamp(db_mod_ms, birth_ms) {
                    continue;
                }
            }
            if let Some(db_in_ms) = parse_iso8601_ms(&r.last_user_input_time) {
                if !is_post_birth_timestamp(db_in_ms, birth_ms) {
                    continue;
                }
            }
            let Some(tr_birth_ms) = transcript_birth_ms(&tr) else {
                continue;
            };
            if !is_post_birth_timestamp(tr_birth_ms, birth_ms) {
                continue;
            }
            let Some(mtime_ms) = file_mtime_ms(&tr) else {
                continue;
            };
            if !is_post_birth_timestamp(mtime_ms, birth_ms) {
                continue;
            }
        }
        matches.push((sid.to_string(), tr));
        if matches.len() > 1 {
            return None;
        }
    }
    matches.pop()
}

/// Derive the canonical external `session_id` (used for AGY DB & claimed IDs) for `session`.
pub(crate) fn canonical_external_session_id(session: &SessionRecord) -> Option<String> {
    if !session.discovered_only && !session.session_id.as_str().starts_with("tmux-") {
        return Some(session.session_id.0.clone());
    }
    let tp = session.transcript_path.as_deref()?;
    extract_session_id_from_transcript_path(session.agent, Path::new(tp))
}

pub(crate) fn extract_session_id_from_transcript_path(
    agent: AgentKind,
    path: &Path,
) -> Option<String> {
    match agent {
        AgentKind::Claude => {
            let stem = path.file_stem()?.to_str()?;
            (!stem.is_empty()).then(|| stem.to_string())
        }
        AgentKind::Codex => {
            let stem = path.file_stem()?.to_str()?;
            // Filename format: `rollout-2026-11-21T12-11-53-<36-char-uuid>`.
            if stem.len() >= 36 {
                let tail = &stem[stem.len() - 36..];
                if tail.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                    return Some(tail.to_string());
                }
            }
            None
        }
        AgentKind::Agy => {
            // Path format: `.../brain/<uuid>/.system_generated/logs/transcript.jsonl`
            let mut comps = path.components().peekable();
            while let Some(c) = comps.next() {
                if c.as_os_str() == "brain" {
                    return comps
                        .next()
                        .and_then(|id| id.as_os_str().to_str())
                        .map(str::to_string);
                }
            }
            None
        }
        AgentKind::Shell => None,
    }
}

pub(crate) fn encode_claude_project_dir(path: &str) -> String {
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

// ---------------------------------------------------------------------------
// Single-pass incremental `TelemetryFold` (shared by TranscriptIndex & metrics)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ClaudeMsgStats {
    req_start_ts: Option<u64>,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    block_count: u32,
    has_thinking: bool,
    first_had_stop_reason: bool,
    context_tokens: u64,
    output_tokens: u64,
    model: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ClaudeFold {
    order: Vec<String>,
    by_msg: HashMap<String, ClaudeMsgStats>,
    prev_user_ts: Option<u64>,
    latest_event_ts: Option<u64>,
    latest_model: Option<String>,
    seq: u64,
}

#[derive(Debug, Clone, Default)]
struct CodexFold {
    ctx_used: Option<u64>,
    ctx_window: Option<u64>,
    total_in: Option<u64>,
    total_out: Option<u64>,
    req_start_ts: Option<u64>,
    first_item_ts: Option<u64>,
    saw_model_item: bool,
    saw_reasoning_item: bool,
    closed_via_usage_record: bool,
    latest_event_ts: Option<u64>,
    reliable_tps: Option<(f32, u64, u64)>,
    fallback_tps: Option<(f32, u64, u64)>,
    latest_ttft_ms: Option<u64>,
    recent_turns: Vec<TurnMetricSample>,
    prev_total_out: Option<u64>,
    latest_model: Option<String>,
    latest_effort: Option<String>,
}

impl CodexFold {
    fn record_turn(&mut self, ts_ms: Option<u64>, out_tok: u64, reasoning_tok: u64) {
        let ttft_ms = match (self.req_start_ts, self.first_item_ts) {
            (Some(r), Some(fi)) if fi > r => {
                let d = fi - r;
                (MIN_TTFT_MS..=MAX_TTFT_MS).contains(&d).then_some(d)
            }
            _ => None,
        };
        if let Some(t) = ttft_ms {
            self.latest_ttft_ms = Some(t);
        }
        let gen_ms = match (self.first_item_ts, ts_ms) {
            (Some(fi), Some(t)) if t > fi => Some(t - fi),
            _ => None,
        };
        let tot_ms = match (self.req_start_ts, ts_ms) {
            (Some(r), Some(t)) if t > r => Some(t - r),
            _ => None,
        };

        let stream_res = if reasoning_tok > 0 && self.saw_reasoning_item {
            let vis = out_tok.saturating_sub(reasoning_tok);
            gen_ms.and_then(|g| {
                compute_tps_checked(
                    vis,
                    g,
                    MIN_CODEX_STREAM_VISIBLE_TOKENS,
                    MIN_STREAM_TPS_DURATION_MS,
                )
                .map(|tps| (tps, vis, g))
            })
        } else if reasoning_tok == 0 {
            gen_ms.and_then(|g| {
                compute_tps_checked(
                    out_tok,
                    g,
                    MIN_STREAM_TPS_OUTPUT_TOKENS,
                    MIN_STREAM_TPS_DURATION_MS,
                )
                .map(|tps| (tps, out_tok, g))
            })
        } else {
            None
        };

        let fb_res = tot_ms.and_then(|tot| {
            compute_tps_checked(
                out_tok,
                tot,
                MIN_FALLBACK_TPS_OUTPUT_TOKENS,
                MIN_FALLBACK_TPS_DURATION_MS,
            )
            .map(|tps| (tps, out_tok, tot))
        });

        let (turn_tps, turn_out, turn_dur) = if let Some((tps, tok, dur)) = stream_res {
            self.reliable_tps = Some((tps, tok, dur));
            (Some(tps), Some(tok), Some(dur))
        } else {
            if self.reliable_tps.is_none() {
                if let Some((tps, tok, dur)) = fb_res {
                    self.fallback_tps = Some((tps, tok, dur));
                }
            }
            (
                fb_res.map(|x| x.0),
                (out_tok > 0).then_some(out_tok),
                tot_ms.or(gen_ms),
            )
        };

        if let Some(t) = ts_ms {
            if ttft_ms.is_some() || turn_tps.is_some() || out_tok > 0 {
                push_turn_sample(
                    &mut self.recent_turns,
                    TurnMetricSample {
                        ts_ms: t,
                        ttft_ms,
                        tps: turn_tps,
                        output_tokens: turn_out,
                        duration_ms: turn_dur,
                    },
                );
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AgyFold {
    latest_ctx: Option<u64>,
    latest_model: Option<String>,
    total_in: u64,
    total_out: u64,
    last_tps: Option<(f32, u64, u64)>,
    recent_turns: Vec<TurnMetricSample>,
    pending_pr: Option<(u64, u64)>,
    latest_event_ts: Option<u64>,
}

/// Single-pass, append-only telemetry accumulator for JSONL transcripts.
/// Eliminates redundant second scans of JSONL files for metrics/model/effort extraction.
#[derive(Debug, Clone)]
pub(crate) struct TelemetryFold {
    agent: AgentKind,
    claude: ClaudeFold,
    codex: CodexFold,
    agy: AgyFold,
}

impl TelemetryFold {
    pub(crate) fn new(agent: AgentKind) -> Self {
        Self {
            agent,
            claude: ClaudeFold::default(),
            codex: CodexFold::default(),
            agy: AgyFold::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn feed_text(&mut self, text: &str) {
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
                self.feed_obj(&obj);
            }
        }
    }

    pub(crate) fn feed_obj(&mut self, obj: &Value) {
        match self.agent {
            AgentKind::Claude => self.feed_claude(obj),
            AgentKind::Codex => self.feed_codex(obj),
            AgentKind::Agy => self.feed_agy(obj),
            AgentKind::Shell => {}
        }
    }

    fn feed_claude(&mut self, obj: &Value) {
        let f = &mut self.claude;
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            f.latest_event_ts = Some(f.latest_event_ts.map_or(t, |cur| cur.max(t)));
        }

        if kind == "user" {
            if let Some(t) = ts_ms {
                f.prev_user_ts = Some(t);
            }
            return;
        }

        if kind == "assistant" {
            if let Some(msg) = obj.get("message") {
                let model = msg
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                if !model.is_empty() {
                    f.latest_model = Some(model.to_string());
                }
                if model != "<synthetic>" {
                    if let Some(u) = msg.get("usage") {
                        let inp = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                        let cache_r = u
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cache_w = u
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let out = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                        let ctx = inp.saturating_add(cache_r).saturating_add(cache_w);
                        if ctx > 0 || out > 0 {
                            let chunk_has_thinking = msg
                                .get("content")
                                .and_then(Value::as_array)
                                .is_some_and(|blocks| {
                                    blocks.iter().any(|b| {
                                        matches!(
                                            b.get("type").and_then(Value::as_str),
                                            Some("thinking" | "redacted_thinking")
                                        )
                                    })
                                });
                            let first_had_stop = msg
                                .get("stop_reason")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.trim().is_empty());
                            let id = msg
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    f.seq += 1;
                                    format!("anon-{}", f.seq)
                                });
                            let entry = f.by_msg.entry(id.clone()).or_insert_with(|| {
                                f.order.push(id);
                                ClaudeMsgStats {
                                    req_start_ts: f.prev_user_ts,
                                    first_ts: ts_ms,
                                    last_ts: ts_ms,
                                    block_count: 0,
                                    has_thinking: false,
                                    first_had_stop_reason: first_had_stop,
                                    context_tokens: 0,
                                    output_tokens: 0,
                                    model: (!model.is_empty()).then(|| model.to_string()),
                                }
                            });
                            entry.block_count = entry.block_count.saturating_add(1);
                            entry.has_thinking |= chunk_has_thinking;
                            if entry.first_ts.is_none() {
                                entry.first_ts = ts_ms;
                            }
                            if let Some(t) = ts_ms {
                                entry.last_ts = Some(entry.last_ts.map_or(t, |cur| cur.max(t)));
                            }
                            entry.context_tokens = entry.context_tokens.max(ctx);
                            entry.output_tokens = entry.output_tokens.max(out);
                        }
                    }
                }
            }
        }
    }

    fn feed_codex(&mut self, obj: &Value) {
        let f = &mut self.codex;
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            f.latest_event_ts = Some(f.latest_event_ts.map_or(t, |cur| cur.max(t)));
        }
        let payload = obj.get("payload");
        if kind == "turn_context" {
            if let Some(p) = payload {
                if let Some(m) = p
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    f.latest_model = Some(m.to_string());
                }
                let eff = p
                    .get("effort")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        p.get("collaboration_mode")
                            .and_then(|c| c.get("settings"))
                            .and_then(|s| s.get("reasoning_effort"))
                            .and_then(Value::as_str)
                    })
                    .or_else(|| p.get("reasoning_effort").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if let Some(e) = eff {
                    f.latest_effort = Some(e.to_ascii_lowercase());
                }
            }
            return;
        }
        let ptype = payload
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");

        match (kind, ptype) {
            ("response_item", "message") => {
                if payload.and_then(|p| p.get("role")).and_then(Value::as_str) == Some("user") {
                    if let Some(t) = ts_ms {
                        f.req_start_ts = Some(t);
                    }
                    f.first_item_ts = None;
                    f.saw_model_item = false;
                    f.saw_reasoning_item = false;
                    f.closed_via_usage_record = false;
                } else {
                    if let Some(t) = ts_ms {
                        f.first_item_ts = f.first_item_ts.or(Some(t));
                    }
                    f.saw_model_item = true;
                }
            }
            ("response_item", "custom_tool_call_output" | "function_call_output") => {
                if !f.saw_model_item {
                    if let Some(t) = ts_ms {
                        f.req_start_ts = Some(t);
                    }
                    f.first_item_ts = None;
                    f.saw_model_item = false;
                    f.saw_reasoning_item = false;
                }
            }
            ("response_item", "reasoning" | "custom_tool_call" | "function_call") => {
                if let Some(t) = ts_ms {
                    f.first_item_ts = f.first_item_ts.or(Some(t));
                }
                f.saw_model_item = true;
                if ptype == "reasoning" {
                    f.saw_reasoning_item = true;
                }
            }
            ("event_msg", "item_completed") => {
                if let Some(item_type) = payload
                    .and_then(|p| p.get("item"))
                    .and_then(|it| it.get("type"))
                    .and_then(Value::as_str)
                {
                    if matches!(item_type, "Reasoning" | "AgentMessage" | "Message") {
                        if let Some(t) = ts_ms {
                            f.first_item_ts = f.first_item_ts.or(Some(t));
                        }
                        f.saw_model_item = true;
                        if item_type == "Reasoning" {
                            f.saw_reasoning_item = true;
                        }
                    }
                }
            }
            ("token_usage_record", _) | ("event_msg", "token_usage_record") => {
                let usage = payload.and_then(|p| p.get("usage")).or(payload);
                let out_tok = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let reasoning_tok = usage
                    .and_then(|u| u.get("reasoning_output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                f.record_turn(ts_ms, out_tok, reasoning_tok);
                if let Some(t) = ts_ms {
                    f.req_start_ts = Some(t);
                }
                f.first_item_ts = None;
                f.saw_model_item = false;
                f.saw_reasoning_item = false;
                f.closed_via_usage_record = true;
            }
            ("event_msg", "token_count") => {
                let Some(info) = payload
                    .and_then(|p| p.get("info"))
                    .filter(|v| v.is_object())
                else {
                    return;
                };
                if let Some(win) = info.get("model_context_window").and_then(Value::as_u64) {
                    if win > 0 {
                        f.ctx_window = Some(win);
                    }
                }
                if let Some(tot) = info.get("total_token_usage") {
                    if let Some(v) = tot.get("input_tokens").and_then(Value::as_u64) {
                        f.total_in = Some(v);
                    }
                    if let Some(v) = tot.get("output_tokens").and_then(Value::as_u64) {
                        f.total_out = Some(v);
                    }
                }
                if let Some(last) = info.get("last_token_usage") {
                    let used = last
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .or_else(|| last.get("total_tokens").and_then(Value::as_u64));
                    if let Some(u) = used.filter(|&u| u > 0) {
                        f.ctx_used = Some(u);
                    }
                    let out_tok = last
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let reasoning_tok = last
                        .get("reasoning_output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let changed = f.total_out != f.prev_total_out;
                    let was_closed = f.closed_via_usage_record;
                    f.closed_via_usage_record = false;
                    if changed && !was_closed && out_tok > 0 {
                        f.record_turn(ts_ms, out_tok, reasoning_tok);
                    }
                    f.prev_total_out = f.total_out;
                    if f.saw_model_item || (changed && !was_closed) {
                        if let Some(t) = ts_ms {
                            f.req_start_ts = Some(t);
                        }
                        f.first_item_ts = None;
                        f.saw_model_item = false;
                        f.saw_reasoning_item = false;
                    }
                }
            }
            ("event_msg", "task_complete")
                if f.reliable_tps.is_none() && f.fallback_tps.is_none() =>
            {
                if let (Some(dur), Some(out_tok)) = (
                    payload
                        .and_then(|p| p.get("duration_ms"))
                        .and_then(Value::as_u64),
                    f.total_out.filter(|&v| v > 0),
                ) {
                    if let Some(tps) = compute_tps_checked(
                        out_tok,
                        dur,
                        MIN_FALLBACK_TPS_OUTPUT_TOKENS,
                        MIN_FALLBACK_TPS_DURATION_MS,
                    ) {
                        f.fallback_tps = Some((tps, out_tok, dur));
                        if let Some(t) = ts_ms {
                            push_turn_sample(
                                &mut f.recent_turns,
                                TurnMetricSample {
                                    ts_ms: t,
                                    ttft_ms: None,
                                    tps: Some(tps),
                                    output_tokens: Some(out_tok),
                                    duration_ms: Some(dur),
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn feed_agy(&mut self, obj: &Value) {
        let f = &mut self.agy;
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            f.latest_event_ts = Some(f.latest_event_ts.map_or(t, |cur| cur.max(t)));
        }

        if let (Some((pr_start, pr_out)), Some(next_ts)) = (f.pending_pr.take(), ts_ms) {
            if !matches!(kind, "USER_INPUT" | "SYSTEM_MESSAGE" | "PLANNER_RESPONSE") {
                let dur = next_ts.saturating_sub(pr_start);
                if dur >= 3_000 {
                    if let Some(tps) =
                        compute_tps_checked(pr_out, dur, MIN_STREAM_TPS_OUTPUT_TOKENS, 3_000)
                    {
                        f.last_tps = Some((tps, pr_out, dur));
                        push_turn_sample(
                            &mut f.recent_turns,
                            TurnMetricSample {
                                ts_ms: next_ts,
                                ttft_ms: None,
                                tps: Some(tps),
                                output_tokens: Some(pr_out),
                                duration_ms: Some(dur),
                            },
                        );
                    }
                }
            }
        }

        if kind == "PLANNER_RESPONSE" {
            if let Some(m) = obj
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                f.latest_model = Some(m.to_string());
            }
            let inp = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let cache_r = obj
                .get("cache_read_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let out = obj
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let ctx = inp.saturating_add(cache_r);
            if ctx > 0 {
                f.latest_ctx = Some(ctx);
                f.total_in = f.total_in.saturating_add(ctx);
            }
            if out > 0 {
                f.total_out = f.total_out.saturating_add(out);
                if let Some(t) = ts_ms {
                    f.pending_pr = Some((t, out));
                }
            }
        }
    }

    pub(crate) fn snapshot(
        &self,
        now_ms: u64,
    ) -> (Option<SessionMetrics>, Option<String>, Option<String>) {
        match self.agent {
            AgentKind::Claude => {
                let metrics = self.snapshot_claude_metrics(now_ms);
                (metrics, self.claude.latest_model.clone(), None)
            }
            AgentKind::Codex => {
                let metrics = self.snapshot_codex_metrics(now_ms);
                (
                    metrics,
                    self.codex.latest_model.clone(),
                    self.codex.latest_effort.clone(),
                )
            }
            AgentKind::Agy => {
                let metrics = self.snapshot_agy_metrics(now_ms);
                let model = self.agy.latest_model.clone();
                let effort = model.as_deref().and_then(extract_agy_effort_from_model);
                (metrics, model, effort)
            }
            AgentKind::Shell => (None, None, None),
        }
    }

    fn snapshot_claude_metrics(&self, now_ms: u64) -> Option<SessionMetrics> {
        let f = &self.claude;
        if f.order.is_empty() {
            return None;
        }
        let mut total_in: u64 = 0;
        let mut total_out: u64 = 0;
        let mut latest_ctx: Option<u64> = None;
        let mut latest_model: Option<String> = None;
        let mut reliable_tps: Option<(f32, u64, u64)> = None;
        let mut fallback_tps: Option<(f32, u64, u64)> = None;
        let mut recent_turns: Vec<TurnMetricSample> = Vec::new();

        for id in &f.order {
            let Some(m) = f.by_msg.get(id) else { continue };
            total_in = total_in.saturating_add(m.context_tokens);
            total_out = total_out.saturating_add(m.output_tokens);
            if m.context_tokens > 0 {
                latest_ctx = Some(m.context_tokens);
                if m.model.is_some() {
                    latest_model = m.model.clone();
                }
            }

            let ttft_ms = if m.block_count >= 2 || !m.first_had_stop_reason || m.output_tokens < 50
            {
                match (m.req_start_ts, m.first_ts) {
                    (Some(r), Some(fi)) if fi > r => {
                        let d = fi - r;
                        (MIN_TTFT_MS..=MAX_TTFT_MS).contains(&d).then_some(d)
                    }
                    _ => None,
                }
            } else {
                None
            };

            let chunk_span = match (m.first_ts, m.last_ts) {
                (Some(a), Some(b)) if b > a => b - a,
                _ => 0,
            };
            let call_span = match (m.req_start_ts, m.last_ts) {
                (Some(a), Some(b)) if b > a => b - a,
                _ => 0,
            };
            let dur = if (MIN_FALLBACK_TPS_DURATION_MS..=MAX_TPS_DURATION_MS).contains(&call_span) {
                call_span
            } else if !m.has_thinking {
                chunk_span
            } else {
                0
            };

            let stream_tps = compute_tps_checked(
                m.output_tokens,
                dur,
                MIN_STREAM_TPS_OUTPUT_TOKENS,
                MIN_STREAM_TPS_DURATION_MS,
            );
            let fb_tps = if stream_tps.is_none() {
                compute_tps_checked(
                    m.output_tokens,
                    dur,
                    MIN_FALLBACK_TPS_OUTPUT_TOKENS,
                    MIN_FALLBACK_TPS_DURATION_MS,
                )
            } else {
                None
            };

            if let Some(tps) = stream_tps {
                reliable_tps = Some((tps, m.output_tokens, dur));
            } else if reliable_tps.is_none() {
                if let Some(tps) = fb_tps {
                    fallback_tps = Some((tps, m.output_tokens, dur));
                }
            }

            let turn_tps = stream_tps.or(fb_tps);
            if let Some(ts) = m.last_ts.or(m.first_ts) {
                if ttft_ms.is_some() || turn_tps.is_some() || m.output_tokens > 0 {
                    push_turn_sample(
                        &mut recent_turns,
                        TurnMetricSample {
                            ts_ms: ts,
                            ttft_ms,
                            tps: turn_tps,
                            output_tokens: (m.output_tokens > 0).then_some(m.output_tokens),
                            duration_ms: (dur > 0).then_some(dur),
                        },
                    );
                }
            }
        }

        let ctx_window =
            infer_context_window(AgentKind::Claude, latest_model.as_deref(), latest_ctx, None);
        let ctx_pct = match (latest_ctx, ctx_window) {
            (Some(u), Some(w)) => pct_1dp(u, w),
            _ => None,
        };
        let chosen_tps = reliable_tps.or(fallback_tps);
        let ttft_ms = recent_turns.iter().rev().find_map(|t| t.ttft_ms);
        let avg_ttft_ms = compute_avg_ttft_ms(&recent_turns);

        Some(SessionMetrics {
            context_used_tokens: latest_ctx,
            context_window_tokens: ctx_window,
            context_used_pct: ctx_pct,
            tps: chosen_tps.map(|x| x.0),
            ttft_ms,
            avg_ttft_ms,
            last_output_tokens: chosen_tps.map(|x| x.1),
            last_duration_ms: chosen_tps.map(|x| x.2),
            total_input_tokens: (total_in > 0).then_some(total_in),
            total_output_tokens: (total_out > 0).then_some(total_out),
            recent_turns,
            source: "transcript".into(),
            updated_ms: f.latest_event_ts.unwrap_or(now_ms),
        })
    }

    fn snapshot_codex_metrics(&self, now_ms: u64) -> Option<SessionMetrics> {
        let f = &self.codex;
        if f.ctx_used.is_none()
            && f.ctx_window.is_none()
            && f.total_in.is_none()
            && f.reliable_tps.is_none()
            && f.fallback_tps.is_none()
            && f.recent_turns.is_empty()
        {
            return None;
        }
        let ctx_pct = match (f.ctx_used, f.ctx_window) {
            (Some(u), Some(w)) => pct_1dp(u, w),
            _ => None,
        };
        let chosen_tps = f.reliable_tps.or(f.fallback_tps);
        let ttft_ms = f
            .latest_ttft_ms
            .or_else(|| f.recent_turns.iter().rev().find_map(|t| t.ttft_ms));
        let avg_ttft_ms = compute_avg_ttft_ms(&f.recent_turns);
        Some(SessionMetrics {
            context_used_tokens: f.ctx_used,
            context_window_tokens: f.ctx_window,
            context_used_pct: ctx_pct,
            tps: chosen_tps.map(|x| x.0),
            ttft_ms,
            avg_ttft_ms,
            last_output_tokens: chosen_tps.map(|x| x.1),
            last_duration_ms: chosen_tps.map(|x| x.2),
            total_input_tokens: f.total_in,
            total_output_tokens: f.total_out,
            recent_turns: f.recent_turns.clone(),
            source: "transcript".into(),
            updated_ms: f.latest_event_ts.unwrap_or(now_ms),
        })
    }

    fn snapshot_agy_metrics(&self, now_ms: u64) -> Option<SessionMetrics> {
        let f = &self.agy;
        if f.latest_ctx.is_none() && f.total_out == 0 && f.last_tps.is_none() {
            return None;
        }
        let ctx_window = infer_context_window(
            AgentKind::Agy,
            f.latest_model.as_deref(),
            f.latest_ctx,
            None,
        );
        let ctx_pct = match (f.latest_ctx, ctx_window) {
            (Some(u), Some(w)) => pct_1dp(u, w),
            _ => None,
        };
        Some(SessionMetrics {
            context_used_tokens: f.latest_ctx,
            context_window_tokens: ctx_window,
            context_used_pct: ctx_pct,
            tps: f.last_tps.map(|x| x.0),
            ttft_ms: None,
            avg_ttft_ms: None,
            last_output_tokens: f.last_tps.map(|x| x.1),
            last_duration_ms: f.last_tps.map(|x| x.2),
            total_input_tokens: (f.total_in > 0).then_some(f.total_in),
            total_output_tokens: (f.total_out > 0).then_some(f.total_out),
            recent_turns: f.recent_turns.clone(),
            source: "transcript".into(),
            updated_ms: f.latest_event_ts.unwrap_or(now_ms),
        })
    }
}

// ---------------------------------------------------------------------------
// Core JSONL metrics extraction (pure, zero IO)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn extract_transcript_metrics(
    agent: AgentKind,
    text: &str,
    now_ms: u64,
) -> Option<SessionMetrics> {
    let mut fold = TelemetryFold::new(agent);
    fold.feed_text(text);
    fold.snapshot(now_ms).0
}

fn pct_1dp(used: u64, window: u64) -> Option<f32> {
    if window == 0 {
        return None;
    }
    let raw = ((used as f64) / (window as f64) * 100.0).clamp(0.0, 100.0);
    if !raw.is_finite() {
        return None;
    }
    Some(((raw * 10.0).round() / 10.0) as f32)
}

fn round_1dp(v: f32) -> Option<f32> {
    if !v.is_finite() {
        return None;
    }
    Some((v * 10.0).round() / 10.0)
}

// ---------------------------------------------------------------------------
// Status line fallback parser (Claude / Codex)
// ---------------------------------------------------------------------------

/// Parse context-window percentage and explicit `(1M context)`/`[1M]` tags from the
/// bottom lines of a `capture-pane` snapshot. Never invents values.
pub fn parse_status_line_metrics(
    agent: AgentKind,
    tail: &str,
    now_ms: u64,
) -> Option<SessionMetrics> {
    if !matches!(agent, AgentKind::Claude | AgentKind::Codex) {
        return None;
    }
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let bottom = if lines.len() > 6 {
        &lines[lines.len() - 6..]
    } else {
        &lines[..]
    };

    let mut used_pct: Option<f32> = None;
    let mut window_tokens: Option<u64> = None;

    for line in bottom.iter().rev() {
        let lower = line.to_ascii_lowercase();
        if window_tokens.is_none() && (lower.contains("(1m context)") || lower.contains("[1m]")) {
            window_tokens = Some(1_000_000);
        }
        if used_pct.is_none() {
            if let Some(left) = extract_percent_left(&lower) {
                used_pct = round_1dp((100.0 - left).clamp(0.0, 100.0));
            }
        }
    }

    if used_pct.is_none() && window_tokens.is_none() {
        return None;
    }

    Some(SessionMetrics {
        context_used_tokens: None,
        context_window_tokens: window_tokens,
        context_used_pct: used_pct,
        tps: None,
        ttft_ms: None,
        avg_ttft_ms: None,
        last_output_tokens: None,
        last_duration_ms: None,
        total_input_tokens: None,
        total_output_tokens: None,
        recent_turns: Vec::new(),
        source: "status_line".into(),
        updated_ms: now_ms,
    })
}

fn extract_percent_left(lower: &str) -> Option<f32> {
    for needle in ["% context left", "% left"] {
        if let Some(pos) = lower.find(needle) {
            let prefix = lower[..pos].trim_end();
            let start = prefix
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(0, |i| i + 1);
            let num_str = &prefix[start..];
            if let Ok(v) = num_str.parse::<f32>() {
                if (0.0..=100.0).contains(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Merge transcript metrics (`"agy_db"` / `"transcript"`) with optional status-line metrics.
pub(crate) fn merge_metrics(
    transcript: Option<SessionMetrics>,
    status: Option<SessionMetrics>,
) -> Option<SessionMetrics> {
    match (transcript, status) {
        (Some(mut tr), Some(st)) => {
            if let Some(w) = st.context_window_tokens {
                if tr.context_window_tokens.is_none() || w > tr.context_window_tokens.unwrap_or(0) {
                    tr.context_window_tokens = Some(w);
                }
            }
            if let (Some(u), Some(w)) = (tr.context_used_tokens, tr.context_window_tokens) {
                tr.context_used_pct = pct_1dp(u, w);
            } else if tr.context_used_pct.is_none() {
                tr.context_used_pct = st.context_used_pct;
            }
            Some(tr)
        }
        (Some(tr), None) => Some(tr),
        (None, Some(st)) => Some(st),
        (None, None) => None,
    }
}

/// Compare two `Option<SessionMetrics>` ignoring `updated_ms` to avoid noisy SSE republishes.
pub(crate) fn metrics_meaningfully_equal(
    a: &Option<SessionMetrics>,
    b: &Option<SessionMetrics>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.context_used_tokens == y.context_used_tokens
                && x.context_window_tokens == y.context_window_tokens
                && x.context_used_pct == y.context_used_pct
                && x.tps == y.tps
                && x.ttft_ms == y.ttft_ms
                && x.avg_ttft_ms == y.avg_ttft_ms
                && x.last_output_tokens == y.last_output_tokens
                && x.last_duration_ms == y.last_duration_ms
                && x.total_input_tokens == y.total_input_tokens
                && x.total_output_tokens == y.total_output_tokens
                && x.recent_turns.len() == y.recent_turns.len()
                && x.recent_turns.last() == y.recent_turns.last()
                && x.source == y.source
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Remote zero-install SSH pure parsers (zero local disk IO)
// ---------------------------------------------------------------------------

/// Normalize a POSIX directory path string without touching the local filesystem.
fn norm_remote_dir(dir: &str) -> &str {
    let trimmed = dir.trim();
    if trimmed == "/" {
        "/"
    } else {
        trimmed.trim_end_matches('/')
    }
}

fn remote_dir_matches(candidate: &str, remote_cwd: &str, remote_canon_cwd: Option<&str>) -> bool {
    let nc = norm_remote_dir(candidate);
    if nc.is_empty() {
        return false;
    }
    let nw = norm_remote_dir(remote_cwd);
    if !nw.is_empty() && nc == nw {
        return true;
    }
    if let Some(canon) = remote_canon_cwd
        .map(norm_remote_dir)
        .filter(|s| !s.is_empty())
    {
        if nc == canon {
            return true;
        }
    }
    false
}

/// Parsed remote Claude `~/.claude/sessions/<pid>.json` entry (zero local disk IO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClaudeSessionFile {
    pub pid: u32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub started_at_ms: Option<u64>,
}

/// Pure parser for remote Claude `~/.claude/sessions/<pid>.json` payloads.
/// Rejects malformed JSON, mismatched `expected_pid`, or invalid/path-traversal `sessionId`.
pub fn parse_claude_pid_session_json(
    json_text: &str,
    expected_pid: Option<u32>,
) -> Option<RemoteClaudeSessionFile> {
    let v: Value = serde_json::from_str(json_text.trim()).ok()?;
    let recorded_pid = v
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    let pid = match (recorded_pid, expected_pid) {
        (Some(rec), Some(exp)) if rec != exp => return None,
        (Some(rec), _) => rec,
        (None, Some(exp)) => exp,
        (None, None) => return None,
    };
    let sid = v.get("sessionId").and_then(Value::as_str)?.trim();
    if sid.is_empty()
        || sid.contains('/')
        || sid.contains('\\')
        || sid.contains("..")
        || sid.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
    {
        return None;
    }
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let started_at_ms = v
        .get("startedAt")
        .and_then(Value::as_i64)
        .and_then(epoch_i64_to_ms)
        .or_else(|| {
            v.get("startedAt")
                .and_then(Value::as_str)
                .and_then(parse_iso8601_ms)
        });
    Some(RemoteClaudeSessionFile {
        pid,
        session_id: sid.to_string(),
        cwd,
        started_at_ms,
    })
}

/// Verify that a parsed remote Claude `<pid>.json` is neither already claimed nor a stale
/// leftover from a recycled remote OS PID (evaluated strictly in the remote host clock domain).
pub fn verify_remote_claude_pid_session(
    entry: &RemoteClaudeSessionFile,
    os_pid_birth_ms: Option<u64>,
    file_mtime_ms: Option<u64>,
    claimed_ids: &HashSet<String>,
) -> bool {
    if claimed_ids.contains(entry.session_id.as_str()) {
        return false;
    }
    if let Some(os_birth_ms) = os_pid_birth_ms {
        if let Some(started_ms) = entry.started_at_ms {
            if started_ms.abs_diff(os_birth_ms) > CLAUDE_PID_START_MAX_SKEW_MS {
                return false;
            }
        } else if let Some(mtime_ms) = file_mtime_ms {
            if mtime_ms.saturating_add(86_400_000) < os_birth_ms {
                return false;
            }
        }
    }
    true
}

/// Build the canonical remote Claude transcript path `<remote_home>/.claude/projects/<enc>/<sid>.jsonl`.
pub fn claude_remote_transcript_path(remote_home: &str, cwd: &str, session_id: &str) -> String {
    let home = norm_remote_dir(remote_home);
    let enc = encode_claude_project_dir(cwd);
    format!("{home}/.claude/projects/{enc}/{session_id}.jsonl")
}

/// Parsed Codex rollout JSONL first-line `session_meta` header (`{id, timestamp, cwd}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub id: String,
    pub timestamp_ms: Option<u64>,
    pub cwd: Option<String>,
}

/// Pure parser for a Codex rollout JSONL first line (`type == "session_meta"`).
/// Extracts `{id, timestamp, cwd}` from `payload` (with fallback to top-level fields) without touching local disk.
pub fn parse_codex_session_meta_line(line: &str) -> Option<CodexSessionMeta> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let obj: Value = serde_json::from_str(trimmed).ok()?;
    if obj.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = obj.get("payload").unwrap_or(&obj);
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| obj.get("id").and_then(Value::as_str))?
        .trim();
    if id.is_empty()
        || id.contains('/')
        || id.contains("..")
        || id.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
    {
        return None;
    }
    let parse_ts_val = |v: &Value| -> Option<u64> {
        v.as_str()
            .and_then(parse_iso8601_ms)
            .or_else(|| v.as_i64().and_then(epoch_i64_to_ms))
    };
    let timestamp_ms = payload
        .get("timestamp")
        .and_then(parse_ts_val)
        .or_else(|| obj.get("timestamp").and_then(parse_ts_val));
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| obj.get("cwd").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(CodexSessionMeta {
        id: id.to_string(),
        timestamp_ms,
        cwd,
    })
}

/// Pure remote Codex rollout candidate (built from zero-`sqlite3` POSIX `session_meta` headers + `sf_stat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCodexRolloutCandidate {
    pub id: String,
    pub remote_path: String,
    pub cwd: String,
    pub birth_ms: Option<u64>,
    pub mtime_ms: Option<u64>,
}

/// Strictly match a remote Codex pane against zero-`sqlite3` POSIX `session_meta` rollout candidates
/// under `unique_in_cwd && !claimed && post_birth` rules (zero local disk IO).
/// Returns `Some((id, remote_path))` ONLY when exactly ONE candidate matches.
pub fn match_remote_codex_rollouts(
    candidates: &[RemoteCodexRolloutCandidate],
    remote_cwd: &str,
    remote_canon_cwd: Option<&str>,
    claimed_ids: &HashSet<String>,
    min_birth_ms: Option<u64>,
    unique_in_cwd: bool,
) -> Option<(String, String)> {
    if !unique_in_cwd {
        return None;
    }
    let mut matches: Vec<(String, String)> = Vec::new();
    for cand in candidates {
        let id = cand.id.trim();
        let path = cand.remote_path.trim();
        if id.is_empty()
            || path.is_empty()
            || id.contains('/')
            || id.contains("..")
            || claimed_ids.contains(id)
        {
            continue;
        }
        if !remote_dir_matches(&cand.cwd, remote_cwd, remote_canon_cwd) {
            continue;
        }
        if let Some(birth_ms) = min_birth_ms {
            let Some(cand_birth) = cand.birth_ms else {
                continue;
            };
            if !is_post_birth_timestamp(cand_birth, birth_ms) {
                continue;
            }
            if let Some(mtime_ms) = cand.mtime_ms {
                if !is_post_birth_timestamp(mtime_ms, birth_ms) {
                    continue;
                }
            }
        }
        matches.push((id.to_string(), path.to_string()));
        if matches.len() > 1 {
            return None;
        }
    }
    matches.pop()
}

/// Pure parser for Codex `state_5.sqlite` `threads` rows emitted as JSON (`sqlite3 -json`).
pub fn parse_codex_thread_rows_json(json_text: &str) -> Vec<CodexThreadRow> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<CodexThreadRow>>(trimmed).unwrap_or_default()
}

/// Pure parser for Codex `state_5.sqlite` `threads` rows emitted as TSV or unit-separated (`\x1f`) lines.
/// Supports both 6-column (`id, rollout_path, cwd, name, created_at, updated_at`) and
/// 7-column (`id, rollout_path, cwd, name, archived, created_at, updated_at`) projections.
pub fn parse_codex_thread_rows_delimited(text: &str, sep: char) -> Vec<CodexThreadRow> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(sep).collect();
        if cols.len() < 3 {
            continue;
        }
        let id = cols[0].trim();
        let rollout = cols[1].trim();
        let cwd = cols[2].trim();
        if id.is_empty() || rollout.is_empty() || id.contains('/') || id.contains("..") {
            continue;
        }
        let name = cols
            .get(3)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (created_at, updated_at) = if cols.len() >= 7 {
            (
                cols[5].trim().parse::<i64>().unwrap_or(0),
                cols[6].trim().parse::<i64>().unwrap_or(0),
            )
        } else {
            (
                cols.get(4)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(0),
                cols.get(5)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(0),
            )
        };
        out.push(CodexThreadRow {
            id: id.to_string(),
            rollout_path: Some(rollout.to_string()),
            cwd: cwd.to_string(),
            name,
            created_at,
            updated_at,
        });
    }
    out
}

/// Pure remote version of `match_codex_thread_with_birth` (zero local disk IO).
/// Matches status-bar title + remote `cwd`, enforcing `unique_in_cwd`, `!claimed_ids`, and
/// remote-clock `min_birth_ms` without calling local `std::fs::canonicalize` or `Path::is_file`.
pub fn match_remote_codex_threads(
    rows: &[CodexThreadRow],
    remote_cwd: &str,
    remote_canon_cwd: Option<&str>,
    tail: &str,
    claimed_ids: &HashSet<String>,
    min_birth_ms: Option<u64>,
    unique_in_cwd: bool,
) -> Option<(String, String)> {
    if !unique_in_cwd {
        return None;
    }
    let hint = extract_codex_status_title(tail)?;
    let hint_clean = hint.trim_end_matches(['…', '.']).trim();
    if hint_clean.is_empty() {
        return None;
    }

    let resolve_row = |r: &CodexThreadRow| -> Option<(String, String)> {
        let id = r.id.trim();
        if id.is_empty() || id.contains('/') || id.contains("..") || claimed_ids.contains(id) {
            return None;
        }
        if !remote_dir_matches(&r.cwd, remote_cwd, remote_canon_cwd) {
            return None;
        }
        let path = r.rollout_path.as_deref()?.trim();
        if path.is_empty() {
            return None;
        }
        if let Some(birth_ms) = min_birth_ms {
            let updated_ms = epoch_i64_to_ms(r.updated_at)?;
            if !is_post_birth_timestamp(updated_ms, birth_ms) {
                return None;
            }
            let created_ms = epoch_i64_to_ms(r.created_at)?;
            if !is_post_birth_timestamp(created_ms, birth_ms) {
                return None;
            }
        }
        Some((id.to_string(), path.to_string()))
    };

    // Pass 1: exact `name == hint` AND matching `cwd` (strictly unambiguous).
    let mut exact_hits = Vec::new();
    for r in rows {
        let nm = r.name.as_deref().unwrap_or("").trim();
        if !nm.is_empty() && nm == hint {
            if let Some(hit) = resolve_row(r) {
                exact_hits.push(hit);
                if exact_hits.len() > 1 {
                    return None;
                }
            }
        }
    }
    if exact_hits.len() == 1 {
        return exact_hits.pop();
    }

    // Pass 2: truncated title prefix match (`name.starts_with(hint_clean)`) when hint >= 8 chars.
    if hint_clean.chars().count() >= 8 {
        let mut prefix_hits = Vec::new();
        for r in rows {
            let nm = r.name.as_deref().unwrap_or("").trim();
            if nm.starts_with(hint_clean) {
                if let Some(hit) = resolve_row(r) {
                    prefix_hits.push(hit);
                    if prefix_hits.len() > 1 {
                        return None;
                    }
                }
            }
        }
        if prefix_hits.len() == 1 {
            return prefix_hits.pop();
        }
    }
    None
}

/// Pure parser for AGY `conversation_summaries` root rows emitted as JSON (`sqlite3 -json`).
pub fn parse_agy_conv_rows_json(json_text: &str) -> Vec<AgyConvRow> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<AgyConvRow>>(trimmed).unwrap_or_default()
}

/// Pure parser for AGY `conversation_summaries` root rows emitted as TSV (`'\t'`) or
/// unit-separated (`'\x1f'`) delimited lines:
/// `conversation_id <sep> workspace_uris <sep> status <sep> last_modified_time <sep> last_user_input_time <sep> step_count`
pub fn parse_agy_conv_rows_delimited(text: &str, sep: char) -> Vec<AgyConvRow> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(sep).collect();
        let cid = cols.first().map(|s| s.trim()).unwrap_or("");
        if cid.is_empty() || cid.contains('/') || cid.contains("..") {
            continue;
        }
        out.push(AgyConvRow {
            conversation_id: cid.to_string(),
            workspace_uris: cols.get(1).map(|s| s.trim()).unwrap_or("").to_string(),
            status: cols.get(2).map(|s| s.trim()).unwrap_or("").to_string(),
            last_modified_time: cols.get(3).map(|s| s.trim()).unwrap_or("").to_string(),
            last_user_input_time: cols.get(4).map(|s| s.trim()).unwrap_or("").to_string(),
            step_count: cols
                .get(5)
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(0),
        });
    }
    out
}

/// Pure parser for AGY `conversation_summaries` `nesting_depth = 1` child rows emitted as JSON.
pub fn parse_agy_child_rows_json(json_text: &str) -> Vec<AgyChildRow> {
    let trimmed = json_text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<AgyChildRow>>(trimmed).unwrap_or_default()
}

/// Pure parser for AGY `conversation_summaries` `nesting_depth = 1` child rows emitted as TSV/delimited lines:
/// `conversation_id <sep> parent_conversation_id <sep> agent_name <sep> status <sep> killed`
pub fn parse_agy_child_rows_delimited(text: &str, sep: char) -> Vec<AgyChildRow> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(sep).collect();
        let cid = cols.first().map(|s| s.trim()).unwrap_or("");
        let parent = cols.get(1).map(|s| s.trim()).unwrap_or("");
        if cid.is_empty() || parent.is_empty() || cid.contains('/') || cid.contains("..") {
            continue;
        }
        out.push(AgyChildRow {
            conversation_id: cid.to_string(),
            parent_conversation_id: parent.to_string(),
            agent_name: cols.get(2).map(|s| s.trim()).unwrap_or("").to_string(),
            status: cols.get(3).map(|s| s.trim()).unwrap_or("").to_string(),
            killed: cols
                .get(4)
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(0),
        });
    }
    out
}

fn extract_agy_presence_stem(path_str: &str) -> Option<String> {
    let p = path_str.trim();
    if !p.contains("/presence/") || !p.ends_with(".lock") {
        return None;
    }
    let stem = Path::new(p).file_stem()?.to_str()?;
    if stem.len() >= 16
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Some(stem.to_string())
    } else {
        None
    }
}

/// Pure parser for remote AGY `presence/<uuid>.lock` output from EITHER:
/// - Linux `/proc/$pid/fd` (`ls -l /proc/<pid>/fd`, `<pid>\t<path>`, `<pid> ... -> <path>`, or `/proc/<pid>/fd:` headers), OR
/// - macOS/BSD `lsof -n -P -Fpn` (`p<pid>` / `n<path>`).
pub fn parse_proc_fd_agy_presence(output: &str) -> HashMap<u32, Vec<String>> {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();
    let mut current_pid: Option<u32> = None;

    let mut push_lock = |pid: u32, path_str: &str| {
        if let Some(stem) = extract_agy_presence_stem(path_str) {
            let entry = map.entry(pid).or_default();
            if !entry.iter().any(|s| s == &stem) {
                entry.push(stem);
            }
        }
    };

    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // `lsof -Fpn` lines: `p<pid>` / `n<path>`
        if let Some(rest) = line.strip_prefix('p') {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                current_pid = Some(pid);
                continue;
            }
        }
        if let Some(path_str) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                push_lock(pid, path_str);
                continue;
            }
        }
        // Header lines like `/proc/12345/fd:` or `12345:`
        if let Some(hdr) = line.strip_suffix(':') {
            if let Some(after_proc) = hdr.strip_prefix("/proc/") {
                if let Some(pid_part) = after_proc.split('/').next() {
                    if let Ok(pid) = pid_part.parse::<u32>() {
                        current_pid = Some(pid);
                        continue;
                    }
                }
            } else if let Ok(pid) = hdr.trim().parse::<u32>() {
                current_pid = Some(pid);
                continue;
            }
        }
        // Tab or space delimited `<pid> <path>` or `<pid> ... -> <target>`
        if let Some((first, rest)) = line.split_once(['\t', ' ']) {
            if let Ok(pid) = first.trim().parse::<u32>() {
                let target = rest
                    .rsplit_once("->")
                    .map(|(_, rhs)| rhs.trim())
                    .unwrap_or_else(|| rest.trim());
                push_lock(pid, target);
                continue;
            }
        }
        // `ls -l` symlink line under a `/proc/<pid>/fd:` header: `lrwx... 12 -> /path/.../presence/<uuid>.lock`
        if let Some((_, rhs)) = line.rsplit_once("->") {
            if let Some(pid) = current_pid {
                push_lock(pid, rhs.trim());
            }
        }
    }

    map
}

/// Build the canonical remote AGY compact transcript path
/// `<remote_home>/.gemini/antigravity-cli/brain/<conv_id>/.system_generated/logs/transcript.jsonl`.
pub fn agy_remote_transcript_path(remote_home: &str, conv_id: &str) -> String {
    let home = norm_remote_dir(remote_home);
    format!(
        "{home}/.gemini/antigravity-cli/brain/{conv_id}/.system_generated/logs/transcript.jsonl"
    )
}

/// Pure remote version of `match_agy_conversation_with_birth_in` (zero local disk IO).
/// Enforces `unique_in_cwd`, `!claimed_ids`, exact `workspace_uris` match, and remote-clock
/// `min_birth_ms` (against DB timestamps + `step_count > 0`). Returns `Some((conv_id, remote_path))`
/// ONLY when strictly unambiguous (`len() == 1`).
pub fn match_remote_agy_conversation(
    remote_home: &str,
    rows: &[AgyConvRow],
    remote_cwd: &str,
    remote_canon_cwd: Option<&str>,
    claimed_ids: &HashSet<String>,
    min_birth_ms: Option<u64>,
    unique_in_cwd: bool,
) -> Option<(String, String)> {
    if !unique_in_cwd {
        return None;
    }
    let mut uri_candidates = Vec::with_capacity(2);
    let nw = norm_remote_dir(remote_cwd);
    if !nw.is_empty() {
        uri_candidates.push(format!("file://{nw}"));
    }
    if let Some(canon) = remote_canon_cwd
        .map(norm_remote_dir)
        .filter(|s| !s.is_empty())
    {
        let canon_uri = format!("file://{canon}");
        if !uri_candidates.contains(&canon_uri) {
            uri_candidates.push(canon_uri);
        }
    }
    if uri_candidates.is_empty() {
        return None;
    }

    let mut matches: Vec<(String, String)> = Vec::new();
    for r in rows {
        let sid = r.conversation_id.trim();
        if sid.is_empty() || sid.contains('/') || sid.contains("..") || claimed_ids.contains(sid) {
            continue;
        }
        let uri_matched = uri_candidates.iter().any(|u| {
            let quoted = format!("\"{u}\"");
            let quoted_slash = format!("\"{u}/\"");
            r.workspace_uris.contains(&quoted) || r.workspace_uris.contains(&quoted_slash)
        });
        if !uri_matched {
            continue;
        }
        if let Some(birth_ms) = min_birth_ms {
            if r.step_count <= 0 {
                continue;
            }
            let Some(db_mod_ms) = parse_iso8601_ms(&r.last_modified_time) else {
                continue;
            };
            if !is_post_birth_timestamp(db_mod_ms, birth_ms) {
                continue;
            }
            if let Some(db_in_ms) = parse_iso8601_ms(&r.last_user_input_time) {
                if !is_post_birth_timestamp(db_in_ms, birth_ms) {
                    continue;
                }
            }
        }
        matches.push((
            sid.to_string(),
            agy_remote_transcript_path(remote_home, sid),
        ));
        if matches.len() > 1 {
            return None;
        }
    }
    matches.pop()
}

/// Discovered remote AGY Swarm / subagent child conversation (zero local disk IO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgyChild {
    pub conversation_id: String,
    pub agent_name: Option<String>,
    pub state: starfane_proto::SessionState,
    pub remote_transcript_path: String,
}

/// Convert remote `AgyChildRow` entries matching `parent_id` into `RemoteAgyChild` descriptors.
pub fn remote_agy_children_of(
    remote_home: &str,
    children: &[AgyChildRow],
    parent_id: &str,
) -> Vec<RemoteAgyChild> {
    let mut out = Vec::new();
    for c in children {
        if c.parent_conversation_id != parent_id {
            continue;
        }
        let sid = c.conversation_id.trim();
        if sid.is_empty() || sid.contains('/') || sid.contains("..") {
            continue;
        }
        let state = if c.killed != 0 {
            starfane_proto::SessionState::Ended
        } else if c.status.contains("RUNNING") {
            starfane_proto::SessionState::Working
        } else {
            starfane_proto::SessionState::Idle
        };
        let agent_name = {
            let n = c.agent_name.trim();
            (!n.is_empty()).then(|| n.to_string())
        };
        out.push(RemoteAgyChild {
            conversation_id: sid.to_string(),
            agent_name,
            state,
            remote_transcript_path: agy_remote_transcript_path(remote_home, sid),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn encode_len_field(field: u64, payload: &[u8], out: &mut Vec<u8>) {
        encode_varint((field << 3) | 2, out);
        encode_varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn encode_varint_field(field: u64, val: u64, out: &mut Vec<u8>) {
        encode_varint(field << 3, out);
        encode_varint(val, out);
    }

    fn encode_ts_msg(ms: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_varint_field(1, ms / 1_000, &mut buf);
        encode_varint_field(2, (ms % 1_000) * 1_000_000, &mut buf);
        buf
    }

    fn encode_agy_step_meta(t: &AgyStepTiming) -> Vec<u8> {
        let mut buf = Vec::new();
        // Put an unknown field first to verify field-order independence.
        encode_varint_field(3, 2, &mut buf);
        if let Some(ms) = t.created_at_ms {
            encode_len_field(1, &encode_ts_msg(ms), &mut buf);
        }
        if let Some(ms) = t.first_token_at_ms {
            encode_len_field(6, &encode_ts_msg(ms), &mut buf);
        }
        if let Some(ms) = t.completed_at_ms {
            encode_len_field(7, &encode_ts_msg(ms), &mut buf);
        }
        let mut usage = Vec::new();
        encode_varint_field(1, 1669, &mut usage);
        encode_varint_field(2, t.input_tokens, &mut usage);
        encode_varint_field(3, t.output_tokens, &mut usage);
        encode_varint_field(5, t.cache_read_tokens, &mut usage);
        encode_len_field(7, b"bot-1234", &mut usage);
        encode_varint_field(9, t.thinking_tokens, &mut usage);
        encode_varint_field(10, t.visible_tokens, &mut usage);
        encode_len_field(9, &usage, &mut buf);
        encode_len_field(35, b"deadbeef", &mut buf);
        buf
    }

    fn bytes_to_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    #[test]
    fn agy_protobuf_decode_burst_filter_ttft_avg_and_recent_turns_cap() {
        let sample = AgyStepTiming {
            idx: 10,
            created_at_ms: Some(1_795_685_617_123),
            first_token_at_ms: Some(1_795_685_623_581),
            completed_at_ms: Some(1_795_685_626_075),
            input_tokens: 4_088,
            output_tokens: 613,
            cache_read_tokens: 339_112,
            thinking_tokens: 274,
            visible_tokens: 339,
        };
        let raw = encode_agy_step_meta(&sample);
        let decoded = parse_agy_step_metadata_proto(&raw).unwrap();
        assert_eq!(decoded.created_at_ms, Some(1_795_685_617_123));
        assert_eq!(decoded.first_token_at_ms, Some(1_795_685_623_581));
        assert_eq!(decoded.completed_at_ms, Some(1_795_685_626_075));
        assert_eq!(decoded.input_tokens, 4_088);
        assert_eq!(decoded.output_tokens, 613);
        assert_eq!(decoded.cache_read_tokens, 339_112);
        assert_eq!(decoded.thinking_tokens, 274);
        assert_eq!(decoded.visible_tokens, 339);

        // Corrupt / empty protobuf payloads fail gracefully.
        assert!(parse_agy_step_metadata_proto(&[]).is_none());
        assert!(parse_agy_step_metadata_proto(&[0x0a, 0xff, 0x01]).is_none());

        // Test buffered-burst rejection + reliable TPS preservation + TTFT average + 20-turn cap.
        let mut timings = Vec::new();
        let base_ts = 1_795_600_000_000u64;
        // 23 normal turns (indices 0..23): TTFT = 1_000ms + i*100ms, streaming TPS = 100.0 (400 tok / 4000ms).
        for i in 0..23u64 {
            let f1 = base_ts + i * 20_000;
            let ttft = 1_000 + i * 100;
            let f6 = f1 + ttft;
            let f7 = f6 + 4_000;
            timings.push(AgyStepTiming {
                idx: (23 - i) as i64, // intentionally unsorted input to test ascending idx sort
                created_at_ms: Some(f1),
                first_token_at_ms: Some(f6),
                completed_at_ms: Some(f7),
                input_tokens: 3_000,
                output_tokens: 400,
                cache_read_tokens: 170_000,
                thinking_tokens: 100,
                visible_tokens: 300,
            });
        }
        // Reverse idx mapping so idx 100 is latest, and insert a buffered-burst turn at idx 200:
        // total_ms = 52_000, gen_ms = 2_000 (2_000*20=40_000 < 52_000), out = 300 < 400 -> burst!
        for (pos, t) in timings.iter_mut().enumerate() {
            t.idx = pos as i64;
        }
        let burst_f1 = base_ts + 23 * 20_000;
        let burst_f6 = burst_f1 + 50_000;
        let burst_f7 = burst_f6 + 2_000;
        timings.push(AgyStepTiming {
            idx: 23,
            created_at_ms: Some(burst_f1),
            first_token_at_ms: Some(burst_f6),
            completed_at_ms: Some(burst_f7),
            input_tokens: 4_000,
            output_tokens: 300,
            cache_read_tokens: 180_000,
            thinking_tokens: 50,
            visible_tokens: 250,
        });

        let base_tr = SessionMetrics {
            context_used_tokens: Some(150_000),
            context_window_tokens: Some(512_000),
            context_used_pct: Some(29.3),
            tps: Some(50.0),
            ttft_ms: None,
            avg_ttft_ms: None,
            last_output_tokens: Some(200),
            last_duration_ms: Some(4_000),
            total_input_tokens: Some(2_500_000),
            total_output_tokens: Some(18_000),
            recent_turns: Vec::new(),
            source: "transcript".into(),
            updated_ms: 123,
        };
        let m =
            agy_step_timings_to_metrics(&timings, Some("gemini-4-neon-high"), Some(base_tr), 999)
                .unwrap();
        assert_eq!(m.source, "agy_db");
        assert_eq!(m.context_used_tokens, Some(184_000));
        assert_eq!(m.context_window_tokens, Some(512_000));
        assert_eq!(m.total_input_tokens, Some(2_500_000));
        assert_eq!(m.total_output_tokens, Some(18_000));
        // Reliable streaming TPS (100.0) must NOT be replaced by buffered burst (150.0 or 5.8)!
        assert_eq!(m.tps, Some(100.0));
        // Latest valid TTFT comes from the burst step (50_000ms) and is retained in recent_turns.
        assert_eq!(m.ttft_ms, Some(50_000));
        // Capped at 20 turns in chronological order (indices 4..=23).
        assert_eq!(m.recent_turns.len(), 20);
        assert!(m.recent_turns.windows(2).all(|w| w[0].ts_ms <= w[1].ts_ms));
        // Average TTFT over indices 4..=22 (1400..=3200, avg 2300, 19 turns) + index 23 (50000):
        // (19*2300 + 50000) / 20 = 93700 / 20 = 4685.
        assert_eq!(m.avg_ttft_ms, Some(4_685));

        let hex_blob = bytes_to_hex(&raw);
        let tsv_parsed = parse_agy_step_timing_tsv(&format!("42\t{hex_blob}\nbad-line\n"));
        assert_eq!(tsv_parsed.len(), 1);
        assert_eq!(tsv_parsed[0].idx, 42);
        assert_eq!(tsv_parsed[0].output_tokens, 613);

        // Verify metrics_meaningfully_equal covers avg_ttft_ms + recent_turns length/last.
        let mut m1_eq = m.clone();
        assert!(metrics_meaningfully_equal(
            &Some(m.clone()),
            &Some(m1_eq.clone())
        ));
        m1_eq.avg_ttft_ms = Some(999);
        assert!(!metrics_meaningfully_equal(
            &Some(m.clone()),
            &Some(m1_eq.clone())
        ));
        m1_eq.avg_ttft_ms = m.avg_ttft_ms;
        m1_eq.recent_turns.last_mut().unwrap().ttft_ms = Some(1_111);
        assert!(!metrics_meaningfully_equal(
            &Some(m.clone()),
            &Some(m1_eq.clone())
        ));
        m1_eq.recent_turns.pop();
        assert!(!metrics_meaningfully_equal(&Some(m.clone()), &Some(m1_eq)));

        // Verify AgyStepTimingCache reads from real SQLite DB and hits cache when unchanged.
        let dir = std::env::temp_dir().join(format!(
            "sf-agydb-cache-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conv-1.db");
        let init_sql = format!(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, metadata BLOB);
             INSERT INTO steps VALUES (10, 15, 3, X'{hex_blob}');
             INSERT INTO steps VALUES (11, 5, 3, X'{hex_blob}');"
        );
        if Command::new("sqlite3")
            .arg(&db)
            .arg(&init_sql)
            .status()
            .is_ok_and(|s| s.success())
        {
            let mut cache = AgyStepTimingCache::default();
            let got = cache.refresh_from_path("conv-1", &db);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].idx, 10);
            assert_eq!(got[0].output_tokens, 613);
            // Cache hit returns identical slice without SQLite re-query.
            assert_eq!(cache.refresh_from_path("conv-1", &db).len(), 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agy_transcript_fallback_filters_idle_gaps_and_short_turns() {
        // PLANNER_RESPONSE followed by USER_INPUT / SYSTEM_MESSAGE / PLANNER_RESPONSE, or <150 tok,
        // or <3_000ms must NOT produce TPS; followed by GENERIC >=3_000ms and >=150 tok succeeds.
        let jsonl = r#"
{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:40:00Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":0,"output_tokens":500}
{"step_index":2,"type":"USER_INPUT","created_at":"2026-11-21T04:40:05Z","content":"next user"}
{"step_index":3,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:41:00Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":0,"output_tokens":500}
{"step_index":4,"type":"SYSTEM_MESSAGE","created_at":"2026-11-21T04:41:05Z","content":"wakeup"}
{"step_index":5,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:42:00Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":0,"output_tokens":100}
{"step_index":6,"type":"GENERIC","created_at":"2026-11-21T04:42:05Z","content":"small output"}
{"step_index":7,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:43:00Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":0,"output_tokens":200}
{"step_index":8,"type":"GENERIC","created_at":"2026-11-21T04:43:02Z","content":"too fast"}
"#;
        let m_filtered = extract_transcript_metrics(AgentKind::Agy, jsonl, 1000).unwrap();
        assert_eq!(m_filtered.tps, None);
        assert!(m_filtered.recent_turns.is_empty());

        let valid_jsonl = format!(
            "{jsonl}{}\n{}\n",
            r#"{"step_index":9,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:00Z","model":"gemini-4-neon-high","input_tokens":2000,"cache_read_tokens":1000,"output_tokens":300}"#,
            r#"{"step_index":10,"type":"GENERIC","created_at":"2026-11-21T04:44:03Z","content":"tool"}"#
        );
        let m_valid = extract_transcript_metrics(AgentKind::Agy, &valid_jsonl, 1000).unwrap();
        assert_eq!(m_valid.tps, Some(100.0));
        assert_eq!(m_valid.recent_turns.len(), 1);
        assert_eq!(m_valid.recent_turns[0].tps, Some(100.0));
        assert_eq!(m_valid.recent_turns[0].duration_ms, Some(3_000));
    }

    #[test]
    fn codex_token_usage_record_reasoning_visible_tps_ttft_and_tool_output_ordering() {
        let jsonl = concat!(
            r#"{"timestamp":"2026-11-25T07:20:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:04.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"r1"}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:04.005Z","type":"response_item","payload":{"type":"reasoning","id":"r1"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:06.500Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a1"}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:08.000Z","type":"token_usage_record","payload":{"usage":{"input_tokens":50000,"output_tokens":240,"reasoning_output_tokens":80}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:08.100Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:08.101Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"output_tokens":240},"last_token_usage":{"input_tokens":50000,"output_tokens":240,"reasoning_output_tokens":80},"model_context_window":828400}}}"#,
            "\n",
            // Turn 2: starts at 08.100 (custom_tool_call_output when !saw_model_item),
            // Reasoning item at 10.100 -> TTFT=2000ms, visible=50 (<80) -> fallback TPS = 120/4s = 30.0,
            // reliable TPS stays 40.0 from Turn 1!
            r#"{"timestamp":"2026-11-25T07:20:10.100Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"r2"}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:12.100Z","type":"token_usage_record","payload":{"usage":{"input_tokens":60000,"output_tokens":120,"reasoning_output_tokens":70}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:20:12.105Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110000,"output_tokens":360},"last_token_usage":{"input_tokens":60000,"output_tokens":120,"reasoning_output_tokens":70},"model_context_window":828400}}}"#,
            "\n",
        );
        let m = extract_transcript_metrics(AgentKind::Codex, jsonl, 999).unwrap();
        assert_eq!(m.source, "transcript");
        assert_eq!(m.tps, Some(40.0));
        assert_eq!(m.last_output_tokens, Some(160));
        assert_eq!(m.last_duration_ms, Some(4_000));
        assert_eq!(m.ttft_ms, Some(2_000));
        assert_eq!(m.avg_ttft_ms, Some(3_000));
        assert_eq!(m.recent_turns.len(), 2);
        assert_eq!(m.recent_turns[0].tps, Some(40.0));
        assert_eq!(m.recent_turns[0].ttft_ms, Some(4_000));
        assert_eq!(m.recent_turns[1].tps, Some(30.0));
        assert_eq!(m.recent_turns[1].ttft_ms, Some(2_000));

        // Legacy Codex without `token_usage_record`: `custom_tool_call_output` AFTER `custom_tool_call`
        // must NOT overwrite `req_start_ts` 1ms before `token_count`!
        let legacy_jsonl = concat!(
            r#"{"timestamp":"2026-11-25T07:30:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:30:02.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"shell","call_id":"c1"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:30:04.999Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-25T07:30:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"output_tokens":200},"last_token_usage":{"input_tokens":10000,"output_tokens":200,"reasoning_output_tokens":0},"model_context_window":258400}}}"#,
            "\n",
        );
        let ml = extract_transcript_metrics(AgentKind::Codex, legacy_jsonl, 999).unwrap();
        assert_eq!(ml.ttft_ms, Some(2_000));
        assert_eq!(ml.tps, Some(66.7)); // 200 / 3.0s over gen_ms (NOT 200 / 0.001s!)
    }

    #[test]
    fn claude_thinking_suppresses_chunk_span_and_single_block_ttft() {
        let jsonl = concat!(
            // Turn 1: single-block completed message >=50 tokens -> TTFT suppressed (None), TPS over call_span = 200/4s = 50.0.
            r#"{"type":"user","timestamp":"2026-11-25T08:00:00.000Z","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-25T08:00:04.000Z","message":{"id":"m1","model":"claude-opus-5-5","stop_reason":"end_turn","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":1000,"cache_read_input_tokens":9000,"cache_creation_input_tokens":0,"output_tokens":200}}}"#,
            "\n",
            // Turn 2: multi-block with thinking -> TTFT = 2500ms, TPS over call_span (16.5 - 10.0 = 6.5s) = 400/6.5 = 61.5.
            r#"{"type":"user","timestamp":"2026-11-25T08:00:10.000Z","message":{"role":"user","content":"think"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-25T08:00:12.500Z","message":{"id":"m2","model":"claude-opus-5-5","stop_reason":null,"content":[{"type":"thinking","thinking":"..."}],"usage":{"input_tokens":1000,"cache_read_input_tokens":10000,"cache_creation_input_tokens":0,"output_tokens":100}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-25T08:00:16.500Z","message":{"id":"m2","model":"claude-opus-5-5","stop_reason":"end_turn","content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":1000,"cache_read_input_tokens":10000,"cache_creation_input_tokens":0,"output_tokens":400}}}"#,
            "\n",
        );
        let m = extract_transcript_metrics(AgentKind::Claude, jsonl, 999).unwrap();
        assert_eq!(m.tps, Some(61.5));
        assert_eq!(m.ttft_ms, Some(2_500));
        assert_eq!(m.avg_ttft_ms, Some(2_500));
        assert_eq!(m.recent_turns.len(), 2);
        assert_eq!(m.recent_turns[0].ttft_ms, None);
        assert_eq!(m.recent_turns[0].tps, Some(50.0));
        assert_eq!(m.recent_turns[1].ttft_ms, Some(2_500));
        assert_eq!(m.recent_turns[1].tps, Some(61.5));

        // Orphan assistant message with `has_thinking = true` and NO `user` timestamp must NEVER
        // fall back to `chunk_span`!
        let orphan_thinking = concat!(
            r#"{"type":"assistant","timestamp":"2026-11-25T08:05:00.000Z","message":{"id":"m_orphan","model":"claude-opus-5-5","stop_reason":null,"content":[{"type":"thinking","thinking":"..."}],"usage":{"input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-25T08:05:02.000Z","message":{"id":"m_orphan","model":"claude-opus-5-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1000,"output_tokens":300}}}"#,
            "\n",
        );
        let mo = extract_transcript_metrics(AgentKind::Claude, orphan_thinking, 999).unwrap();
        assert_eq!(mo.tps, None);
    }

    #[test]
    fn codex_thread_matching_uses_status_title_and_avoids_short_false_positives() {
        let dir =
            std::env::temp_dir().join(format!("starfane-cxm-{}", starfane_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("rollout-1-01a18088-1111-1111-1111-111111111111.jsonl");
        let f2 = dir.join("rollout-2-01a18191-2222-2222-2222-222222222222.jsonl");
        std::fs::write(&f1, "{}\n").unwrap();
        std::fs::write(&f2, "{}\n").unwrap();

        let rows = vec![
            CodexThreadRow {
                id: "01a18088-1111-1111-1111-111111111111".into(),
                rollout_path: Some(f1.to_string_lossy().into_owned()),
                cwd: "/Users/demo/work/app-a".into(),
                name: Some("检查仓库进度并同步远端".into()),
                created_at: 0,
                updated_at: 0,
            },
            CodexThreadRow {
                id: "01a18191-2222-2222-2222-222222222222".into(),
                rollout_path: Some(f2.to_string_lossy().into_owned()),
                cwd: "/Users/demo/work/app-c".into(),
                name: Some("查".into()),
                created_at: 0,
                updated_at: 0,
            },
        ];

        let tail = "  Worked for 39s\n› Ask Codex to do anything\n  GPT-6.1-Sol medium · ~/work/app-a · 检查仓库进度并同步远端\n  ? for shortcuts\n";
        let empty_claimed = HashSet::new();
        let hit =
            match_codex_thread(&rows, "/Users/demo/work/app-a", tail, &empty_claimed).unwrap();
        assert_eq!(hit.0, "01a18088-1111-1111-1111-111111111111");
        assert_eq!(hit.1, f1);

        // Claimed thread id must be excluded so another pane never steals an active rollout.
        let mut claimed = HashSet::new();
        claimed.insert("01a18088-1111-1111-1111-111111111111".to_string());
        assert!(match_codex_thread(&rows, "/Users/demo/work/app-a", tail, &claimed).is_none());

        // Must NOT falsely match 1-char name "查" on an unrelated title!
        let unrelated = "  GPT-6.1-Sol medium · ~/work/app-c · 查看 Issue #23\n";
        assert!(
            match_codex_thread(&rows, "/Users/demo/work/app-c", unrelated, &empty_claimed)
                .is_none()
        );

        // Blank Codex pane (no title segment) returns None.
        let blank = "› Ask Codex to do anything\n  GPT-6.2-Sol high · ~/work/app-a\n";
        assert!(
            match_codex_thread(&rows, "/Users/demo/work/app-a", blank, &empty_claimed).is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_metrics_dedups_message_chunks_and_merges_status_line() {
        let jsonl = r#"
{"type":"user","timestamp":"2026-11-02T02:49:35.000Z","message":{"role":"user","content":"fix bug"}}
{"type":"assistant","timestamp":"2026-11-02T02:49:41.590Z","message":{"id":"msg_1","model":"claude-opus-5-5","role":"assistant","usage":{"input_tokens":2,"cache_read_input_tokens":800000,"cache_creation_input_tokens":75000,"output_tokens":500}}}
{"type":"assistant","timestamp":"2026-11-02T02:49:45.000Z","message":{"id":"msg_1","model":"claude-opus-5-5","role":"assistant","usage":{"input_tokens":2,"cache_read_input_tokens":800000,"cache_creation_input_tokens":75000,"output_tokens":1000}}}
{"type":"assistant","timestamp":"2026-11-02T02:49:46.000Z","message":{"id":"syn","model":"<synthetic>","role":"assistant","usage":{"input_tokens":0,"output_tokens":0}}}
"#;
        let tr = extract_transcript_metrics(AgentKind::Claude, jsonl, 999).unwrap();
        assert_eq!(tr.source, "transcript");
        assert_eq!(tr.context_used_tokens, Some(875_002));
        assert_eq!(tr.context_window_tokens, Some(1_000_000));
        assert_eq!(tr.context_used_pct, Some(87.5));
        assert_eq!(tr.total_output_tokens, Some(1000));
        assert_eq!(tr.last_output_tokens, Some(1000));
        assert_eq!(tr.last_duration_ms, Some(10_000));
        assert_eq!(tr.tps, Some(100.0));

        let status = parse_status_line_metrics(
            AgentKind::Claude,
            "some output\nOpus 5.5 (1M context) | ~/Work/repo | main | 12% context left\n",
            1000,
        )
        .unwrap();
        assert_eq!(status.source, "status_line");
        assert_eq!(status.context_used_pct, Some(88.0));
        assert_eq!(status.context_window_tokens, Some(1_000_000));

        let merged = merge_metrics(Some(tr), Some(status)).unwrap();
        assert_eq!(merged.source, "transcript");
        assert_eq!(merged.context_window_tokens, Some(1_000_000));
        assert_eq!(merged.context_used_pct, Some(87.5));
        assert_eq!(merged.tps, Some(100.0));
    }

    #[test]
    fn codex_metrics_extracts_exact_window_and_tps() {
        let jsonl = r#"
{"timestamp":"2026-11-21T04:29:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"check"}]}}
{"timestamp":"2026-11-21T04:29:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7523311,"output_tokens":22148},"last_token_usage":{"input_tokens":156646,"output_tokens":873,"total_tokens":157519},"model_context_window":828400}}}
"#;
        let m = extract_transcript_metrics(AgentKind::Codex, jsonl, 2000).unwrap();
        assert_eq!(m.source, "transcript");
        assert_eq!(m.context_used_tokens, Some(156_646));
        assert_eq!(m.context_window_tokens, Some(828_400));
        assert_eq!(m.context_used_pct, Some(18.9));
        assert_eq!(m.total_input_tokens, Some(7_523_311));
        assert_eq!(m.total_output_tokens, Some(22_148));
        assert_eq!(m.last_output_tokens, Some(873));
        assert_eq!(m.last_duration_ms, Some(10_000));
        assert_eq!(m.tps, Some(87.3));
    }

    #[test]
    fn agy_metrics_extracts_context_window_for_gemini4_and_none_for_unknown() {
        let jsonl = r#"
{"step_index":452,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:02Z","model":"gemini-4-neon-high","input_tokens":3505,"cache_read_tokens":176329,"output_tokens":1826}
{"step_index":453,"type":"GENERIC","created_at":"2026-11-21T04:44:31Z","content":"ok"}
"#;
        let m = extract_transcript_metrics(AgentKind::Agy, jsonl, 3000).unwrap();
        assert_eq!(m.source, "transcript");
        assert_eq!(m.context_used_tokens, Some(179_834));
        assert_eq!(m.context_window_tokens, Some(512_000));
        assert_eq!(m.context_used_pct, Some(35.1));
        assert_eq!(m.last_output_tokens, Some(1826));
        assert_eq!(m.last_duration_ms, Some(29_000));
        assert_eq!(m.tps, Some(63.0));

        let unknown_jsonl = r#"
{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:02Z","model":"mystery-model","input_tokens":1000,"cache_read_tokens":0,"output_tokens":100}
"#;
        let m2 = extract_transcript_metrics(AgentKind::Agy, unknown_jsonl, 3000).unwrap();
        assert_eq!(m2.context_window_tokens, None);
        assert_eq!(m2.context_used_pct, None);
    }

    #[test]
    fn compute_tps_guards_zero_tiny_and_huge_durations() {
        assert_eq!(compute_tps_checked(0, 1000, 1, 100), None);
        assert_eq!(compute_tps_checked(100, 0, 1, 100), None);
        assert_eq!(compute_tps_checked(100, 50, 1, 100), None);
        assert_eq!(compute_tps_checked(100, 400_000, 1, 100), None);
        assert_eq!(compute_tps_checked(100, 2_000, 150, 1_500), None);
        assert_eq!(compute_tps_checked(10_000, 2_000, 150, 1_500), None); // >400 tok/s capped
        assert_eq!(compute_tps_checked(500, 2_000, 150, 1_500), Some(250.0));
    }

    #[test]
    fn file_cache_hits_when_mtime_and_len_unchanged() {
        let dir =
            std::env::temp_dir().join(format!("starfane-mc-{}", starfane_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.jsonl");
        std::fs::write(
            &file,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:02Z","input_tokens":1000,"cache_read_tokens":500,"output_tokens":200}
{"step_index":2,"type":"GENERIC","created_at":"2026-11-21T04:44:06Z","content":"ok"}
"#,
        )
        .unwrap();

        let ts_initial = parse_iso8601_ms("2026-11-21T04:44:06Z").unwrap();
        let ts_appended = parse_iso8601_ms("2026-11-21T04:44:10Z").unwrap();
        let mut cache = HashMap::new();
        let m1 = cached_transcript_telemetry(&mut cache, AgentKind::Agy, &file, 100)
            .0
            .unwrap();
        assert_eq!(m1.updated_ms, ts_initial);
        assert_eq!(m1.tps, Some(50.0));

        let m2 = cached_transcript_telemetry(&mut cache, AgentKind::Agy, &file, 999)
            .0
            .unwrap();
        assert_eq!(m2.updated_ms, ts_initial);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(
                    f,
                    r#"{{"step_index":3,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:10Z","input_tokens":2000,"cache_read_tokens":500,"output_tokens":200}}"#
                )
            })
            .unwrap();
        let m3 = cached_transcript_telemetry(&mut cache, AgentKind::Agy, &file, 2000)
            .0
            .unwrap();
        assert_eq!(m3.updated_ms, ts_appended);
        assert_eq!(m3.context_used_tokens, Some(2500));

        // When events omit timestamps, `updated_ms` falls back to `now_ms`, so a cache hit at
        // `now_ms = 999` keeps the initial `100` instead of re-reading/re-computing.
        let notime = dir.join("notime.jsonl");
        std::fs::write(
            &notime,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":500,"cache_read_tokens":0,"output_tokens":50}
"#,
        )
        .unwrap();
        let n1 = cached_transcript_telemetry(&mut cache, AgentKind::Agy, &notime, 100)
            .0
            .unwrap();
        assert_eq!(n1.updated_ms, 100);
        let n2 = cached_transcript_telemetry(&mut cache, AgentKind::Agy, &notime, 999)
            .0
            .unwrap();
        assert_eq!(n2.updated_ms, 100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_claude_pid_session_file_and_codex_resume_uuid_and_agy_unique_cwd() {
        let home =
            std::env::temp_dir().join(format!("starfane-home-{}", starfane_proto::id::new_id("T")));
        let cwd = "/Users/test/my_proj";
        let enc = encode_claude_project_dir(cwd);
        let cl_jsonl = home
            .join(".claude/projects")
            .join(enc)
            .join("sid-1234.jsonl");
        std::fs::create_dir_all(cl_jsonl.parent().unwrap()).unwrap();
        std::fs::write(&cl_jsonl, "{}\n").unwrap();
        let cl_sess = home.join(".claude/sessions/4242.json");
        std::fs::create_dir_all(cl_sess.parent().unwrap()).unwrap();
        std::fs::write(
            &cl_sess,
            format!(r#"{{"pid":4242,"sessionId":"sid-1234","cwd":"{cwd}"}}"#),
        )
        .unwrap();

        assert_eq!(
            resolve_claude_transcript_by_pid_in(&home, 4242),
            Some(("sid-1234".into(), cl_jsonl))
        );
        assert_eq!(resolve_claude_transcript_by_pid_in(&home, 9999), None);

        let cx_uuid = "01a1572d-8227-7972-873d-d0848f641c83";
        let cx_jsonl = home
            .join(".codex/sessions/2026/11/21")
            .join(format!("rollout-2026-11-21T12-00-00-{cx_uuid}.jsonl"));
        std::fs::create_dir_all(cx_jsonl.parent().unwrap()).unwrap();
        std::fs::write(&cx_jsonl, "{}\n").unwrap();
        assert_eq!(
            resolve_codex_transcript_by_id_in(&home, cx_uuid),
            Some(cx_jsonl.clone())
        );
        assert_eq!(
            extract_session_id_from_transcript_path(AgentKind::Codex, &cx_jsonl).as_deref(),
            Some(cx_uuid)
        );

        let agy_id = "81166b09-793d-4380-9ff7-637632489fcf";
        let agy_tr = home
            .join(".gemini/antigravity-cli/brain")
            .join(agy_id)
            .join(".system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(agy_tr.parent().unwrap()).unwrap();
        std::fs::write(&agy_tr, "{}\n").unwrap();
        let agy_rows = vec![AgyConvRow {
            conversation_id: agy_id.into(),
            workspace_uris: r#"["file:///Users/demo"]"#.into(),
            status: String::new(),
            last_modified_time: String::new(),
            last_user_input_time: String::new(),
            step_count: 1,
        }];
        let no_claimed = HashSet::new();
        assert_eq!(rows_test_anchor_check(&agy_rows), 1);
        assert_eq!(
            match_agy_conversation_in(&home, &agy_rows, "/Users/demo", &no_claimed),
            Some((agy_id.into(), agy_tr.clone()))
        );
        // Must NOT match child path `/Users/demo/work/app-b`!
        assert!(
            match_agy_conversation_in(&home, &agy_rows, "/Users/demo/work/app-b", &no_claimed)
                .is_none()
        );
        // Already-claimed conversation ID must be skipped.
        let mut claimed = HashSet::new();
        claimed.insert(agy_id.to_string());
        assert!(match_agy_conversation_in(&home, &agy_rows, "/Users/demo", &claimed).is_none());
        assert_eq!(
            extract_session_id_from_transcript_path(AgentKind::Agy, &agy_tr).as_deref(),
            Some(agy_id)
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    fn rows_test_anchor_check(rows: &[AgyConvRow]) -> usize {
        rows.len()
    }

    #[test]
    fn process_birth_and_pid_recycling_guards_reject_stale_transcripts() {
        assert_eq!(parse_ps_etime_secs("05"), Some(5));
        assert_eq!(parse_ps_etime_secs("07:57"), Some(477));
        assert_eq!(parse_ps_etime_secs("03:34:26"), Some(12_866));
        assert_eq!(parse_ps_etime_secs("01-03:34:26"), Some(99_266));
        assert_eq!(parse_ps_etime_secs("00:99"), None);
        assert_eq!(parse_ps_etime_secs(""), None);

        let now_ms = 1_795_587_600_000u64;
        let ps_sample = "  92848       07:57\n  68521 01-03:34:26\n";
        let births = parse_ps_pid_etimes(ps_sample, now_ms);
        assert_eq!(births.get(&92848), Some(&(now_ms - 477_000)));
        assert_eq!(births.get(&68521), Some(&(now_ms - 99_266_000)));

        let home = std::env::temp_dir().join(format!(
            "starfane-birth-guard-{}",
            starfane_proto::id::new_id("T")
        ));
        let cwd = "/Users/demo/.agent_session";

        // 1. AGY: pre-birth conversation (`10:55` yesterday) is strictly rejected for a pane born at `14:13` today!
        let old_agy_id = "4b808b49-48a6-4aaa-9eab-15d68e18f657";
        let old_agy_tr = home
            .join(".gemini/antigravity-cli/brain")
            .join(old_agy_id)
            .join(".system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(old_agy_tr.parent().unwrap()).unwrap();
        std::fs::write(
            &old_agy_tr,
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-24T02:55:39Z","content":"教我配置一下gh吧"}
"#,
        )
        .unwrap();
        let pane_birth_ms = parse_iso8601_ms("2026-11-25T06:13:18Z").unwrap();
        let agy_rows = vec![AgyConvRow {
            conversation_id: old_agy_id.into(),
            workspace_uris: format!(r#"["file://{cwd}"]"#),
            status: "CASCADE_RUN_STATUS_IDLE".into(),
            last_modified_time: "2026-11-24 09:39:32.148245+00:00".into(),
            last_user_input_time: "2026-11-24 09:39:06.702198+00:00".into(),
            step_count: 65,
        }];
        assert!(
            match_agy_conversation_with_birth_in(
                &home,
                &agy_rows,
                cwd,
                &HashSet::new(),
                Some(pane_birth_ms),
            )
            .is_none(),
            "pre-birth AGY conversation must never attach to newly spawned pane"
        );

        // Post-birth AGY conversation (`14:15` today >= `14:13` birth) attaches cleanly when unique!
        let new_agy_id = "99999999-1111-2222-3333-444444444444";
        let new_agy_tr = home
            .join(".gemini/antigravity-cli/brain")
            .join(new_agy_id)
            .join(".system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(new_agy_tr.parent().unwrap()).unwrap();
        std::fs::write(
            &new_agy_tr,
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-25T06:15:00Z","content":"new prompt"}
"#,
        )
        .unwrap();
        let mut both_rows = vec![AgyConvRow {
            conversation_id: new_agy_id.into(),
            workspace_uris: format!(r#"["file://{cwd}"]"#),
            status: "CASCADE_RUN_STATUS_IDLE".into(),
            last_modified_time: "2026-11-25 06:15:05.000000+00:00".into(),
            last_user_input_time: "2026-11-25 06:15:00.000000+00:00".into(),
            step_count: 2,
        }];
        both_rows.extend(agy_rows);
        assert_eq!(
            match_agy_conversation_with_birth_in(
                &home,
                &both_rows,
                cwd,
                &HashSet::new(),
                Some(pane_birth_ms),
            ),
            Some((new_agy_id.into(), new_agy_tr))
        );

        // 2. Claude: recycled OS PID (`78428.json` from days ago) is rejected when OS PID birth is today!
        let enc = encode_claude_project_dir(cwd);
        let cl_jsonl = home
            .join(".claude/projects")
            .join(enc)
            .join("stale-claude-sid.jsonl");
        std::fs::create_dir_all(cl_jsonl.parent().unwrap()).unwrap();
        std::fs::write(&cl_jsonl, "{}\n").unwrap();
        let cl_sess = home.join(".claude/sessions/78428.json");
        std::fs::create_dir_all(cl_sess.parent().unwrap()).unwrap();
        let stale_start_ms = pane_birth_ms - 86_400_000 * 5;
        std::fs::write(
            &cl_sess,
            format!(
                r#"{{"pid":78428,"sessionId":"stale-claude-sid","cwd":"{cwd}","startedAt":{stale_start_ms}}}"#
            ),
        )
        .unwrap();
        let mut pid_births = HashMap::new();
        pid_births.insert(78428, pane_birth_ms);
        assert!(
            resolve_claude_transcript_by_pids_in(&home, &[78428], &pid_births, &HashSet::new())
                .is_none(),
            "recycled Claude PID must not bind stale <pid>.json"
        );
        // Matching PID birth within tolerance succeeds.
        pid_births.insert(78428, stale_start_ms + 1_500);
        assert!(resolve_claude_transcript_by_pids_in(
            &home,
            &[78428],
            &pid_births,
            &HashSet::new()
        )
        .is_some());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn extracts_reasoning_effort_for_agy_codex_and_claude() {
        assert_eq!(
            extract_agy_effort_from_model("gemini-4-neon-high").as_deref(),
            Some("high")
        );
        assert_eq!(
            extract_agy_effort_from_model("gemini-4-neon-xhigh").as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            extract_agy_effort_from_model("Orchestrator").as_deref(),
            Some("fixed")
        );
        assert_eq!(extract_agy_effort_from_model("unknown-model"), None);

        let codex_jsonl = r#"
{"type":"turn_context","payload":{"model":"gpt-6.2-sol","collaboration_mode":{"settings":{"reasoning_effort":"medium"}}}}
{"type":"turn_context","payload":{"model":"gpt-6.2-sol","effort":"xhigh"}}
"#;
        let (cx_m, cx_e) = extract_codex_turn_context(codex_jsonl);
        assert_eq!(cx_m.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(cx_e.as_deref(), Some("xhigh"));

        let home = std::env::temp_dir().join(format!(
            "starfane-effort-test-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "model = \"gpt-6.2-sol\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"effortLevel":"medium","modelSettings":{"claude-opus-5-5":{"effortLevel":"max"}}}"#,
        )
        .unwrap();

        assert_eq!(read_codex_default_effort(&home).as_deref(), Some("high"));
        assert_eq!(
            read_claude_default_effort(&home, Some("claude-opus-5-5")).as_deref(),
            Some("max")
        );
        assert_eq!(
            read_claude_default_effort(&home, Some("claude-sonnet")).as_deref(),
            Some("medium")
        );
        assert_eq!(
            infer_session_effort(
                AgentKind::Codex,
                Some("gpt-6.2-sol"),
                None,
                Some("xhigh"),
                Some(&home)
            )
            .as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            infer_session_effort(
                AgentKind::Codex,
                Some("gpt-6.2-sol"),
                None,
                None,
                Some(&home)
            )
            .as_deref(),
            Some("high")
        );
        assert_eq!(
            infer_session_effort(
                AgentKind::Claude,
                Some("claude-sonnet"),
                Some("high"),
                None,
                Some(&home)
            )
            .as_deref(),
            Some("high")
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn parses_lsof_agy_presence_and_separates_same_cwd_panes_with_model_effort() {
        let lsof_out = "\
p51090
n/Users/demo/.gemini/antigravity-cli/code_tracker
n/Users/demo/.gemini/antigravity-cli/presence/66fac8b3-93ac-4c7d-814c-b79bb0ec9c28.lock
n/Users/demo/.gemini/antigravity-cli/presence/not-a-lock.tmp
p93736
n/Users/demo/.gemini/antigravity-cli/presence/f03f879a-451a-495e-ad84-90e4b6569501.lock
p99999
n/Users/demo/.gemini/antigravity-cli/brain/other/file.txt
";
        let parsed = parse_lsof_agy_presence(lsof_out);
        assert_eq!(
            parsed.get(&51090).map(String::as_str),
            Some("66fac8b3-93ac-4c7d-814c-b79bb0ec9c28")
        );
        assert_eq!(
            parsed.get(&93736).map(String::as_str),
            Some("f03f879a-451a-495e-ad84-90e4b6569501")
        );
        assert!(!parsed.contains_key(&99999));

        // Cache hit within TTL, refresh after TTL, separates two panes sharing the same cwd.
        let mut cache = AgyPresenceCache::default();
        let t0 = std::time::Instant::now();
        let stale = cache.stale_pids(&[11886, 51090, 91448, 93736], t0);
        assert_eq!(stale, vec![11886, 51090, 91448, 93736]);
        cache.update(&stale, &parsed, t0);
        assert!(cache
            .stale_pids(&[11886, 51090, 91448, 93736], t0)
            .is_empty());
        assert_eq!(
            cache.find_for_pids(&[11886, 51090]).as_deref(),
            Some("66fac8b3-93ac-4c7d-814c-b79bb0ec9c28")
        );
        assert_eq!(
            cache.find_for_pids(&[91448, 93736]).as_deref(),
            Some("f03f879a-451a-495e-ad84-90e4b6569501")
        );

        // Distinct AGY transcripts yield distinct model, effort, and telemetry metrics.
        let home = std::env::temp_dir().join(format!(
            "starfane-agy-presence-{}",
            starfane_proto::id::new_id("T")
        ));
        let id1 = "66fac8b3-93ac-4c7d-814c-b79bb0ec9c28";
        let id2 = "f03f879a-451a-495e-ad84-90e4b6569501";
        let tr1 = home
            .join(".gemini/antigravity-cli/brain")
            .join(id1)
            .join(".system_generated/logs/transcript.jsonl");
        let tr2 = home
            .join(".gemini/antigravity-cli/brain")
            .join(id2)
            .join(".system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(tr1.parent().unwrap()).unwrap();
        std::fs::create_dir_all(tr2.parent().unwrap()).unwrap();
        std::fs::write(
            &tr1,
            concat!(
                r#"{"step_index":1,"type":"USER_INPUT","created_at":"2026-11-23T10:00:00Z","content":"hello pane 17"}"#,
                "\n",
                r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T10:00:02Z","model":"gemini-4-neon-high","content":"done 17","input_tokens":100000,"cache_read_tokens":28000,"output_tokens":400}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &tr2,
            concat!(
                r#"{"step_index":1,"type":"USER_INPUT","created_at":"2026-11-23T10:05:00Z","content":"hello pane 44"}"#,
                "\n",
                r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T10:05:04Z","model":"gemini-4-neon-medium","content":"done 44","input_tokens":256000,"cache_read_tokens":0,"output_tokens":800}"#,
                "\n"
            ),
        )
        .unwrap();

        let resolved1 = resolve_agy_transcript_by_id_in(&home, id1).unwrap();
        let resolved2 = resolve_agy_transcript_by_id_in(&home, id2).unwrap();
        assert_eq!(resolved1, tr1);
        assert_eq!(resolved2, tr2);

        let mut mcache = HashMap::new();
        let (m1, model1, eff1) =
            cached_transcript_telemetry(&mut mcache, AgentKind::Agy, &resolved1, 1000);
        let (m2, model2, eff2) =
            cached_transcript_telemetry(&mut mcache, AgentKind::Agy, &resolved2, 1000);
        let m1 = m1.expect("metrics 1");
        let m2 = m2.expect("metrics 2");
        assert_eq!(model1.as_deref(), Some("gemini-4-neon-high"));
        assert_eq!(eff1.as_deref(), Some("high"));
        assert_eq!(m1.context_used_tokens, Some(128_000));
        assert_eq!(m1.context_window_tokens, Some(512_000));
        assert_eq!(m1.context_used_pct, Some(25.0));

        assert_eq!(model2.as_deref(), Some("gemini-4-neon-medium"));
        assert_eq!(eff2.as_deref(), Some("medium"));
        assert_eq!(m2.context_used_tokens, Some(256_000));
        assert_eq!(m2.context_window_tokens, Some(512_000));
        assert_eq!(m2.context_used_pct, Some(50.0));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remote_zero_install_pure_parsers_claude_codex_agy_and_status() {
        // 1. Remote Claude `<pid>.json` parsing + recycled-PID skew + claimed exclusion.
        let birth_ms = 1_795_600_000_000u64;
        let cl_json = format!(
            r#"{{"pid":4242,"sessionId":"cl-sid-99","cwd":"/home/ubuntu/repo","startedAt":{}}}"#,
            birth_ms - 1_500
        );
        let parsed = parse_claude_pid_session_json(&cl_json, Some(4242)).unwrap();
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.session_id, "cl-sid-99");
        assert_eq!(parsed.cwd.as_deref(), Some("/home/ubuntu/repo"));
        assert_eq!(parsed.started_at_ms, Some(birth_ms - 1_500));
        assert_eq!(
            claude_remote_transcript_path("/home/ubuntu/", "/home/ubuntu/repo", "cl-sid-99"),
            "/home/ubuntu/.claude/projects/-home-ubuntu-repo/cl-sid-99.jsonl"
        );
        // PID mismatch rejected.
        assert!(parse_claude_pid_session_json(&cl_json, Some(9999)).is_none());
        // Path traversal sessionId rejected.
        assert!(parse_claude_pid_session_json(
            r#"{"pid":4242,"sessionId":"../etc/passwd","cwd":"/home/ubuntu"}"#,
            Some(4242)
        )
        .is_none());
        // Valid birth window passes, recycled PID (1 day old) fails, claimed fails.
        assert!(verify_remote_claude_pid_session(
            &parsed,
            Some(birth_ms),
            None,
            &HashSet::new()
        ));
        assert!(!verify_remote_claude_pid_session(
            &parsed,
            Some(birth_ms + 86_400_000),
            None,
            &HashSet::new()
        ));
        let mut claimed = HashSet::new();
        claimed.insert("cl-sid-99".to_string());
        assert!(!verify_remote_claude_pid_session(
            &parsed,
            Some(birth_ms),
            None,
            &claimed
        ));

        // 2. Remote Codex `session_meta` first-line parser + pure-POSIX zero-sqlite3 matcher.
        let meta_line = r#"{"timestamp":"2026-11-25T08:15:00.000Z","type":"session_meta","payload":{"id":"01a18088-1111-1111-1111-111111111111","timestamp":"2026-11-25T08:15:00.000Z","cwd":"/home/ubuntu/work"}}"#;
        let sm = parse_codex_session_meta_line(meta_line).unwrap();
        assert_eq!(sm.id, "01a18088-1111-1111-1111-111111111111");
        assert_eq!(sm.cwd.as_deref(), Some("/home/ubuntu/work"));
        let sm_ts = sm.timestamp_ms.unwrap();
        assert!(parse_codex_session_meta_line(r#"{"type":"turn_context","payload":{}}"#).is_none());

        let rollouts = vec![
            RemoteCodexRolloutCandidate {
                id: "old-rollout".into(),
                remote_path: "/home/ubuntu/.codex/sessions/2026/11/24/r-old.jsonl".into(),
                cwd: "/home/ubuntu/work".into(),
                birth_ms: Some(sm_ts - 86_400_000),
                mtime_ms: Some(sm_ts - 86_400_000),
            },
            RemoteCodexRolloutCandidate {
                id: sm.id.clone(),
                remote_path: "/home/ubuntu/.codex/sessions/2026/11/25/r-new.jsonl".into(),
                cwd: "/home/ubuntu/work".into(),
                birth_ms: Some(sm_ts),
                mtime_ms: Some(sm_ts + 2_000),
            },
        ];
        assert_eq!(
            match_remote_codex_rollouts(
                &rollouts,
                "/home/ubuntu/work",
                None,
                &HashSet::new(),
                Some(sm_ts - 1_000),
                true,
            ),
            Some((
                sm.id.clone(),
                "/home/ubuntu/.codex/sessions/2026/11/25/r-new.jsonl".into()
            ))
        );
        assert!(match_remote_codex_rollouts(
            &rollouts,
            "/home/ubuntu/work",
            None,
            &HashSet::new(),
            Some(sm_ts - 1_000),
            false,
        )
        .is_none());
        let mut cx_claimed = HashSet::new();
        cx_claimed.insert(sm.id.clone());
        assert!(match_remote_codex_rollouts(
            &rollouts,
            "/home/ubuntu/work",
            None,
            &cx_claimed,
            Some(sm_ts - 1_000),
            true,
        )
        .is_none());

        // 3. Remote Codex SQLite delimited (TSV) + JSON + pure remote matcher.
        let cx_tsv = format!(
            "cx-1\t/home/ubuntu/.codex/sessions/2026/11/25/r1.jsonl\t/home/ubuntu/work\t排查远端延迟问题\t{}\t{}\n",
            (sm_ts / 1000) as i64,
            ((sm_ts + 5_000) / 1000) as i64
        );
        let cx_rows = parse_codex_thread_rows_delimited(&cx_tsv, '\t');
        assert_eq!(cx_rows.len(), 1);
        let cx_tail = "  GPT-6.2-Sol high · ~/work · 排查远端延迟问题\n";
        assert_eq!(
            match_remote_codex_threads(
                &cx_rows,
                "/home/ubuntu/work/",
                None,
                cx_tail,
                &HashSet::new(),
                Some(sm_ts - 1_000),
                true,
            ),
            Some((
                "cx-1".into(),
                "/home/ubuntu/.codex/sessions/2026/11/25/r1.jsonl".into()
            ))
        );
        assert!(match_remote_codex_threads(
            &cx_rows,
            "/home/ubuntu/work",
            None,
            cx_tail,
            &HashSet::new(),
            Some(sm_ts + 60_000),
            true,
        )
        .is_none());

        // 4. Remote AGY `conversation_summaries` delimited rows (root + Swarm child) + `/proc/$pid/fd` parser.
        let agy_tsv = "conv-old\t[\"file:///home/ubuntu/work\"]\tCASCADE_RUN_STATUS_IDLE\t2026-11-24T01:00:00Z\t2026-11-24T01:00:00Z\t12\n\
                       conv-new\t[\"file:///home/ubuntu/work\"]\tCASCADE_RUN_STATUS_RUNNING\t2026-11-25T08:16:00Z\t2026-11-25T08:15:30Z\t4\n";
        let agy_rows = parse_agy_conv_rows_delimited(agy_tsv, '\t');
        assert_eq!(agy_rows.len(), 2);
        assert_eq!(
            match_remote_agy_conversation(
                "/home/ubuntu",
                &agy_rows,
                "/home/ubuntu/work",
                None,
                &HashSet::new(),
                Some(sm_ts),
                true,
            ),
            Some((
                "conv-new".into(),
                "/home/ubuntu/.gemini/antigravity-cli/brain/conv-new/.system_generated/logs/transcript.jsonl".into()
            ))
        );
        assert!(match_remote_agy_conversation(
            "/home/ubuntu",
            &agy_rows,
            "/home/ubuntu/work/sub",
            None,
            &HashSet::new(),
            Some(sm_ts),
            true,
        )
        .is_none());

        let child_tsv = "child-1\tconv-new\tssh-mirror\tCASCADE_RUN_STATUS_RUNNING\t0\n\
                         child-2\tconv-new\tssh-webui\tCASCADE_RUN_STATUS_IDLE\t1\n";
        let child_rows = parse_agy_child_rows_delimited(child_tsv, '\t');
        let children = remote_agy_children_of("/home/ubuntu", &child_rows, "conv-new");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].agent_name.as_deref(), Some("ssh-mirror"));
        assert_eq!(children[0].state, starfane_proto::SessionState::Working);
        assert_eq!(children[1].state, starfane_proto::SessionState::Ended);

        let proc_fd_out = "\
/proc/1234/fd:
lrwx------ 1 ubuntu ubuntu 64 Nov 25 08:15 11 -> /home/ubuntu/.gemini/antigravity-cli/presence/66fac8b3-93ac-4c7d-814c-b79bb0ec9c28.lock
5678\t/home/ubuntu/.gemini/antigravity-cli/presence/f03f879a-451a-495e-ad84-90e4b6569501.lock
p9012
n/Users/demo/.gemini/antigravity-cli/presence/11111111-2222-3333-4444-555555555555.lock
";
        let proc_locks = parse_proc_fd_agy_presence(proc_fd_out);
        assert_eq!(
            proc_locks.get(&1234),
            Some(&vec!["66fac8b3-93ac-4c7d-814c-b79bb0ec9c28".to_string()])
        );
        assert_eq!(
            proc_locks.get(&5678),
            Some(&vec!["f03f879a-451a-495e-ad84-90e4b6569501".to_string()])
        );
        assert_eq!(
            proc_locks.get(&9012),
            Some(&vec!["11111111-2222-3333-4444-555555555555".to_string()])
        );

        // 5. Pure config effort + status-line fallback parsers.
        assert_eq!(
            parse_codex_default_effort_toml("model_reasoning_effort = \"xhigh\"\n").as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            parse_claude_default_effort_json(
                r#"{"effortLevel":"medium","modelSettings":{"claude-opus-5-5":{"effortLevel":"max"}}}"#,
                Some("claude-opus-5-5")
            )
            .as_deref(),
            Some("max")
        );
        let st = parse_status_line_metrics(
            AgentKind::Codex,
            "GPT-6.2-Sol high · ~/work · 75% context left\n",
            1234,
        )
        .unwrap();
        assert_eq!(st.context_used_pct, Some(25.0));
    }

    #[test]
    fn codex_status_line_model_effort_and_stale_scrollback_title_isolation() {
        // Blank Codex prompts (`%20`, `%28`, `%36`) expose model + effort and return `None` for title.
        let blank_tail = "\
› Ask Codex to do anything

  GPT-6.1-Sol low · ~/Work/chatgpt-register
  ← for agents · ? for shortcuts
";
        assert_eq!(
            parse_codex_status_line_model_effort(blank_tail),
            (Some("gpt-6.1-sol".into()), Some("low".into()))
        );
        assert_eq!(extract_codex_status_title(blank_tail), None);

        // Active / resumed Codex pane (`%21`) exposes model, effort, AND explicit thread title.
        let active_tail = "\
› Ask Codex to do anything

  GPT-6.1-Sol high · ~/Work/chatgpt-register · 新增 zjc API 并按本地样式打包
  ← for agents · ? for shortcuts
";
        assert_eq!(
            parse_codex_status_line_model_effort(active_tail),
            (Some("gpt-6.1-sol".into()), Some("high".into()))
        );
        assert_eq!(
            extract_codex_status_title(active_tail).as_deref(),
            Some("新增 zjc API 并按本地样式打包")
        );

        // Stale exited Codex status bar in scrollback ABOVE a fresh blank Codex prompt (live `%28`/`%36`):
        // MUST stop at bottom-most status bar -> returns `None` title and current `gpt-6.1-sol`/`low`!
        let restarted_blank_tail = "\
  GPT-6-Astra high · ~/Work/lingbot-vla-v2 · 安装 lingbot-vla-v2 环境并确认日志工具 · Main [default] · ← for agents
❯ codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen
  >_ OpenAI Codex (v0.169.0)
     ~/Work/lingbot-vla-v2
› Ask Codex to do anything
  GPT-6.1-Sol low · ~/Work/lingbot-vla-v2
  ← for agents · ? for shortcuts
";
        assert_eq!(extract_codex_status_title(restarted_blank_tail), None);
        assert_eq!(
            parse_codex_status_line_model_effort(restarted_blank_tail),
            (Some("gpt-6.1-sol".into()), Some("low".into()))
        );

        // Various model families and effort values (`o3`, `o4-mini`, `codex-mini`, `xhigh`, no effort).
        assert_eq!(
            parse_codex_status_line_model_effort("  o4-mini xhigh · ~/repo · Debug test\n"),
            (Some("o4-mini".into()), Some("xhigh".into()))
        );
        assert_eq!(
            parse_codex_status_line_model_effort("  codex-mini medium · ~/repo\n"),
            (Some("codex-mini".into()), Some("medium".into()))
        );
        assert_eq!(
            parse_codex_status_line_model_effort("  o3 · ~/repo\n"),
            (Some("o3".into()), None)
        );
        assert_eq!(
            parse_codex_status_line_model_effort("  ← for agents · ? for shortcuts\n"),
            (None, None)
        );
        // Context-left suffix is NOT treated as a thread title.
        assert_eq!(
            extract_codex_status_title("GPT-6.2-Sol high · ~/work · 75% context left\n"),
            None
        );
    }
}
