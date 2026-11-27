//! Persistent idempotent daily & per-session token consumption ledger (`<state_dir>/usage_daily.json`).
//!
//! Design invariants:
//! - Every entry in `UsageLedgerData.sources` is keyed by a stable source key (canonical transcript
//!   file path for local transcripts, or `remote:<session_id>` for remote/non-file sessions).
//!   Re-upserting replaces that source's daily buckets cleanly, preventing double-counting.
//! - Live updates ride piggy-backed on `sync_discovered_panes`'s existing `TranscriptIndex` sync
//!   only when `(file_len, mtime_ns)` actually changed (0 extra steady-state stat/read IO).
//! - Startup backfill runs once in a throttled background task (~15 ms pause between parsed files)
//!   over the last 35 days of local transcript files, skipping any whose `(file_len, mtime_ns)` is
//!   already recorded cleanly on disk.

use super::metrics::{
    epoch_ms_to_ymd, local_tz_offset_info, parse_transcript_usage_file, ymd_shift_days,
    DailyTokenCounts,
};
use super::{id, lock, Core};
use crate::store::write_private;
use serde::{Deserialize, Serialize};
use starfane_proto::{
    AgentKind, DailyUsageBucket, SessionUsageRankItem, TokenUsageTotals, UsageSummaryResponse,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const USAGE_LEDGER_VERSION: u32 = 1;
const USAGE_FILE_NAME: &str = "usage_daily.json";
const USAGE_FLUSH_INTERVAL_MS: u64 = 15_000;
const BACKFILL_DAYS: i64 = 35;
const BACKFILL_MAX_FILES: usize = 1_500;
const BACKFILL_SLEEP_PER_FILE: Duration = Duration::from_millis(15);

fn default_usage_version() -> u32 {
    USAGE_LEDGER_VERSION
}

pub(crate) type LiveSessionMetaTuple = (Option<String>, Option<String>, bool);

#[derive(Debug, Clone)]
pub(crate) struct LiveSessionMeta {
    pub session_id: String,
    pub label: Option<String>,
    pub model: Option<String>,
    pub active: bool,
}

pub(crate) struct RemoteUsageUpdate<'a> {
    pub source_key: String,
    pub agent: AgentKind,
    pub session_id: String,
    pub label: Option<String>,
    pub model: Option<String>,
    pub now_ms: u64,
    pub today_ymd: &'a str,
    pub counts: DailyTokenCounts,
}

