//! Session context-window & generation-speed (TPS) telemetry extraction:
//! - Read-only `~/.cc-switch/cc-switch.db` streaming TPS (`duration - ttft`), TTFT, and
//!   `input_token_semantics`-aware context usage (cached by DB + WAL `(mtime, len)`).
//! - Exact transcript JSONL resolution (Claude subtree PIDs → `~/.claude/sessions/<pid>.json`,
//!   Codex `resume <uuid>` & `~/.codex/state_5.sqlite` `threads.name` + `cwd`, AGY
//!   `conversation_summaries.db` unique-cwd root conversation).
//! - Transcript JSONL metrics with `(mtime, len)` file caching + status-line fallback.

use super::transcript::{parse_iso8601_ms, read_tail_utf8, MAX_TRANSCRIPT_READ_BYTES};
use baton_proto::{AgentKind, SessionMetrics, SessionRecord};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Minimum generation duration (ms) accepted for TPS calculation (avoids divide-by-zero / jitter spikes).
const MIN_TPS_DURATION_MS: u64 = 100;
/// Maximum single-call generation duration (ms) accepted for TPS calculation (5 minutes).
const MAX_TPS_DURATION_MS: u64 = 300_000;
/// Sanity upper bound on computed TPS to guard against clock skew / batched timestamps.
const MAX_PLAUSIBLE_TPS: f64 = 5_000.0;

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

fn db_signature(db_path: &Path) -> DbSig {
    let mut wal_os = db_path.as_os_str().to_os_string();
    wal_os.push("-wal");
    DbSig {
        db: file_signature(db_path),
        wal: file_signature(Path::new(&wal_os)),
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
        return String::from_utf8(out.stdout).ok();
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
    String::from_utf8(retry.stdout).ok()
}

// ---------------------------------------------------------------------------
// File-level `(mtime, len)` cache for JSONL transcript metrics
// ---------------------------------------------------------------------------

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
    let metrics = extract_transcript_metrics(agent, &tail, now_ms);
    let (model, effort) = match agent {
        AgentKind::Codex => extract_codex_turn_context(&tail),
        AgentKind::Agy => {
            let m = extract_agy_latest_model(&tail);
            let e = m.as_deref().and_then(extract_agy_effort_from_model);
            (m, e)
        }
        AgentKind::Claude => (extract_claude_latest_model(&tail), None),
    };
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

/// Scan AGY `transcript.jsonl` `PLANNER_RESPONSE` entries (keeping the latest) for `model`.
pub(crate) fn extract_agy_latest_model(text: &str) -> Option<String> {
    let mut latest: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('{') || !t.contains("\"PLANNER_RESPONSE\"") || !t.contains("\"model\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
            continue;
        }
        if let Some(m) = obj
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            latest = Some(m.to_string());
        }
    }
    latest
}

/// Scan Claude JSONL `assistant` messages (keeping the latest) for `message.model`.
fn extract_claude_latest_model(text: &str) -> Option<String> {
    let mut latest: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('{') || !t.contains("\"assistant\"") || !t.contains("\"model\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(m) = obj
            .get("message")
            .and_then(|msg| msg.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            latest = Some(m.to_string());
        }
    }
    latest
}

/// Scan Codex rollout JSONL `turn_context` entries (keeping the latest) for `(model, effort)`.
pub(crate) fn extract_codex_turn_context(text: &str) -> (Option<String>, Option<String>) {
    let mut latest_model: Option<String> = None;
    let mut latest_effort: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('{') || !t.contains("\"turn_context\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("turn_context") {
            continue;
        }
        let Some(payload) = obj.get("payload") else {
            continue;
        };
        if let Some(m) = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            latest_model = Some(m.to_string());
        }
        let eff = payload
            .get("effort")
            .and_then(Value::as_str)
            .or_else(|| {
                payload
                    .get("collaboration_mode")
                    .and_then(|c| c.get("settings"))
                    .and_then(|s| s.get("reasoning_effort"))
                    .and_then(Value::as_str)
            })
            .or_else(|| payload.get("reasoning_effort").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(e) = eff {
            latest_effort = Some(e.to_ascii_lowercase());
        }
    }
    (latest_model, latest_effort)
}

/// Read default `model_reasoning_effort` from `<home>/.codex/config.toml`.
pub(crate) fn read_codex_default_effort(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".codex/config.toml")).ok()?;
    let val: toml::Value = toml::from_str(&text).ok()?;
    val.get("model_reasoning_effort")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Read default effort level from `<home>/.claude/settings.json` (`modelSettings.<model>.effortLevel`
/// with fallback to top-level `effortLevel`).
pub(crate) fn read_claude_default_effort(home: &Path, model: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".claude/settings.json")).ok()?;
    let obj: Value = serde_json::from_str(&text).ok()?;
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
    }
}
// Removed leftover tail lines after cached_transcript_metrics refactor.