pub(crate) struct TranscriptUsageRecord<'a> {
    pub source_path: &'a Path,
    pub agent: AgentKind,
    pub session_id: &'a str,
    pub label: Option<&'a str>,
    pub model: Option<&'a str>,
    pub file_len: u64,
    pub mtime_ns: u128,
    pub last_active_ms: u64,
    pub daily: BTreeMap<String, DailyTokenCounts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageSourceEntry {
    pub agent: AgentKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub file_len: u64,
    #[serde(default)]
    pub mtime_ns: u128,
    #[serde(default)]
    pub last_active_ms: u64,
    #[serde(default)]
    pub daily: BTreeMap<String, DailyTokenCounts>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct UsageLedgerData {
    #[serde(default = "default_usage_version")]
    pub v: u32,
    #[serde(default)]
    pub tz_offset: String,
    #[serde(default)]
    pub sources: HashMap<String, UsageSourceEntry>,
}

#[derive(Debug)]
pub(crate) struct UsageLedger {
    path: PathBuf,
    data: UsageLedgerData,
    dirty: bool,
    last_flushed_ms: u64,
}

impl UsageLedger {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(USAGE_FILE_NAME);
        let (_, cur_tz) = local_tz_offset_info();
        let data = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<UsageLedgerData>(&raw) {
                Ok(mut parsed) => {
                    if parsed.tz_offset.is_empty() {
                        parsed.tz_offset = cur_tz;
                    }
                    parsed
                }
                Err(e) => {
                    log::warn!(
                        "usage ledger {} unreadable ({e}); rebuilding",
                        path.display()
                    );
                    UsageLedgerData {
                        v: USAGE_LEDGER_VERSION,
                        tz_offset: cur_tz,
                        sources: HashMap::new(),
                    }
                }
            },
            Err(_) => UsageLedgerData {
                v: USAGE_LEDGER_VERSION,
                tz_offset: cur_tz,
                sources: HashMap::new(),
            },
        };
        Self {
            path,
            data,
            dirty: false,
            last_flushed_ms: 0,
        }
    }

    pub fn tz_mismatch(&self) -> bool {
        let (_, cur_tz) = local_tz_offset_info();
        !self.data.tz_offset.is_empty() && self.data.tz_offset != cur_tz
    }

    pub fn source_matches_sig(&self, source_key: &str, file_len: u64, mtime_ns: u128) -> bool {
        if self.tz_mismatch() {
            return false;
        }
        self.data
            .sources
            .get(source_key)
            .is_some_and(|e| e.file_len == file_len && e.mtime_ns == mtime_ns)
    }

    pub fn upsert_source(&mut self, source_key: String, mut entry: UsageSourceEntry) -> bool {
        entry.daily.retain(|_, v| !v.is_zero());
        if entry.daily.is_empty() {
            return false;
        }
        let (_, cur_tz) = local_tz_offset_info();
        if self.data.tz_offset != cur_tz {
            self.data.tz_offset = cur_tz;
            self.dirty = true;
        }
        if self.data.sources.get(&source_key) == Some(&entry) {
            return false;
        }
        self.data.sources.insert(source_key, entry);
        self.dirty = true;
        true
    }

    /// Upsert monotonic remote cumulative usage so multi-day sessions add ONLY today's positive delta
    /// `(cur - prev_sum)` into today's bucket instead of moving historical day totals into today.
    pub fn upsert_remote_cumulative(&mut self, upd: RemoteUsageUpdate<'_>) -> bool {
        let cur_in = upd.counts.input_tokens;
        let cur_out = upd.counts.output_tokens;
        let cur_cr = upd.counts.cache_read_tokens;
        if cur_in == 0 && cur_out == 0 && cur_cr == 0 {
            return false;
        }
        let (_, cur_tz) = local_tz_offset_info();
        if self.data.tz_offset != cur_tz {
            self.data.tz_offset = cur_tz;
            self.dirty = true;
        }
        match self.data.sources.get_mut(&upd.source_key) {
            Some(existing) => {
                let mut sum_in = 0u64;
                let mut sum_out = 0u64;
                let mut sum_cr = 0u64;
                for c in existing.daily.values() {
                    sum_in = sum_in.saturating_add(c.input_tokens);
                    sum_out = sum_out.saturating_add(c.output_tokens);
                    sum_cr = sum_cr.saturating_add(c.cache_read_tokens);
                }
                let mut changed = false;
                if existing.label != upd.label {
                    existing.label = upd.label;
                    changed = true;
                }
                if existing.model != upd.model && upd.model.is_some() {
                    existing.model = upd.model;
                    changed = true;
                }
                if cur_in < sum_in || cur_out < sum_out || cur_cr < sum_cr {
                    // Counter reset (new rollout / compaction): replace cleanly into today's bucket.
                    let mut daily = BTreeMap::new();
                    daily.insert(
                        upd.today_ymd.to_string(),
                        DailyTokenCounts {
                            input_tokens: cur_in,
                            output_tokens: cur_out,
                            cache_read_tokens: cur_cr,
                        },
                    );
                    existing.daily = daily;
                    existing.last_active_ms = upd.now_ms;
                    changed = true;
                } else {
                    let d_in = cur_in.saturating_sub(sum_in);
                    let d_out = cur_out.saturating_sub(sum_out);
                    let d_cr = cur_cr.saturating_sub(sum_cr);
                    if d_in > 0 || d_out > 0 || d_cr > 0 {
                        existing
                            .daily
                            .entry(upd.today_ymd.to_string())
                            .or_default()
                            .add_counts(d_in, d_out, d_cr);
                        existing.last_active_ms = upd.now_ms;
                        changed = true;
                    }
                }
                if changed {
                    self.dirty = true;
                }
                changed
            }
            None => {
                let mut daily = BTreeMap::new();
                daily.insert(
                    upd.today_ymd.to_string(),
                    DailyTokenCounts {
                        input_tokens: cur_in,
                        output_tokens: cur_out,
                        cache_read_tokens: cur_cr,
                    },
                );
                self.data.sources.insert(
                    upd.source_key,
                    UsageSourceEntry {
                        agent: upd.agent,
                        session_id: upd.session_id,
                        label: upd.label,
                        model: upd.model,
                        file_len: 0,
                        mtime_ns: 0,
                        last_active_ms: upd.now_ms,
                        daily,
                    },
                );
                self.dirty = true;
                true
            }
        }
    }

    pub fn today_totals(&self, now_ms: u64) -> TokenUsageTotals {
        let (tz_ms, _) = local_tz_offset_info();
        let today = epoch_ms_to_ymd(now_ms, tz_ms);
        let mut tot = TokenUsageTotals::default();
        for src in self.data.sources.values() {
            if let Some(d) = src.daily.get(&today) {
                tot.add_assign(&TokenUsageTotals::from_counts(
                    d.input_tokens,
                    d.output_tokens,
                    d.cache_read_tokens,
                ));
            }
        }
        tot
    }

    pub fn build_summary(
        &self,
        days: usize,
        now_ms: u64,
        live_meta: &HashMap<String, LiveSessionMetaTuple>,
        live_by_path: &HashMap<String, LiveSessionMeta>,
    ) -> UsageSummaryResponse {
        let days = days.clamp(1, 90);
        let (tz_ms, tz_offset) = local_tz_offset_info();
        let today_date = epoch_ms_to_ymd(now_ms, tz_ms);

        // Build chronological calendar list `[today - (days - 1) ..= today]` (with at least 30-day lookback for KPI cards).
        let lookback = days.max(30);
        let mut all_dates: Vec<String> = Vec::with_capacity(lookback);
        for offset in (0..lookback).rev() {
            if let Some(ymd) = ymd_shift_days(&today_date, -(offset as i64)) {
                all_dates.push(ymd);
            }
        }
        if all_dates.is_empty() {
            all_dates.push(today_date.clone());
        }
        let range_start_idx = all_dates.len().saturating_sub(days);
        let range_dates: Vec<String> = all_dates[range_start_idx..].to_vec();
        let start_7 = ymd_shift_days(&today_date, -6).unwrap_or_else(|| today_date.clone());
        let start_30 = ymd_shift_days(&today_date, -29).unwrap_or_else(|| today_date.clone());
        let range_start = range_dates
            .first()
            .cloned()
            .unwrap_or_else(|| today_date.clone());

        let mut buckets: BTreeMap<String, DailyUsageBucket> = range_dates
            .iter()
            .map(|date| {
                (
                    date.clone(),
                    DailyUsageBucket {
                        date: date.clone(),
                        totals: TokenUsageTotals::default(),
                        by_agent: BTreeMap::new(),
                        session_count: 0,
                    },
                )
            })
            .collect();
        let mut day_sessions: HashMap<String, HashSet<String>> = HashMap::new();
        let mut today = TokenUsageTotals::default();
        let mut last_7_days = TokenUsageTotals::default();
        let mut last_30_days = TokenUsageTotals::default();
        let mut range_totals = TokenUsageTotals::default();
        let mut by_agent_range: BTreeMap<String, TokenUsageTotals> = BTreeMap::new();
        let mut by_session: HashMap<String, SessionUsageRankItem> = HashMap::new();

        for (src_key, src) in &self.data.sources {
            let by_path_hit = live_by_path.get(src_key.as_str());
            let sid_key = if let Some(hit) = by_path_hit {
                hit.session_id.as_str()
            } else if src.session_id.is_empty() {
                src_key.as_str()
            } else {
                src.session_id.as_str()
            };
            let agent_key = src.agent.as_str().to_string();
            let mut sess_range_tot = TokenUsageTotals::default();

            for (ymd, cnt) in &src.daily {
                let item_tot = TokenUsageTotals::from_counts(
                    cnt.input_tokens,
                    cnt.output_tokens,
                    cnt.cache_read_tokens,
                );
                if item_tot.total_tokens == 0 && item_tot.cache_read_tokens == 0 {
                    continue;
                }
                if ymd == &today_date {
                    today.add_assign(&item_tot);
                }
                if ymd >= &start_7 && ymd <= &today_date {
                    last_7_days.add_assign(&item_tot);
                }
                if ymd >= &start_30 && ymd <= &today_date {
                    last_30_days.add_assign(&item_tot);
                }
                if ymd >= &range_start && ymd <= &today_date {
                    range_totals.add_assign(&item_tot);
                    by_agent_range
                        .entry(agent_key.clone())
                        .or_default()
                        .add_assign(&item_tot);
                    if let Some(bucket) = buckets.get_mut(ymd) {
                        bucket.totals.add_assign(&item_tot);
                        bucket
                            .by_agent
                            .entry(agent_key.clone())
                            .or_default()
                            .add_assign(&item_tot);
                        day_sessions
                            .entry(ymd.clone())
                            .or_default()
                            .insert(sid_key.to_string());
                    }
                    sess_range_tot.add_assign(&item_tot);
                }
            }

            if sess_range_tot.total_tokens > 0 || sess_range_tot.cache_read_tokens > 0 {
                let (live_label, live_model, live_active) = by_path_hit
                    .map(|hit| (hit.label.clone(), hit.model.clone(), hit.active))
                    .or_else(|| live_meta.get(sid_key).cloned())
                    .unwrap_or((None, None, false));
                let entry =
                    by_session
                        .entry(sid_key.to_string())
                        .or_insert_with(|| SessionUsageRankItem {
                            session_id: starfane_proto::SessionId::new(sid_key),
                            agent: src.agent,
                            label: live_label.clone().or_else(|| src.label.clone()),
                            model: live_model.clone().or_else(|| src.model.clone()),
                            active: live_active,
                            totals: TokenUsageTotals::default(),
                            last_active_ms: src.last_active_ms,
                        });
                entry.totals.add_assign(&sess_range_tot);
                entry.active |= live_active;
                if let Some(lbl) = live_label {
                    entry.label = Some(lbl);
                } else if entry.label.is_none() {
                    entry.label = src.label.clone();
                }
                if let Some(mdl) = live_model {
                    entry.model = Some(mdl);
                } else if entry.model.is_none() {
                    entry.model = src.model.clone();
                }
                entry.last_active_ms = entry.last_active_ms.max(src.last_active_ms);
            }
        }

        let daily: Vec<DailyUsageBucket> = buckets
            .into_values()
            .map(|mut b| {
                if let Some(sids) = day_sessions.get(&b.date) {
                    b.session_count = sids.len();
                }
                b
            })
            .collect();

        let mut top_sessions: Vec<SessionUsageRankItem> = by_session.into_values().collect();
        top_sessions.sort_by(|a, b| {
            b.totals
                .total_tokens
                .cmp(&a.totals.total_tokens)
                .then_with(|| b.last_active_ms.cmp(&a.last_active_ms))
        });
        top_sessions.truncate(20);

        UsageSummaryResponse {
            days,
            tz_offset,
            today_date,
            today,
            last_7_days,
            last_30_days,
            range_totals,
            by_agent_range,
            daily,
            top_sessions,
            updated_ms: now_ms,
        }
    }

    fn take_flush_payload(&mut self, force: bool, now_ms: u64) -> Option<(PathBuf, String)> {
        if !self.dirty {
            return None;
        }
        if !force
            && self.last_flushed_ms > 0
            && now_ms.saturating_sub(self.last_flushed_ms) < USAGE_FLUSH_INTERVAL_MS
        {
            return None;
        }
        let body = serde_json::to_string(&self.data).ok()?;
        self.dirty = false;
        self.last_flushed_ms = now_ms;
        Some((self.path.clone(), body))
    }

    fn restore_dirty(&mut self) {
        self.dirty = true;
    }
}