// ---------------------------------------------------------------------------
// `~/.cc-switch/cc-switch.db` read-only cache & cc-switch streaming TPS math
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CcSwitchRow {
    pub session_id: String,
    #[serde(default)]
    pub app_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub first_token_ms: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// `0` = Claude exclusive, `1` = Codex inclusive (`input_tokens` already includes cache),
    /// `2` = AGY exclusive (`input_tokens + cache_read_tokens + cache_creation_tokens`).
    #[serde(default)]
    pub input_token_semantics: i64,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CcSwitchEntry {
    pub metrics: SessionMetrics,
    pub model: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct CcSwitchCache {
    sig: Option<DbSig>,
    codex_default_window: Option<u64>,
    by_session: HashMap<String, CcSwitchEntry>,
}

impl CcSwitchCache {
    /// Refresh from `~/.cc-switch/cc-switch.db` only when DB or WAL `(mtime, len)` changed.
    pub(crate) fn refresh_if_needed(&mut self, now_ms: u64) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let db_path = home.join(".cc-switch/cc-switch.db");
        let sig = db_signature(&db_path);
        if sig.db.is_none() {
            return;
        }
        if self.sig.as_ref() == Some(&sig) {
            return;
        }
        if self.codex_default_window.is_none() {
            self.codex_default_window = read_codex_default_context_window(&home);
        }
        let sql = r#"
WITH valid AS (
  SELECT rowid, session_id, app_type, model,
         COALESCE(input_tokens, 0) AS input_tokens,
         COALESCE(output_tokens, 0) AS output_tokens,
         COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
         COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
         latency_ms, first_token_ms, duration_ms,
         COALESCE(input_token_semantics, 0) AS input_token_semantics,
         COALESCE(created_at, 0) AS created_at
  FROM proxy_request_logs
  WHERE status_code = 200
    AND session_id IS NOT NULL
    AND session_id != ''
    AND (COALESCE(input_tokens, 0) > 0 OR COALESCE(output_tokens, 0) > 0)
),
ranked AS (
  SELECT *,
         ROW_NUMBER() OVER (
           PARTITION BY session_id
           ORDER BY
             CASE WHEN LOWER(COALESCE(model, '')) LIKE '%haiku%'
                    OR LOWER(COALESCE(model, '')) LIKE '%flash-worker%'
                  THEN 1 ELSE 0 END ASC,
             created_at DESC,
             rowid DESC
         ) AS rn
  FROM valid
),
totals AS (
  SELECT session_id,
         SUM(CASE WHEN input_token_semantics = 1 THEN input_tokens
                  ELSE input_tokens + cache_read_tokens + cache_creation_tokens END) AS total_input_tokens,
         SUM(output_tokens) AS total_output_tokens
  FROM valid
  GROUP BY session_id
)
SELECT r.session_id, r.app_type, r.model,
       r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_creation_tokens,
       r.latency_ms, r.first_token_ms, r.duration_ms,
       r.input_token_semantics, r.created_at,
       t.total_input_tokens, t.total_output_tokens
FROM ranked r
JOIN totals t ON r.session_id = t.session_id
WHERE r.rn = 1;
"#;
        let Some(json_out) = run_sqlite_json(&db_path, sql) else {
            return;
        };
        let Ok(rows) = serde_json::from_str::<Vec<CcSwitchRow>>(&json_out) else {
            return;
        };
        let mut map = HashMap::with_capacity(rows.len());
        let cx_win = self.codex_default_window;
        for row in rows {
            let model = row
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let metrics = cc_switch_row_to_metrics(&row, cx_win, now_ms);
            map.insert(row.session_id, CcSwitchEntry { metrics, model });
        }
        self.by_session = map;
        self.sig = Some(sig);
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<&CcSwitchEntry> {
        self.by_session.get(session_id)
    }
}

/// Convert a single `cc-switch` `proxy_request_logs` row into `SessionMetrics`:
/// - Streaming TPS: uses `total_ms - first_token_ms` when `total_ms > first_token_ms` and diff >= 100ms,
///   excluding TTFT wait time; falls back to `total_ms` otherwise.
/// - Context tokens: respects `input_token_semantics` (`1` = inclusive, `0|2` = exclusive).
pub(crate) fn cc_switch_row_to_metrics(
    row: &CcSwitchRow,
    codex_default_window: Option<u64>,
    now_ms: u64,
) -> SessionMetrics {
    let ctx_used = if row.input_token_semantics == 1 {
        row.input_tokens
    } else {
        row.input_tokens
            .saturating_add(row.cache_read_tokens)
            .saturating_add(row.cache_creation_tokens)
    };
    let ctx_used_opt = (ctx_used > 0).then_some(ctx_used);

    let agent = match row.app_type.as_str() {
        "claude" => AgentKind::Claude,
        "codex" => AgentKind::Codex,
        "agy" => AgentKind::Agy,
        _ => AgentKind::Claude,
    };

    let ctx_window = infer_context_window(
        agent,
        row.model.as_deref(),
        ctx_used_opt,
        codex_default_window,
    );
    let ctx_pct = match (ctx_used_opt, ctx_window) {
        (Some(u), Some(w)) => pct_1dp(u, w),
        _ => None,
    };

    let total_ms = row
        .duration_ms
        .filter(|&v| v > 0)
        .or_else(|| row.latency_ms.filter(|&v| v > 0));
    let ttft_ms = row.first_token_ms.filter(|&v| v > 0);
    let gen_ms = match (total_ms, ttft_ms) {
        (Some(tot), Some(ttft)) if tot > ttft && (tot - ttft) >= MIN_TPS_DURATION_MS => {
            Some(tot - ttft)
        }
        (Some(tot), _) => Some(tot),
        _ => None,
    };
    let tps = gen_ms.and_then(|d| compute_tps(row.output_tokens, d));

    let updated_ms = if row.created_at > 0 {
        // `created_at` in cc-switch.db is Unix seconds (10 digits); accept ms if already 13 digits.
        if row.created_at < 10_000_000_000 {
            row.created_at * 1000
        } else {
            row.created_at
        }
    } else {
        now_ms
    };

    SessionMetrics {
        context_used_tokens: ctx_used_opt,
        context_window_tokens: ctx_window,
        context_used_pct: ctx_pct,
        tps,
        ttft_ms,
        last_output_tokens: (row.output_tokens > 0).then_some(row.output_tokens),
        last_duration_ms: gen_ms,
        total_input_tokens: (row.total_input_tokens > 0).then_some(row.total_input_tokens),
        total_output_tokens: (row.total_output_tokens > 0).then_some(row.total_output_tokens),
        source: "cc_switch".into(),
        updated_ms,
    }
}

/// Read top-level `model_context_window` from `~/.codex/config.toml` if present.
pub(crate) fn read_codex_default_context_window(home: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(home.join(".codex/config.toml")).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("model_context_window")
        .and_then(toml::Value::as_integer)
        .and_then(|v| (v > 0).then_some(v as u64))
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
    }
}

// ---------------------------------------------------------------------------
// Exact existing-pane session/transcript resolvers (Claude, Codex, AGY)
// ---------------------------------------------------------------------------

/// Exact Claude subtree PIDs → `(session_id, transcript_path)` via `~/.claude/sessions/<pid>.json`.
pub(crate) fn resolve_claude_transcript_by_pids(pids: &[u32]) -> Option<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    for &pid in pids {
        if let Some(hit) = resolve_claude_transcript_by_pid_in(&home, pid) {
            return Some(hit);
        }
    }
    None
}