fn infer_session_id_from_transcript_path(agent: AgentKind, path: &Path) -> String {
    match agent {
        AgentKind::Agy => {
            // `<home>/.gemini/antigravity-cli/brain/<conv_id>/.system_generated/logs/transcript.jsonl`
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        }
        AgentKind::Claude | AgentKind::Codex => path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
        AgentKind::Shell => String::new(),
    }
}

fn label_for_backfill(agent: AgentKind, path: &Path) -> Option<String> {
    match agent {
        AgentKind::Agy => {
            let conv_id = infer_session_id_from_transcript_path(AgentKind::Agy, path);
            if conv_id.is_empty() {
                return None;
            }
            let short: String = conv_id.chars().take(8).collect();
            Some(format!("agy-{short}"))
        }
        AgentKind::Claude => {
            // `<home>/.claude/projects/<proj>/<id>.jsonl` or `<proj>/<parent>/subagents/<id>.jsonl`
            let mut after_projects = false;
            let mut proj_dir: Option<String> = None;
            for comp in path.components() {
                if let std::path::Component::Normal(seg) = comp {
                    let s = seg.to_string_lossy();
                    if after_projects {
                        proj_dir = Some(s.into_owned());
                        break;
                    }
                    if s == "projects" {
                        after_projects = true;
                    }
                }
            }
            let proj = proj_dir.or_else(|| {
                let p1 = path.parent()?;
                if p1.file_name().and_then(|n| n.to_str()) == Some("subagents") {
                    p1.parent()
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().into_owned())
                } else {
                    p1.file_name().map(|n| n.to_string_lossy().into_owned())
                }
            })?;
            let seg = proj
                .split('-')
                .rfind(|s| !s.is_empty())
                .unwrap_or(proj.as_str());
            let trimmed: String = seg.chars().take(40).collect();
            (!trimmed.is_empty()).then_some(trimmed)
        }
        AgentKind::Codex => {
            let stem = path.file_stem()?.to_str()?;
            let after_prefix = stem.strip_prefix("rollout-").unwrap_or(stem);
            let stripped = if after_prefix.len() > 20
                && after_prefix.as_bytes().get(10) == Some(&b'T')
                && after_prefix.as_bytes().get(19) == Some(&b'-')
                && after_prefix[..19]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b'T')
            {
                &after_prefix[20..]
            } else {
                after_prefix
            };
            let short: String = stripped.chars().take(8).collect();
            (!short.is_empty()).then(|| format!("codex-{short}"))
        }
        AgentKind::Shell => None,
    }
}

fn collect_recent_local_transcripts(
    home: &Path,
    min_mtime_secs: u64,
) -> Vec<(AgentKind, PathBuf, u64, u128, u64)> {
    let mut out = Vec::new();
    let push_if_recent =
        |out: &mut Vec<(AgentKind, PathBuf, u64, u128, u64)>, agent: AgentKind, p: PathBuf| {
            let Ok(meta) = std::fs::metadata(&p) else {
                return;
            };
            if !meta.is_file() || meta.len() == 0 {
                return;
            }
            let Ok(mod_t) = meta.modified() else {
                return;
            };
            let Ok(dur) = mod_t.duration_since(UNIX_EPOCH) else {
                return;
            };
            if dur.as_secs() < min_mtime_secs {
                return;
            }
            out.push((agent, p, meta.len(), dur.as_nanos(), dur.as_millis() as u64));
        };

    // 1. Claude: `<home>/.claude/projects/*/*.jsonl` & `<home>/.claude/projects/*/subagents/*.jsonl`
    let claude_projects = home.join(".claude").join("projects");
    if let Ok(proj_dirs) = std::fs::read_dir(claude_projects) {
        for proj in proj_dirs.flatten() {
            let proj_path = proj.path();
            if !proj_path.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&proj_path) {
                for ent in entries.flatten() {
                    let p = ent.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        push_if_recent(&mut out, AgentKind::Claude, p);
                    } else if p.is_dir() {
                        let sub_dir = p.join("subagents");
                        if let Ok(subs) = std::fs::read_dir(sub_dir) {
                            for sub in subs.flatten() {
                                let sp = sub.path();
                                if sp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                    push_if_recent(&mut out, AgentKind::Claude, sp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Codex: `<home>/.codex/sessions/<YYYY>/<MM>/<DD>/*.jsonl`
    let codex_root = home.join(".codex").join("sessions");
    if let Ok(years) = std::fs::read_dir(codex_root) {
        for yr in years.flatten() {
            if !yr.path().is_dir() {
                continue;
            }
            if let Ok(months) = std::fs::read_dir(yr.path()) {
                for mo in months.flatten() {
                    if !mo.path().is_dir() {
                        continue;
                    }
                    if let Ok(days) = std::fs::read_dir(mo.path()) {
                        for dy in days.flatten() {
                            if !dy.path().is_dir() {
                                continue;
                            }
                            if let Ok(files) = std::fs::read_dir(dy.path()) {
                                for f in files.flatten() {
                                    let fp = f.path();
                                    if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                        push_if_recent(&mut out, AgentKind::Codex, fp);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. AGY: `<home>/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl`
    let agy_brain = home.join(".gemini").join("antigravity-cli").join("brain");
    if let Ok(convs) = std::fs::read_dir(agy_brain) {
        for c in convs.flatten() {
            let tp = c
                .path()
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if tp.is_file() {
                push_if_recent(&mut out, AgentKind::Agy, tp);
            }
        }
    }

    out.sort_by_key(|a| std::cmp::Reverse(a.4));
    out.truncate(BACKFILL_MAX_FILES);
    out
}

impl Core {
    /// Record per-date usage extracted from a local `TranscriptIndex` (called only when `(len, mtime)` changed).
    pub(crate) fn record_transcript_usage(&self, rec: TranscriptUsageRecord<'_>) {
        if rec.daily.is_empty() || rec.agent.is_shell() {
            return;
        }
        let canon = std::fs::canonicalize(rec.source_path)
            .unwrap_or_else(|_| rec.source_path.to_path_buf());
        let source_key = canon.to_string_lossy().into_owned();
        let entry = UsageSourceEntry {
            agent: rec.agent,
            session_id: rec.session_id.to_string(),
            label: rec.label.map(str::to_string),
            model: rec.model.map(str::to_string),
            file_len: rec.file_len,
            mtime_ns: rec.mtime_ns,
            last_active_ms: rec.last_active_ms,
            daily: rec.daily,
        };
        lock(&self.inner.usage_ledger).upsert_source(source_key, entry);
    }

    /// Record cumulative token totals from a remote session when no local file breakdown exists.
    pub(crate) fn record_remote_session_usage(&self, session: &starfane_proto::SessionRecord) {
        if session.is_local_host() || session.agent.is_shell() {
            return;
        }
        let Some(ref m) = session.metrics else {
            return;
        };
        let inp = m.total_input_tokens.unwrap_or(0);
        let out = m.total_output_tokens.unwrap_or(0);
        let cr = m.total_cache_read_tokens.unwrap_or(0);
        if inp == 0 && out == 0 && cr == 0 {
            return;
        }
        let (tz_ms, _) = local_tz_offset_info();
        let now_ms = id::now_ms();
        let act_ms = m.updated_ms.max(session.last_event_ms);
        let ymd = epoch_ms_to_ymd(act_ms, tz_ms);
        let source_key = format!("remote:{}", session.session_id.as_str());
        let label = session
            .tmux_label
            .clone()
            .or_else(|| session.tmux_target.clone());
        lock(&self.inner.usage_ledger).upsert_remote_cumulative(RemoteUsageUpdate {
            source_key,
            agent: session.agent,
            session_id: session.session_id.0.clone(),
            label,
            model: session.model.clone(),
            now_ms: now_ms.max(act_ms),
            today_ymd: &ymd,
            counts: DailyTokenCounts {
                input_tokens: inp,
                output_tokens: out,
                cache_read_tokens: cr,
            },
        });
    }

    pub(crate) fn today_usage(&self) -> TokenUsageTotals {
        lock(&self.inner.usage_ledger).today_totals(id::now_ms())
    }

    pub async fn usage_summary(&self, days: usize) -> UsageSummaryResponse {
        let mut live_meta: HashMap<String, LiveSessionMetaTuple> = HashMap::new();
        let mut path_rows: Vec<(String, LiveSessionMeta)> = Vec::new();
        {
            let st = lock(&self.inner.state);
            for s in st.sessions.values() {
                let lbl = s
                    .tmux_label
                    .clone()
                    .or_else(|| s.tmux_target.clone())
                    .or_else(|| {
                        s.git_root
                            .as_deref()
                            .or(s.cwd.as_deref())
                            .and_then(|p| Path::new(p).file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                    });
                let active = s.state != starfane_proto::SessionState::Ended;
                live_meta.insert(
                    s.session_id.0.clone(),
                    (lbl.clone(), s.model.clone(), active),
                );
                if let Some(tp) = s.transcript_path.as_deref().filter(|p| !p.is_empty()) {
                    path_rows.push((
                        tp.to_string(),
                        LiveSessionMeta {
                            session_id: s.session_id.0.clone(),
                            label: lbl,
                            model: s.model.clone(),
                            active,
                        },
                    ));
                }
            }
        }
        let mut live_by_path: HashMap<String, LiveSessionMeta> =
            HashMap::with_capacity(path_rows.len());
        for (raw_tp, meta) in path_rows {
            let canon_key = std::fs::canonicalize(&raw_tp)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| raw_tp.clone());
            live_by_path
                .entry(canon_key)
                .or_insert_with(|| meta.clone());
            live_by_path.entry(raw_tp).or_insert(meta);
        }
        lock(&self.inner.usage_ledger).build_summary(days, id::now_ms(), &live_meta, &live_by_path)
    }

    pub(crate) async fn flush_usage_async(&self) {
        let now_ms = id::now_ms();
        let payload = lock(&self.inner.usage_ledger).take_flush_payload(false, now_ms);
        let Some((path, body)) = payload else {
            return;
        };
        let core = self.clone();
        let res = tokio::task::spawn_blocking(move || write_private(&path, body.as_bytes())).await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("usage ledger write failed: {e}");
                lock(&core.inner.usage_ledger).restore_dirty();
            }
            Err(e) => {
                log::warn!("usage ledger write task failed: {e}");
                lock(&core.inner.usage_ledger).restore_dirty();
            }
        }
    }

    pub(crate) fn flush_usage_sync(&self) {
        let now_ms = id::now_ms();
        let payload = lock(&self.inner.usage_ledger).take_flush_payload(true, now_ms);
        if let Some((path, body)) = payload {
            if let Err(e) = write_private(&path, body.as_bytes()) {
                log::warn!("usage ledger sync write failed: {e}");
                lock(&self.inner.usage_ledger).restore_dirty();
            }
        }
    }

    /// Low-priority background backfill of the last ~35 days' local transcripts after startup.
    pub(crate) fn spawn_usage_backfill(&self) {
        let Some(home) = self.effective_home_dir() else {
            return;
        };
        let core = self.clone();
        tokio::spawn(async move {
            let mut shut = core.subscribe_shutdown();
            tokio::select! {
                _ = super::wait_shutdown(&mut shut) => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            if core.is_shutting_down() {
                return;
            }
            let now_secs = id::now_ms() / 1000;
            let min_mtime_secs = now_secs.saturating_sub((BACKFILL_DAYS as u64) * 86_400);
            let candidates = tokio::task::spawn_blocking(move || {
                collect_recent_local_transcripts(&home, min_mtime_secs)
            })
            .await
            .unwrap_or_default();

            let (tz_ms, _) = local_tz_offset_info();
            for (agent, raw_path, file_len, mtime_ns, mtime_ms) in candidates {
                if core.is_shutting_down() {
                    return;
                }
                let canon_path =
                    std::fs::canonicalize(&raw_path).unwrap_or_else(|_| raw_path.clone());
                let key = canon_path.to_string_lossy().into_owned();
                if lock(&core.inner.usage_ledger).source_matches_sig(&key, file_len, mtime_ns) {
                    continue;
                }
                let p_clone = canon_path.clone();
                let parsed = tokio::task::spawn_blocking(move || {
                    parse_transcript_usage_file(agent, &p_clone, tz_ms, Some(mtime_ms))
                })
                .await;
                if let Ok(Ok(parsed_usage)) = parsed {
                    if !parsed_usage.daily.is_empty() {
                        let sid = infer_session_id_from_transcript_path(agent, &canon_path);
                        let label = label_for_backfill(agent, &canon_path);
                        core.record_transcript_usage(TranscriptUsageRecord {
                            source_path: &canon_path,
                            agent,
                            session_id: &sid,
                            label: label.as_deref(),
                            model: parsed_usage.model.as_deref(),
                            file_len,
                            mtime_ns,
                            last_active_ms: parsed_usage.latest_ts.unwrap_or(mtime_ms),
                            daily: parsed_usage.daily,
                        });
                    }
                }
                tokio::select! {
                    _ = super::wait_shutdown(&mut shut) => return,
                    _ = tokio::time::sleep(BACKFILL_SLEEP_PER_FILE) => {}
                }
            }
            core.flush_usage_async().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::metrics::TelemetryFold;
    use super::*;

    #[test]
    fn epoch_ms_to_ymd_respects_timezone_boundaries() {
        // 2026-11-26T20:30:00Z -> UTC = 2026-11-26, +08:00 = 2026-11-27T04:30:00 (2026-11-27)
        let ts_ms: u64 = 1_795_725_000_000;
        assert_eq!(epoch_ms_to_ymd(ts_ms, 0), "2026-11-26");
        assert_eq!(epoch_ms_to_ymd(ts_ms, 8 * 3_600_000), "2026-11-27");
        assert_eq!(epoch_ms_to_ymd(ts_ms, -5 * 3_600_000), "2026-11-26");
        assert_eq!(
            ymd_shift_days("2026-03-01", -1).as_deref(),
            Some("2026-02-28")
        );
        assert_eq!(
            ymd_shift_days("2024-03-01", -1).as_deref(),
            Some("2024-02-29")
        );
    }

    #[test]
    fn telemetry_fold_daily_usage_dedups_claude_and_codex_and_sums_agy() {
        // 1. Claude streaming chunks on same msg.id across two days must deduplicate cleanly.
        let mut cf = TelemetryFold::new(AgentKind::Claude);
        cf.feed_text(concat!(
            r#"{"type":"user","timestamp":"2026-11-26T10:00:00Z","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-26T10:00:01Z","message":{"id":"m1","model":"opus","usage":{"input_tokens":100,"cache_read_input_tokens":500,"cache_creation_input_tokens":50,"output_tokens":20}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-26T10:00:02Z","message":{"id":"m1","model":"opus","usage":{"input_tokens":100,"cache_read_input_tokens":500,"cache_creation_input_tokens":50,"output_tokens":80}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-27T01:00:00Z","message":{"id":"m2","model":"opus","usage":{"input_tokens":200,"cache_read_input_tokens":300,"output_tokens":40}}}"#,
            "\n"
        ));
        let cl_daily = cf.daily_usage(0, None);
        assert_eq!(
            cl_daily.get("2026-11-26").copied(),
            Some(DailyTokenCounts {
                input_tokens: 650,
                output_tokens: 80,
                cache_read_tokens: 500,
            })
        );
        assert_eq!(
            cl_daily.get("2026-11-27").copied(),
            Some(DailyTokenCounts {
                input_tokens: 500,
                output_tokens: 40,
                cache_read_tokens: 300,
            })
        );

        // 2. Codex cumulative `token_count` frames: duplicate totals emit 0 delta, cross-day growth attributes delta to day 2.
        let mut xf = TelemetryFold::new(AgentKind::Codex);
        xf.feed_text(concat!(
            r#"{"type":"event_msg","timestamp":"2026-11-26T08:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120}}}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-11-26T08:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120}}}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-11-27T09:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":750,"output_tokens":200}}}}"#,
            "\n"
        ));
        let cx_daily = xf.daily_usage(0, None);
        assert_eq!(
            cx_daily.get("2026-11-26").copied(),
            Some(DailyTokenCounts {
                input_tokens: 1000,
                output_tokens: 120,
                cache_read_tokens: 400,
            })
        );
        assert_eq!(
            cx_daily.get("2026-11-27").copied(),
            Some(DailyTokenCounts {
                input_tokens: 600,
                output_tokens: 80,
                cache_read_tokens: 350,
            })
        );

        // 3. AGY `PLANNER_RESPONSE` sums input + cache_read and output per date.
        let mut af = TelemetryFold::new(AgentKind::Agy);
        af.feed_text(concat!(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-27T12:00:00Z","model":"gemini-4-neon","input_tokens":300,"cache_read_tokens":700,"output_tokens":90}"#,
            "\n"
        ));
        let ag_daily = af.daily_usage(0, None);
        assert_eq!(
            ag_daily.get("2026-11-27").copied(),
            Some(DailyTokenCounts {
                input_tokens: 1000,
                output_tokens: 90,
                cache_read_tokens: 700,
            })
        );
    }

    #[test]
    fn usage_ledger_persists_roundtrips_and_never_double_counts_reupserts() {
        let dir = std::env::temp_dir().join(format!("starfane-usage-test-{}", id::new_id("U")));
        std::fs::create_dir_all(&dir).unwrap();
        let (tz_ms, _) = local_tz_offset_info();
        let now_ms = id::now_ms();
        let today = epoch_ms_to_ymd(now_ms, tz_ms);
        let yesterday = ymd_shift_days(&today, -1).unwrap();

        {
            let mut ledger = UsageLedger::load(&dir);
            let mut d = BTreeMap::new();
            d.insert(
                today.clone(),
                DailyTokenCounts {
                    input_tokens: 1_000,
                    output_tokens: 200,
                    cache_read_tokens: 400,
                },
            );
            d.insert(
                yesterday.clone(),
                DailyTokenCounts {
                    input_tokens: 500,
                    output_tokens: 100,
                    cache_read_tokens: 200,
                },
            );
            assert!(ledger.upsert_source(
                "/tmp/a.jsonl".into(),
                UsageSourceEntry {
                    agent: AgentKind::Claude,
                    session_id: "s1".into(),
                    label: Some("repo-a".into()),
                    model: Some("opus".into()),
                    file_len: 100,
                    mtime_ns: 1,
                    last_active_ms: now_ms,
                    daily: d.clone(),
                },
            ));
            // Identical re-upsert is a no-op (zero double counting).
            assert!(!ledger.upsert_source(
                "/tmp/a.jsonl".into(),
                UsageSourceEntry {
                    agent: AgentKind::Claude,
                    session_id: "s1".into(),
                    label: Some("repo-a".into()),
                    model: Some("opus".into()),
                    file_len: 100,
                    mtime_ns: 1,
                    last_active_ms: now_ms,
                    daily: d,
                },
            ));
            let (path, body) = ledger.take_flush_payload(true, now_ms).unwrap();
            write_private(&path, body.as_bytes()).unwrap();
        }

        let reloaded = UsageLedger::load(&dir);
        let summary = reloaded.build_summary(7, now_ms, &HashMap::new(), &HashMap::new());
        assert_eq!(summary.today.total_tokens, 1_200);
        assert_eq!(summary.today.cache_read_tokens, 400);
        assert_eq!(summary.last_7_days.total_tokens, 1_800);
        assert_eq!(summary.top_sessions.len(), 1);
        assert_eq!(summary.top_sessions[0].totals.total_tokens, 1_800);

        let mut live_by_path = HashMap::new();
        live_by_path.insert(
            "/tmp/a.jsonl".to_string(),
            LiveSessionMeta {
                session_id: "live-sess-1".to_string(),
                label: Some("prome-main".to_string()),
                model: Some("claude-opus-4-6".to_string()),
                active: true,
            },
        );
        let mapped = reloaded.build_summary(7, now_ms, &HashMap::new(), &live_by_path);
        assert_eq!(mapped.top_sessions[0].session_id.as_str(), "live-sess-1");
        assert_eq!(mapped.top_sessions[0].label.as_deref(), Some("prome-main"));
        assert!(mapped.top_sessions[0].active);

        assert_eq!(
            label_for_backfill(
                AgentKind::Agy,
                Path::new("/tmp/.gemini/antigravity-cli/brain/86fb4e44-53af/.system_generated/logs/transcript.jsonl")
            ),
            Some("agy-86fb4e44".to_string())
        );
        assert_eq!(
            label_for_backfill(
                AgentKind::Claude,
                Path::new(
                    "/tmp/.claude/projects/-Users-wujie-Work-workflow/abc/subagents/agent-1.jsonl"
                )
            ),
            Some("workflow".to_string())
        );
        assert_eq!(
            label_for_backfill(
                AgentKind::Codex,
                Path::new("/tmp/.codex/sessions/2026/11/27/rollout-2026-11-27T08-12-33-019abcde-f123.jsonl")
            ),
            Some("codex-019abcde".to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