fn resolve_claude_transcript_by_pid_in(home: &Path, pid: u32) -> Option<(String, PathBuf)> {
    let sess_file = home.join(format!(".claude/sessions/{pid}.json"));
    let text = std::fs::read_to_string(&sess_file).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let sid = v.get("sessionId").and_then(Value::as_str)?.trim();
    if sid.is_empty() || sid.contains('/') || sid.contains("..") {
        return None;
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

/// Exact Codex resume UUID → rollout JSONL resolution under `~/.codex/sessions/YYYY/MM/DD/*<uuid>.jsonl`.
pub(crate) fn resolve_codex_transcript_by_id(uuid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    resolve_codex_transcript_by_id_in(&home, uuid)
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
pub(crate) struct CodexThreadRow {
    pub id: String,
    #[serde(default)]
    pub rollout_path: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct CodexThreadCache {
    sig: Option<DbSig>,
    rows: Vec<CodexThreadRow>,
}

impl CodexThreadCache {
    pub(crate) fn refresh_if_needed(&mut self) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let db_path = home.join(".codex/state_5.sqlite");
        let sig = db_signature(&db_path);
        if sig.db.is_none() || self.sig.as_ref() == Some(&sig) {
            return;
        }
        let sql = "SELECT id, rollout_path, cwd, name, COALESCE(archived, 0) AS archived, COALESCE(updated_at, 0) AS updated_at FROM threads WHERE rollout_path IS NOT NULL AND rollout_path != '' ORDER BY archived ASC, updated_at DESC LIMIT 2000;";
        let Some(json_out) = run_sqlite_json(&db_path, sql) else {
            return;
        };
        let Ok(rows) = serde_json::from_str::<Vec<CodexThreadRow>>(&json_out) else {
            return;
        };
        self.rows = rows;
        self.sig = Some(sig);
    }

    pub(crate) fn match_pane(
        &self,
        cwd: &str,
        tail: &str,
        claimed_ids: &HashSet<String>,
    ) -> Option<(String, PathBuf)> {
        match_codex_thread(&self.rows, cwd, tail, claimed_ids)
    }
}

/// Extract Codex bottom status-bar thread title (3rd ` · ` segment of the model/path/title line).
pub(crate) fn extract_codex_status_title(tail: &str) -> Option<String> {
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
        let lower = line.to_ascii_lowercase();
        if line.contains(" · ")
            && (lower.contains("gpt-")
                || lower.contains("o3")
                || lower.contains("o4")
                || line.contains("~/"))
        {
            let parts: Vec<&str> = line.split(" · ").collect();
            if parts.len() >= 3 {
                // Strip right-aligned status text separated by multiple spaces (e.g. `Goal stalled ...`).
                let raw = parts[2].split("  ").next().unwrap_or("").trim();
                if !raw.is_empty() {
                    return Some(raw.to_string());
                }
            }
        }
    }
    None
}

/// Strictly match a Codex pane against `state_5.sqlite` threads using its status-line title + `cwd`,
/// skipping any thread id in `claimed_ids`. Never guesses on blank/new panes (no title hint → `None`).
pub(crate) fn match_codex_thread(
    rows: &[CodexThreadRow],
    cwd: &str,
    tail: &str,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    let hint = extract_codex_status_title(tail)?;
    let hint_clean = hint.trim_end_matches(['…', '.']).trim();
    if hint_clean.is_empty() {
        return None;
    }
    let norm_cwd = cwd.trim_end_matches('/');

    let resolve_row = |r: &CodexThreadRow| -> Option<(String, PathBuf)> {
        if claimed_ids.contains(r.id.trim()) {
            return None;
        }
        let p = PathBuf::from(r.rollout_path.as_deref()?);
        if p.is_file() {
            Some((r.id.clone(), p))
        } else {
            None
        }
    };

    // Pass 1: exact `name == hint` AND matching `cwd`.
    for r in rows {
        let nm = r.name.as_deref().unwrap_or("").trim();
        if !nm.is_empty() && nm == hint && r.cwd.trim_end_matches('/') == norm_cwd {
            if let Some(hit) = resolve_row(r) {
                return Some(hit);
            }
        }
    }
    // Pass 2: exact `name == hint` across any cwd (e.g. symlinked/worktree path difference).
    for r in rows {
        let nm = r.name.as_deref().unwrap_or("").trim();
        if !nm.is_empty() && nm == hint {
            if let Some(hit) = resolve_row(r) {
                return Some(hit);
            }
        }
    }
    // Pass 3: truncated title prefix match (`name.starts_with(hint_clean)`) ONLY when hint >= 8 chars + matching cwd.
    if hint_clean.chars().count() >= 8 {
        for r in rows {
            let nm = r.name.as_deref().unwrap_or("").trim();
            if nm.starts_with(hint_clean) && r.cwd.trim_end_matches('/') == norm_cwd {
                if let Some(hit) = resolve_row(r) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// Cached read-only snapshot of `~/.gemini/antigravity-cli/conversation_summaries.db` root conversations.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyConvRow {
    pub conversation_id: String,
    #[serde(default)]
    pub workspace_uris: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyChildRow {
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
    pub state: baton_proto::SessionState,
    pub transcript_path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct AgyConvCache {
    sig: Option<DbSig>,
    rows: Vec<AgyConvRow>,
    children: Vec<AgyChildRow>,
}

impl AgyConvCache {
    pub(crate) fn refresh_if_needed(&mut self) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let db_path = home.join(".gemini/antigravity-cli/conversation_summaries.db");
        let sig = db_signature(&db_path);
        if sig.db.is_none() || self.sig.as_ref() == Some(&sig) {
            return;
        }
        let sql = "SELECT conversation_id, COALESCE(workspace_uris, '') AS workspace_uris, COALESCE(status, '') AS status FROM conversation_summaries WHERE nesting_depth = 0 ORDER BY last_modified_time DESC LIMIT 120;";
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

    /// Match a discovered AGY pane to its most recent unclaimed root conversation ONLY when it
    /// is the sole unclaimed live AGY pane in `cwd` (zero cross-pane misattribution).
    pub(crate) fn match_unique_cwd_excluding(
        &self,
        cwd: &str,
        unique_in_cwd: bool,
        claimed_ids: &HashSet<String>,
    ) -> Option<(String, PathBuf)> {
        if !unique_in_cwd {
            return None;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        match_agy_conversation_in(&home, &self.rows, cwd, claimed_ids)
    }

    /// Return Swarm / subagent child conversations whose `parent_conversation_id == parent_id`
    /// and whose `transcript.jsonl` exists on disk.
    pub(crate) fn children_of(&self, parent_id: &str) -> Vec<AgyDiscoveredChild> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
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
                baton_proto::SessionState::Ended
            } else if c.status.contains("RUNNING") {
                baton_proto::SessionState::Working
            } else {
                baton_proto::SessionState::Idle
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

pub(crate) fn resolve_agy_transcript_by_id(conv_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    resolve_agy_transcript_by_id_in(&home, conv_id)
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

fn match_agy_conversation_in(
    home: &Path,
    rows: &[AgyConvRow],
    cwd: &str,
    claimed_ids: &HashSet<String>,
) -> Option<(String, PathBuf)> {
    let target_uri = format!("file://{}", cwd.trim_end_matches('/'));
    for r in rows {
        let sid = r.conversation_id.trim();
        if sid.is_empty() || sid.contains('/') || sid.contains("..") || claimed_ids.contains(sid) {
            continue;
        }
        if !r.workspace_uris.contains(&target_uri) {
            continue;
        }
        // Check exact URI match inside JSON array string to avoid `/Users/demo` matching `/Users/demo/work/app-b`.
        let quoted = format!("\"{target_uri}\"");
        let quoted_slash = format!("\"{target_uri}/\"");
        if !r.workspace_uris.contains(&quoted) && !r.workspace_uris.contains(&quoted_slash) {
            continue;
        }
        let tr = home
            .join(".gemini/antigravity-cli/brain")
            .join(sid)
            .join(".system_generated/logs/transcript.jsonl");
        if tr.is_file() {
            return Some((sid.to_string(), tr));
        }
    }
    None
}

/// Derive the canonical external `session_id` (used as key in `cc-switch.db`) for `session`.
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

// ---------------------------------------------------------------------------
// Core JSONL metrics extraction (pure, zero IO)
// ---------------------------------------------------------------------------

pub(crate) fn extract_transcript_metrics(
    agent: AgentKind,
    text: &str,
    now_ms: u64,
) -> Option<SessionMetrics> {
    match agent {
        AgentKind::Claude => extract_claude_metrics(text, now_ms),
        AgentKind::Codex => extract_codex_metrics(text, now_ms),
        AgentKind::Agy => extract_agy_metrics(text, now_ms),
    }
}

fn compute_tps(output_tokens: u64, duration_ms: u64) -> Option<f32> {
    if output_tokens == 0 || !(MIN_TPS_DURATION_MS..=MAX_TPS_DURATION_MS).contains(&duration_ms) {
        return None;
    }
    let raw = (output_tokens as f64) / ((duration_ms as f64) / 1000.0);
    if !raw.is_finite() || raw <= 0.0 || raw > MAX_PLAUSIBLE_TPS {
        return None;
    }
    Some(((raw * 10.0).round() / 10.0) as f32)
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
// Claude JSONL metrics
// ---------------------------------------------------------------------------

struct ClaudeMsgStats {
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    prev_ts: Option<u64>,
    context_tokens: u64,
    output_tokens: u64,
    model: Option<String>,
}

fn extract_claude_metrics(text: &str, now_ms: u64) -> Option<SessionMetrics> {
    let mut order: Vec<String> = Vec::new();
    let mut by_msg: HashMap<String, ClaudeMsgStats> = HashMap::new();
    let mut prev_event_ts: Option<u64> = None;
    let mut latest_event_ts: Option<u64> = None;
    let mut seq: u64 = 0;

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            latest_event_ts = Some(latest_event_ts.map_or(t, |cur| cur.max(t)));
        }

        if kind == "assistant" {
            if let Some(msg) = obj.get("message") {
                let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
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
                            let id = msg
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    seq += 1;
                                    format!("anon-{seq}")
                                });
                            let entry = by_msg.entry(id.clone()).or_insert_with(|| {
                                order.push(id);
                                ClaudeMsgStats {
                                    first_ts: ts_ms,
                                    last_ts: ts_ms,
                                    prev_ts: prev_event_ts,
                                    context_tokens: 0,
                                    output_tokens: 0,
                                    model: (!model.is_empty()).then(|| model.to_string()),
                                }
                            });
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

        if let Some(t) = ts_ms {
            if matches!(kind, "user" | "assistant" | "system") {
                prev_event_ts = Some(t);
            }
        }
    }

    if order.is_empty() {
        return None;
    }

    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut latest_ctx: Option<u64> = None;
    let mut latest_model: Option<String> = None;
    let mut best_tps: Option<(f32, u64, u64)> = None;

    for id in &order {
        let Some(m) = by_msg.get(id) else { continue };
        total_in = total_in.saturating_add(m.context_tokens);
        total_out = total_out.saturating_add(m.output_tokens);
        if m.context_tokens > 0 {
            latest_ctx = Some(m.context_tokens);
            if m.model.is_some() {
                latest_model = m.model.clone();
            }
        }
        if m.output_tokens > 0 {
            let chunk_span = match (m.first_ts, m.last_ts) {
                (Some(a), Some(b)) if b > a => b - a,
                _ => 0,
            };
            let call_span = match (m.prev_ts, m.last_ts) {
                (Some(a), Some(b)) if b > a => b - a,
                _ => 0,
            };
            let dur = if (MIN_TPS_DURATION_MS..=MAX_TPS_DURATION_MS).contains(&call_span) {
                call_span
            } else {
                chunk_span
            };
            if let Some(tps) = compute_tps(m.output_tokens, dur) {
                best_tps = Some((tps, m.output_tokens, dur));
            }
        }
    }

    let ctx_window =
        infer_context_window(AgentKind::Claude, latest_model.as_deref(), latest_ctx, None);
    let ctx_pct = match (latest_ctx, ctx_window) {
        (Some(u), Some(w)) => pct_1dp(u, w),
        _ => None,
    };

    Some(SessionMetrics {
        context_used_tokens: latest_ctx,
        context_window_tokens: ctx_window,
        context_used_pct: ctx_pct,
        tps: best_tps.map(|x| x.0),
        ttft_ms: None,
        last_output_tokens: best_tps.map(|x| x.1),
        last_duration_ms: best_tps.map(|x| x.2),
        total_input_tokens: (total_in > 0).then_some(total_in),
        total_output_tokens: (total_out > 0).then_some(total_out),
        source: "transcript".into(),
        updated_ms: latest_event_ts.unwrap_or(now_ms),
    })
}

// ---------------------------------------------------------------------------
// Codex JSONL metrics
// ---------------------------------------------------------------------------

fn extract_codex_metrics(text: &str, now_ms: u64) -> Option<SessionMetrics> {
    let mut ctx_used: Option<u64> = None;
    let mut ctx_window: Option<u64> = None;
    let mut total_in: Option<u64> = None;
    let mut total_out: Option<u64> = None;

    let mut call_start_ts: Option<u64> = None;
    let mut latest_event_ts: Option<u64> = None;
    let mut last_tps: Option<(f32, u64, u64)> = None;
    let mut prev_total_out: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            latest_event_ts = Some(latest_event_ts.map_or(t, |cur| cur.max(t)));
        }
        let payload = obj.get("payload");
        let ptype = payload
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");

        match (kind, ptype) {
            ("response_item", "message") => {
                if payload.and_then(|p| p.get("role")).and_then(Value::as_str) == Some("user") {
                    if let Some(t) = ts_ms {
                        call_start_ts = Some(t);
                    }
                }
            }
            ("response_item", "custom_tool_call_output" | "function_call_output") => {
                if let Some(t) = ts_ms {
                    call_start_ts = Some(t);
                }
            }
            ("event_msg", "token_count") => {
                let Some(info) = payload
                    .and_then(|p| p.get("info"))
                    .filter(|v| v.is_object())
                else {
                    continue;
                };
                if let Some(win) = info.get("model_context_window").and_then(Value::as_u64) {
                    if win > 0 {
                        ctx_window = Some(win);
                    }
                }
                if let Some(tot) = info.get("total_token_usage") {
                    if let Some(v) = tot.get("input_tokens").and_then(Value::as_u64) {
                        total_in = Some(v);
                    }
                    if let Some(v) = tot.get("output_tokens").and_then(Value::as_u64) {
                        total_out = Some(v);
                    }
                }
                if let Some(last) = info.get("last_token_usage") {
                    let used = last
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .or_else(|| last.get("total_tokens").and_then(Value::as_u64));
                    if let Some(u) = used.filter(|&u| u > 0) {
                        ctx_used = Some(u);
                    }
                    let out_tok = last
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let changed = total_out != prev_total_out;
                    if changed && out_tok > 0 {
                        if let (Some(start), Some(end)) = (call_start_ts, ts_ms) {
                            let dur = end.saturating_sub(start);
                            if let Some(tps) = compute_tps(out_tok, dur) {
                                last_tps = Some((tps, out_tok, dur));
                            }
                        }
                    }
                    prev_total_out = total_out;
                    if let Some(t) = ts_ms {
                        call_start_ts = Some(t);
                    }
                }
            }
            ("event_msg", "task_complete") if last_tps.is_none() => {
                if let (Some(dur), Some(out_tok)) = (
                    payload
                        .and_then(|p| p.get("duration_ms"))
                        .and_then(Value::as_u64),
                    total_out.filter(|&v| v > 0),
                ) {
                    if let Some(tps) = compute_tps(out_tok, dur) {
                        last_tps = Some((tps, out_tok, dur));
                    }
                }
            }
            _ => {}
        }
    }

    if ctx_used.is_none() && ctx_window.is_none() && total_in.is_none() && last_tps.is_none() {
        return None;
    }

    let ctx_pct = match (ctx_used, ctx_window) {
        (Some(u), Some(w)) => pct_1dp(u, w),
        _ => None,
    };

    Some(SessionMetrics {
        context_used_tokens: ctx_used,
        context_window_tokens: ctx_window,
        context_used_pct: ctx_pct,
        tps: last_tps.map(|x| x.0),
        ttft_ms: None,
        last_output_tokens: last_tps.map(|x| x.1),
        last_duration_ms: last_tps.map(|x| x.2),
        total_input_tokens: total_in,
        total_output_tokens: total_out,
        source: "transcript".into(),
        updated_ms: latest_event_ts.unwrap_or(now_ms),
    })
}

// ---------------------------------------------------------------------------
// AGY transcript.jsonl metrics
// ---------------------------------------------------------------------------

fn extract_agy_metrics(text: &str, now_ms: u64) -> Option<SessionMetrics> {
    let mut latest_ctx: Option<u64> = None;
    let mut latest_model: Option<String> = None;
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut last_tps: Option<(f32, u64, u64)> = None;
    let mut pending_pr: Option<(u64, u64)> = None;
    let mut latest_event_ts: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = obj
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms);
        if let Some(t) = ts_ms {
            latest_event_ts = Some(latest_event_ts.map_or(t, |cur| cur.max(t)));
        }

        if let (Some((pr_start, pr_out)), Some(next_ts)) = (pending_pr.take(), ts_ms) {
            if kind != "USER_INPUT" {
                let dur = next_ts.saturating_sub(pr_start);
                if let Some(tps) = compute_tps(pr_out, dur) {
                    last_tps = Some((tps, pr_out, dur));
                }
            }
        }

        if kind == "PLANNER_RESPONSE" {
            if let Some(m) = obj
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                latest_model = Some(m.to_string());
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
                latest_ctx = Some(ctx);
                total_in = total_in.saturating_add(ctx);
            }
            if out > 0 {
                total_out = total_out.saturating_add(out);
                if let Some(t) = ts_ms {
                    pending_pr = Some((t, out));
                }
            }
        }
    }

    if latest_ctx.is_none() && total_out == 0 && last_tps.is_none() {
        return None;
    }

    let ctx_window =
        infer_context_window(AgentKind::Agy, latest_model.as_deref(), latest_ctx, None);
    let ctx_pct = match (latest_ctx, ctx_window) {
        (Some(u), Some(w)) => pct_1dp(u, w),
        _ => None,
    };

    Some(SessionMetrics {
        context_used_tokens: latest_ctx,
        context_window_tokens: ctx_window,
        context_used_pct: ctx_pct,
        tps: last_tps.map(|x| x.0),
        ttft_ms: None,
        last_output_tokens: last_tps.map(|x| x.1),
        last_duration_ms: last_tps.map(|x| x.2),
        total_input_tokens: (total_in > 0).then_some(total_in),
        total_output_tokens: (total_out > 0).then_some(total_out),
        source: "transcript".into(),
        updated_ms: latest_event_ts.unwrap_or(now_ms),
    })
}

// ---------------------------------------------------------------------------
// Status line fallback parser (Claude / Codex)
// ---------------------------------------------------------------------------

/// Parse context-window percentage and explicit `(1M context)`/`[1M]` tags from the
/// bottom lines of a `capture-pane` snapshot. Never invents values.
pub(crate) fn parse_status_line_metrics(
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
        last_output_tokens: None,
        last_duration_ms: None,
        total_input_tokens: None,
        total_output_tokens: None,
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

/// Priority merger: `cc_switch` (streaming TPS + TTFT) > `transcript` > `status_line`.
/// Enriches `cc_switch` with exact per-session `model_context_window` from transcript (e.g. Codex)
/// or explicit `(1M context)` / `N% context left` from status line.
pub(crate) fn merge_cc_switch_with_fallback(
    cc: Option<SessionMetrics>,
    transcript: Option<SessionMetrics>,
    status: Option<SessionMetrics>,
) -> Option<SessionMetrics> {
    let fallback = merge_metrics(transcript, status);
    match (cc, fallback) {
        (Some(mut c), Some(fb)) => {
            if fb.source == "transcript" && fb.updated_ms > c.updated_ms.saturating_add(15_000) {
                c.context_used_tokens = fb.context_used_tokens.or(c.context_used_tokens);
                c.context_window_tokens = fb.context_window_tokens.or(c.context_window_tokens);
                if let (Some(u), Some(w)) = (c.context_used_tokens, c.context_window_tokens) {
                    c.context_used_pct = pct_1dp(u, w);
                } else if fb.context_used_pct.is_some() {
                    c.context_used_pct = fb.context_used_pct;
                }
                c.tps = fb.tps.or(c.tps);
                c.last_output_tokens = fb.last_output_tokens.or(c.last_output_tokens);
                c.last_duration_ms = fb.last_duration_ms.or(c.last_duration_ms);
                c.total_input_tokens = fb.total_input_tokens.or(c.total_input_tokens);
                c.total_output_tokens = fb.total_output_tokens.or(c.total_output_tokens);
                c.updated_ms = fb.updated_ms;
                return Some(c);
            }
            // Prefer exact per-rollout window (e.g. Codex 258.4K / 828.4K) or fill missing window.
            if (fb.source == "transcript" && fb.context_window_tokens.is_some())
                || c.context_window_tokens.is_none()
            {
                c.context_window_tokens = fb.context_window_tokens;
            }
            if c.context_used_tokens.is_none() {
                c.context_used_tokens = fb.context_used_tokens;
            }
            if let (Some(u), Some(w)) = (c.context_used_tokens, c.context_window_tokens) {
                c.context_used_pct = pct_1dp(u, w);
            } else if c.context_used_pct.is_none() {
                c.context_used_pct = fb.context_used_pct;
            }
            if c.tps.is_none() {
                c.tps = fb.tps;
                c.last_output_tokens = c.last_output_tokens.or(fb.last_output_tokens);
                c.last_duration_ms = c.last_duration_ms.or(fb.last_duration_ms);
            }
            if c.total_input_tokens.is_none() {
                c.total_input_tokens = fb.total_input_tokens;
            }
            if c.total_output_tokens.is_none() {
                c.total_output_tokens = fb.total_output_tokens;
            }
            Some(c)
        }
        (Some(c), None) => Some(c),
        (None, fb) => fb,
    }
}

/// Merge transcript metrics with optional status-line metrics.
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
                && x.last_output_tokens == y.last_output_tokens
                && x.last_duration_ms == y.last_duration_ms
                && x.total_input_tokens == y.total_input_tokens
                && x.total_output_tokens == y.total_output_tokens
                && x.source == y.source
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_switch_streaming_tps_and_input_token_semantics() {
        // AGY (semantics = 2 exclusive): input + cache_read = 284,077 / 512,000 = 55.5%,
        // streaming dur = 18922 - 5467 = 13455ms, tps = 2016 / 13.455 = 149.8 tok/s
        let agy_row = CcSwitchRow {
            session_id: "s-agy".into(),
            app_type: "agy".into(),
            model: Some("gemini-4-neon-high".into()),
            input_tokens: 3_829,
            output_tokens: 2_016,
            cache_read_tokens: 280_248,
            cache_creation_tokens: 0,
            latency_ms: Some(18_922),
            first_token_ms: Some(5_467),
            duration_ms: None,
            input_token_semantics: 2,
            created_at: 1_795_237_572,
            total_input_tokens: 900_000,
            total_output_tokens: 12_000,
        };
        let m_agy = cc_switch_row_to_metrics(&agy_row, Some(872_000), 999);
        assert_eq!(m_agy.source, "cc_switch");
        assert_eq!(m_agy.context_used_tokens, Some(284_077));
        assert_eq!(m_agy.context_window_tokens, Some(512_000));
        assert_eq!(m_agy.context_used_pct, Some(55.5));
        assert_eq!(m_agy.ttft_ms, Some(5_467));
        assert_eq!(m_agy.last_duration_ms, Some(13_455));
        assert_eq!(m_agy.tps, Some(149.8));

        // Codex (semantics = 1 INCLUSIVE): input_tokens ALREADY includes cache_read_tokens!
        let cx_row = CcSwitchRow {
            session_id: "s-cx".into(),
            app_type: "codex".into(),
            model: Some("gpt-6.2-sol".into()),
            input_tokens: 156_646,
            output_tokens: 873,
            cache_read_tokens: 155_776,
            cache_creation_tokens: 0,
            latency_ms: Some(21_660),
            first_token_ms: Some(5_768),
            duration_ms: None,
            input_token_semantics: 1,
            created_at: 1_795_235_354,
            total_input_tokens: 7_500_000,
            total_output_tokens: 22_000,
        };
        let m_cx = cc_switch_row_to_metrics(&cx_row, Some(872_000), 999);
        assert_eq!(m_cx.context_used_tokens, Some(156_646)); // NOT 312,422!
        assert_eq!(m_cx.context_window_tokens, Some(872_000));
        assert_eq!(m_cx.context_used_pct, Some(18.0));
        assert_eq!(m_cx.ttft_ms, Some(5_768));
        assert_eq!(m_cx.last_duration_ms, Some(15_892));
        assert_eq!(m_cx.tps, Some(54.9));

        // Claude (semantics = 0 exclusive, >200K → 1M window, fallback to total_ms when first_token_ms is None).
        let cl_row = CcSwitchRow {
            session_id: "s-cl".into(),
            app_type: "claude".into(),
            model: Some("claude-fable-5-2".into()),
            input_tokens: 2,
            output_tokens: 1_331,
            cache_read_tokens: 568_048,
            cache_creation_tokens: 8_693,
            latency_ms: Some(20_000),
            first_token_ms: None,
            duration_ms: None,
            input_token_semantics: 0,
            created_at: 1_795_032_801,
            total_input_tokens: 1_200_000,
            total_output_tokens: 10_000,
        };
        let m_cl = cc_switch_row_to_metrics(&cl_row, None, 999);
        assert_eq!(m_cl.context_used_tokens, Some(576_743));
        assert_eq!(m_cl.context_window_tokens, Some(1_000_000));
        assert_eq!(m_cl.context_used_pct, Some(57.7));
        assert_eq!(m_cl.ttft_ms, None);
        assert_eq!(m_cl.tps, Some(66.6));
    }

    #[test]
    fn codex_thread_matching_uses_status_title_and_avoids_short_false_positives() {
        let dir = std::env::temp_dir().join(format!("baton-cxm-{}", baton_proto::id::new_id("T")));
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
            },
            CodexThreadRow {
                id: "01a18191-2222-2222-2222-222222222222".into(),
                rollout_path: Some(f2.to_string_lossy().into_owned()),
                cwd: "/Users/demo/work/app-c".into(),
                name: Some("查".into()),
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
        assert_eq!(compute_tps(0, 1000), None);
        assert_eq!(compute_tps(100, 0), None);
        assert_eq!(compute_tps(100, 50), None);
        assert_eq!(compute_tps(100, 400_000), None);
        assert_eq!(compute_tps(500, 2_000), Some(250.0));
    }

    #[test]
    fn file_cache_hits_when_mtime_and_len_unchanged() {
        let dir = std::env::temp_dir().join(format!("baton-mc-{}", baton_proto::id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.jsonl");
        std::fs::write(
            &file,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-21T04:44:02Z","input_tokens":1000,"cache_read_tokens":500,"output_tokens":100}
{"step_index":2,"type":"GENERIC","created_at":"2026-11-21T04:44:04Z","content":"ok"}
"#,
        )
        .unwrap();

        let ts_initial = parse_iso8601_ms("2026-11-21T04:44:04Z").unwrap();
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
            std::env::temp_dir().join(format!("baton-home-{}", baton_proto::id::new_id("T")));
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
        }];
        let no_claimed = HashSet::new();
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
            "baton-effort-test-{}",
            baton_proto::id::new_id("T")
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
            "baton-agy-presence-{}",
            baton_proto::id::new_id("T")
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
}
