//! Periodic discovery of already-running tmux agent panes, heuristic completion
//! detection for legacy (`discovered_only`) panes, pane preview, and direct L4 spawn.

use super::metrics::{
    agy_step_timings_to_metrics, canonical_external_session_id, extract_agy_effort_from_model,
    extract_codex_status_title, extract_session_id_from_transcript_path, file_identity,
    infer_session_effort, is_post_birth_timestamp, merge_metrics, metrics_meaningfully_equal,
    parse_codex_status_line_model_effort, parse_status_line_metrics, query_lsof_agy_presence_all,
    query_pids_birth_ms, resolve_agy_transcript_by_id_opt_home,
    resolve_claude_transcript_by_pids_in, resolve_codex_transcript_by_id_opt_home,
    transcript_birth_ms, AgyDiscoveredChild,
};
use super::state::{is_subagent_active, PaneWatch, State};
use super::transcript::{resolve_transcript_path_with_home, TranscriptWatermark};
use super::{lock, util, Core};
use starfane_proto::*;
use starfane_tmux::{CaptureRequest, DiscoveredPane, PanePreview};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Force a fresh `ps -eo` sweep every N discovery ticks (~30s at 3s interval) even when pane
/// fingerprints look static, so deeply nested child launches inside unchanged shells are still caught.
const PS_FORCE_REFRESH_TICKS: u32 = 10;
/// Maximum consecutive ticks to serve a cached tail capture before re-probing (ensures panes
/// whose `history_size` is saturated at `history-limit` or update in-place still detect activity).
const TAIL_CACHE_MAX_TICKS: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneFingerprint {
    pane_id: PaneId,
    pane_pid: u32,
    current_command: String,
    current_path: String,
    dead: bool,
}

#[derive(Debug, Clone)]
struct TailCacheEntry {
    history_size: u32,
    cursor_x: u32,
    cursor_y: u32,
    in_mode: bool,
    current_command: String,
    ticks_cached: u8,
    tail: String,
}

#[derive(Debug, Clone)]
struct SpawnedPaneMeta {
    spawned_at_ms: u64,
    model: Option<String>,
    effort: Option<String>,
}

/// Off-state cache eliminating redundant `ps -eo` and `tmux capture-pane` subprocesses during
/// steady-state 3-second discovery ticks, plus OS process birth-time & spawn provenance tracking.
#[derive(Debug, Default)]
pub(crate) struct DiscoveryScanCache {
    panes_sig: Vec<PaneFingerprint>,
    include_shell_panes: bool,
    cached_discovered: Vec<DiscoveredPane>,
    ticks_since_ps: u32,
    tail_cache: HashMap<PaneId, TailCacheEntry>,
    pid_birth_ms: HashMap<u32, u64>,
    pane_agent_pid: HashMap<PaneId, u32>,
    pane_first_seen_ms: HashMap<PaneId, u64>,
    pane_birth_ms: HashMap<PaneId, u64>,
    authoritative_panes: HashSet<PaneId>,
    /// Last matched Codex status-bar title per live pane (detects `/resume` switches & `/new` clears).
    codex_title_bindings: HashMap<PaneId, String>,
    spawned_panes: HashMap<PaneId, SpawnedPaneMeta>,
    /// Last broadcast transcript watermark per local session (for O(delta) `DaemonEvent::Output`).
    last_output_watermarks: HashMap<SessionId, TranscriptWatermark>,
    /// Last broadcast live-tail hash per local session (for `DaemonEvent::Output` live tail pushes).
    last_output_tail_hash: HashMap<SessionId, u64>,
    /// Cached Codex `resume <uuid>` -> rollout JSONL path lookups to avoid nested `read_dir` walks.
    codex_resume_paths: HashMap<(PathBuf, String), PathBuf>,
    /// Cached transcript birth timestamps keyed by `(path, dev, ino)` so steady-state ticks never
    /// re-open and JSON-parse transcript headers.
    transcript_births: HashMap<(PathBuf, u64, u64), Option<u64>>,
    pub(crate) home_override: Option<PathBuf>,
}

const MAX_CODEX_RESUME_PATH_CACHE: usize = 512;
const MAX_TRANSCRIPT_BIRTH_CACHE: usize = 1024;
const MIN_FINAL_EMPTY_BIRTH_FILE_LEN: u64 = 4096;

impl DiscoveryScanCache {
    pub(crate) fn evict_pane(&mut self, pane: &PaneId) {
        self.panes_sig.clear();
        self.cached_discovered.retain(|d| &d.pane_id != pane);
        self.tail_cache.remove(pane);
        self.pane_agent_pid.remove(pane);
        self.pane_first_seen_ms.remove(pane);
        self.pane_birth_ms.remove(pane);
        self.authoritative_panes.remove(pane);
        self.codex_title_bindings.remove(pane);
        self.spawned_panes.remove(pane);
        let leg = legacy_session_id(pane);
        self.last_output_watermarks.remove(&leg);
        self.last_output_tail_hash.remove(&leg);
    }

    fn cached_codex_resume_path(&mut self, home: Option<&Path>, uuid: &str) -> Option<PathBuf> {
        let home = home?;
        let key = (home.to_path_buf(), uuid.trim().to_string());
        if let Some(existing) = self.codex_resume_paths.get(&key) {
            if existing.is_file() {
                return Some(existing.clone());
            }
        }
        let resolved = resolve_codex_transcript_by_id_opt_home(Some(home), uuid)?;
        if self.codex_resume_paths.len() >= MAX_CODEX_RESUME_PATH_CACHE {
            self.codex_resume_paths.clear();
        }
        self.codex_resume_paths.insert(key, resolved.clone());
        Some(resolved)
    }
}

/// Number of unchanged 3-second polling ticks (~9s) before a `Working` legacy pane is
/// considered `Idle` (heuristic `TurnComplete`).
const LEGACY_STABLE_TICKS: u32 = 3;
/// Number of tail lines hashed for legacy pane activity detection.
const LEGACY_HASH_LINES: usize = 20;
/// Grace window for newly spawned interactive panes before discovery prunes them if
/// the agent CLI is still launching in the pane.
const LEGACY_SPAWN_GRACE_MS: u64 = 15_000;

/// Deterministic session ID assigned to a tmux/zellij-discovered pane before any hook arrives.
pub(crate) fn legacy_session_id(pane: &PaneId) -> SessionId {
    if pane.as_str().starts_with("zj:") {
        SessionId::new(pane.as_str())
    } else {
        SessionId::new(format!("tmux-{}", pane.as_str()))
    }
}

/// Skip background `capture-pane` while a pane is in tmux `copy-mode` (`pane_in_mode`)
/// so mouse/keyboard scrolling is never contended, and skip resolved Claude/Codex sessions
/// that already have both `transcript_path` and `metrics`.
pub(crate) fn should_capture_tail_for_scan(
    pane_in_mode: bool,
    is_legacy: bool,
    agent: AgentKind,
    needs_tail_fallback: bool,
) -> bool {
    !pane_in_mode
        && (is_legacy
            || (matches!(agent, AgentKind::Claude | AgentKind::Codex) && needs_tail_fallback))
}

fn hash_tail(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in text.lines() {
        let t = line.trim();
        // Ignore volatile TUI footer bars (warning counters, agent/shortcut hints) so passive redraws
        // never flip idle discovered-only panes into fake `Working -> TurnComplete` transitions.
        if t.is_empty() || t.contains("← for agents") || t.contains("? for shortcuts") {
            continue;
        }
        t.hash(&mut h);
        b'\n'.hash(&mut h);
    }
    h.finish()
}

/// Locate a real hook-backed session matching `path_str` on `pane`. When `allow_revive_ended`
/// is true (authoritative PID/rollout/lock evidence for the live pane), an `Ended` real session
/// (e.g. closed transiently by CLI hot-reload while the tmux pane remained open) may be revived.
pub(crate) fn find_matching_or_revivable_real_session(
    st: &State,
    pane: &PaneId,
    pane_agent: Option<AgentKind>,
    path_str: &str,
    allow_revive_ended: bool,
) -> Option<(SessionId, bool)> {
    let matches = |s: &SessionRecord| {
        !s.discovered_only
            && !s.is_subagent
            && pane_agent.is_none_or(|a| s.agent == a)
            && (s.pane.is_none() || s.pane.as_ref() == Some(pane))
            && super::state::transcript_matches_session(
                s.agent,
                &s.session_id,
                s.transcript_path.as_deref(),
                Some(path_str),
            )
    };
    if let Some(live) = st
        .sessions
        .values()
        .filter(|s| s.state != SessionState::Ended && matches(s))
        .max_by_key(|s| s.last_event_ms)
    {
        return Some((live.session_id.clone(), false));
    }
    if allow_revive_ended {
        if let Some(ended) = st
            .sessions
            .values()
            .filter(|s| s.state == SessionState::Ended && matches(s))
            .max_by_key(|s| s.last_event_ms)
        {
            return Some((ended.session_id.clone(), true));
        }
    }
    None
}

pub(crate) struct SyncDelta {
    pub updated: Vec<SessionRecord>,
    pub active_legacy: Vec<(SessionId, PaneId)>,
    pub changed: bool,
}

#[derive(Debug, Clone)]
struct ClaimedSessionRef {
    sid: SessionId,
    agent: AgentKind,
    pane: Option<PaneId>,
    discovered_only: bool,
    transcript_path: Option<String>,
}

fn snapshot_claim_refs(st: &State) -> Vec<ClaimedSessionRef> {
    st.sessions
        .values()
        .filter(|s| !s.is_subagent && s.is_local_host())
        .map(|s| ClaimedSessionRef {
            sid: s.session_id.clone(),
            agent: s.agent,
            pane: (s.state != SessionState::Ended)
                .then(|| s.pane.clone())
                .flatten(),
            discovered_only: s.discovered_only,
            transcript_path: s.transcript_path.clone(),
        })
        .collect()
}

fn is_transcript_claimed_in_refs(
    refs: &[ClaimedSessionRef],
    owner_sid: &SessionId,
    owner_pane: Option<&PaneId>,
    owner_agent: AgentKind,
    candidate_tp: &str,
) -> bool {
    for r in refs {
        if r.sid == *owner_sid || r.agent != owner_agent {
            continue;
        }
        if !r.discovered_only {
            if r.pane.as_ref() != owner_pane
                && super::state::transcript_matches_session(
                    r.agent,
                    &r.sid,
                    r.transcript_path.as_deref(),
                    Some(candidate_tp),
                )
            {
                return true;
            }
        } else if r.pane.is_some()
            && r.pane.as_ref() != owner_pane
            && r.transcript_path.as_deref() == Some(candidate_tp)
        {
            return true;
        }
    }
    false
}

/// True when `candidate_tp` is already owned by a real hook session (active OR `Ended`) on
/// another/no pane, or by a sibling live `discovered_only` pane.
pub(crate) fn is_transcript_claimed_by_other(
    st: &State,
    owner: &SessionRecord,
    candidate_tp: &str,
) -> bool {
    let refs = snapshot_claim_refs(st);
    is_transcript_claimed_in_refs(
        &refs,
        &owner.session_id,
        owner.pane.as_ref(),
        owner.agent,
        candidate_tp,
    )
}

fn collect_claimed_agent_entries(st: &State, agent: AgentKind) -> Vec<(String, Option<PaneId>)> {
    let mut out = Vec::new();
    for s in st.sessions.values() {
        if s.agent != agent || s.is_subagent || !s.is_local_host() {
            continue;
        }
        // Any `Ended` session has no live pane owner (`None`), so heuristic matchers on ANY pane
        // must treat its external session ID as already-claimed historical state.
        let eff_pane = (s.state != SessionState::Ended)
            .then(|| s.pane.clone())
            .flatten();
        if let Some(eid) = canonical_external_session_id(s) {
            out.push((eid, eff_pane.clone()));
        }
        if !s.discovered_only && !s.session_id.as_str().starts_with("tmux-") {
            out.push((s.session_id.0.clone(), eff_pane));
        }
    }
    out
}

fn collect_live_claimed_agent_entries(
    st: &State,
    agent: AgentKind,
) -> Vec<(String, Option<PaneId>)> {
    let mut out = Vec::new();
    for s in st.sessions.values() {
        if s.agent != agent || s.is_subagent || !s.is_local_host() || s.state == SessionState::Ended
        {
            continue;
        }
        let eff_pane = s.pane.clone();
        if let Some(eid) = canonical_external_session_id(s) {
            out.push((eid, eff_pane.clone()));
        }
        if !s.discovered_only && !s.session_id.as_str().starts_with("tmux-") {
            out.push((s.session_id.0.clone(), eff_pane));
        }
    }
    out
}

fn excluded_ids_for_pane(entries: &[(String, Option<PaneId>)], pane: &PaneId) -> HashSet<String> {
    entries
        .iter()
        .filter(|(_, owner_pane)| owner_pane.as_ref() != Some(pane))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Pure reconciliation of discovered tmux panes into daemon `State` (test convenience wrapper).
#[cfg(test)]
pub(crate) fn reconcile_discovered_panes(
    st: &mut State,
    discovered: &[DiscoveredPane],
    live_pane_ids: &HashSet<PaneId>,
    now_ms: u64,
) -> SyncDelta {
    reconcile_discovered_panes_with_restarts(st, discovered, live_pane_ids, &HashSet::new(), now_ms)
}

pub(crate) fn reconcile_discovered_panes_with_restarts(
    st: &mut State,
    discovered: &[DiscoveredPane],
    live_pane_ids: &HashSet<PaneId>,
    restarted_panes: &HashSet<PaneId>,
    now_ms: u64,
) -> SyncDelta {
    let mut updated = Vec::new();
    let mut active_legacy = Vec::new();
    let mut changed = false;

    for dp in discovered {
        if dp.pane_dead {
            continue;
        }
        let suspended_id = st
            .sessions
            .values()
            .filter(|s| {
                !s.is_subagent
                    && s.is_local_host()
                    && s.pane.as_ref() == Some(&dp.pane_id)
                    && s.state == SessionState::Suspended
            })
            .max_by_key(|s| (!s.discovered_only, s.last_event_ms))
            .map(|s| s.session_id.clone());
        if let Some(susp_id) = suspended_id {
            if dp.agent.is_shell() {
                // Suspended tmux waiter shell or Zellij `--start-suspended` pane: keep Suspended
                // intact and never spawn a duplicate Shell card.
                continue;
            }
            if let Some(rec) = st.sessions.get_mut(&susp_id) {
                if dp.agent == rec.agent {
                    rec.state = SessionState::Idle;
                    rec.suspended_at_ms = None;
                    rec.state_since_ms = now_ms;
                    rec.last_event_ms = now_ms;
                    rec.last_error = None;
                    if rec.tmux_target.as_deref() != Some(dp.tmux_target.as_str()) {
                        rec.tmux_target = Some(dp.tmux_target.clone());
                    }
                    if rec.tmux_label.as_deref() != Some(dp.tmux_label.as_str()) {
                        rec.tmux_label = Some(dp.tmux_label.clone());
                    }
                    updated.push(rec.clone());
                    changed = true;
                    if rec.discovered_only {
                        active_legacy.push((susp_id, dp.pane_id.clone()));
                    }
                    continue;
                }
            }
        }
        // If a brand-new agent process PID replaced an earlier CLI inside the same live shell pane,
        // retire any leftover real session from the exited PID before binding the new process.
        if restarted_panes.contains(&dp.pane_id) {
            for s in st.sessions.values_mut() {
                if !s.is_subagent
                    && !s.discovered_only
                    && s.is_local_host()
                    && s.pane.as_ref() == Some(&dp.pane_id)
                    && s.state != SessionState::Ended
                    && s.state != SessionState::Suspended
                    && !(s.state == SessionState::Starting
                        && s.resume_id.is_some()
                        && now_ms.saturating_sub(s.state_since_ms) < LEGACY_SPAWN_GRACE_MS)
                    && now_ms.saturating_sub(s.last_event_ms) >= 1_000
                {
                    s.state = SessionState::Ended;
                    super::state::clear_tmux_binding(s);
                    s.state_since_ms = now_ms;
                    updated.push(s.clone());
                    changed = true;
                }
            }
        }
        // Check if a real (hook-backed) live root session is already bound to this pane.
        // For Shell panes (when `include_shell_panes = true`), any live real agent session on
        // the same pane (e.g. a wrapped/scripted agent whose foreground process is `python3`)
        // prevents creating a duplicate `discovered_only` Shell entry on that pane.
        let real_id = st
            .sessions
            .values()
            .filter(|s| {
                !s.is_subagent
                    && !s.discovered_only
                    && s.is_local_host()
                    && (s.agent == dp.agent || dp.agent.is_shell())
                    && s.pane.as_ref() == Some(&dp.pane_id)
                    && s.state != SessionState::Ended
            })
            .max_by_key(|s| s.last_event_ms)
            .map(|s| s.session_id.clone());

        let leg_id = legacy_session_id(&dp.pane_id);

        if let Some(real_id) = real_id {
            // Merge/remove any leftover synthetic record for this pane and emit an Ended tombstone
            // so open SSE clients remove the superseded `tmux-%N` entry immediately.
            let prev_legacy = st.sessions.get(&leg_id).cloned();
            if st.merge_legacy_into_real(&leg_id, &real_id) {
                if let Some(mut removed) = prev_legacy {
                    removed.state = SessionState::Ended;
                    removed.state_since_ms = now_ms;
                    removed.last_event_ms = now_ms;
                    updated.push(removed);
                }
                changed = true;
            }
            if st.feishu_session_threads.contains_key(&leg_id) {
                st.migrate_session_feishu_thread(&leg_id, &real_id);
                changed = true;
            }
            if let Some(rec) = st.sessions.get_mut(&real_id) {
                let mut rec_changed = false;
                if rec.tmux_target.as_deref() != Some(dp.tmux_target.as_str()) {
                    rec.tmux_target = Some(dp.tmux_target.clone());
                    rec_changed = true;
                }
                if rec.tmux_label.as_deref() != Some(dp.tmux_label.as_str()) {
                    rec.tmux_label = Some(dp.tmux_label.clone());
                    rec_changed = true;
                }
                if rec.git_root.is_none() && dp.git_root.is_some() {
                    rec.git_root = dp.git_root.clone();
                    rec_changed = true;
                }
                if rec.model.is_none() && dp.model.is_some() {
                    rec.model = dp.model.clone();
                    rec_changed = true;
                }
                if rec.effort.is_none() {
                    let eff = dp.effort.clone().or_else(|| {
                        (rec.agent == AgentKind::Agy)
                            .then(|| rec.model.as_deref().and_then(extract_agy_effort_from_model))
                            .flatten()
                    });
                    if eff.is_some() {
                        rec.effort = eff;
                        rec_changed = true;
                    }
                }
                if rec_changed {
                    changed = true;
                    updated.push(rec.clone());
                }
            }
            continue;
        }

        // Upsert synthetic discovered-only record for this legacy tmux pane.
        match st.sessions.get_mut(&leg_id) {
            Some(rec) => {
                let mut rec_changed = false;
                let waking_resumed = rec.state == SessionState::Starting
                    && rec.resume_id.is_some()
                    && now_ms.saturating_sub(rec.state_since_ms) < LEGACY_SPAWN_GRACE_MS;
                if rec.agent != dp.agent
                    || (restarted_panes.contains(&dp.pane_id) && !waking_resumed)
                {
                    rec.agent = dp.agent;
                    rec.model = dp.model.clone();
                    rec.effort = None;
                    rec.transcript_path = None;
                    rec.metrics = None;
                    rec.last_message = None;
                    rec.last_error = None;
                    rec.state_since_ms = now_ms;
                    rec_changed = true;
                }
                if rec.tmux_target.as_deref() != Some(dp.tmux_target.as_str()) {
                    rec.tmux_target = Some(dp.tmux_target.clone());
                    rec_changed = true;
                }
                if rec.tmux_label.as_deref() != Some(dp.tmux_label.as_str()) {
                    rec.tmux_label = Some(dp.tmux_label.clone());
                    rec_changed = true;
                }
                if rec.cwd.as_deref() != Some(dp.cwd.as_str()) {
                    rec.cwd = Some(dp.cwd.clone());
                    rec_changed = true;
                }
                if dp.git_root.is_some() && rec.git_root != dp.git_root {
                    rec.git_root = dp.git_root.clone();
                    rec_changed = true;
                }
                if rec.agent == AgentKind::Shell {
                    if rec.model != dp.model {
                        rec.model = dp.model.clone();
                        rec_changed = true;
                    }
                } else if dp.model.is_some()
                    && (rec.model.is_none()
                        || (rec.transcript_path.is_none() && rec.model != dp.model))
                {
                    rec.model = dp.model.clone();
                    if rec.transcript_path.is_none() {
                        rec.effort = None;
                    }
                    rec_changed = true;
                }
                let dp_eff = dp.effort.clone().or_else(|| {
                    (rec.agent == AgentKind::Agy)
                        .then(|| rec.model.as_deref().and_then(extract_agy_effort_from_model))
                        .flatten()
                });
                if dp_eff.is_some()
                    && (rec.effort.is_none()
                        || (rec.transcript_path.is_none() && rec.effort != dp_eff))
                {
                    rec.effort = dp_eff;
                    rec_changed = true;
                }
                if rec.state == SessionState::Ended {
                    rec.state = SessionState::Idle;
                    rec.state_since_ms = now_ms;
                    rec_changed = true;
                }
                if rec_changed {
                    changed = true;
                    updated.push(rec.clone());
                }
            }
            None => {
                let init_effort = dp.effort.clone().or_else(|| {
                    (dp.agent == AgentKind::Agy)
                        .then(|| dp.model.as_deref().and_then(extract_agy_effort_from_model))
                        .flatten()
                });
                let rec = SessionRecord {
                    session_id: leg_id.clone(),
                    agent: dp.agent,
                    pane: Some(dp.pane_id.clone()),
                    pane_resolution: PaneResolution::Env,
                    cwd: Some(dp.cwd.clone()),
                    git_root: dp.git_root.clone(),
                    model: dp.model.clone(),
                    effort: init_effort,
                    state: SessionState::Idle,
                    state_since_ms: now_ms,
                    last_event_id: None,
                    last_event_ms: now_ms,
                    last_message: None,
                    last_error: None,
                    parent_session_id: None,
                    is_subagent: false,
                    subagent_count: 0,
                    active_subagents: 0,
                    stage: None,
                    unread_inbox: 0,
                    confidence: Confidence::Heuristic,
                    tmux_target: Some(dp.tmux_target.clone()),
                    tmux_label: Some(dp.tmux_label.clone()),
                    discovered_only: true,
                    transcript_path: None,
                    metrics: None,
                    host_id: None,
                    host_display_name: None,
                    mux_kind: Some(if dp.pane_id.as_str().starts_with("zj:") {
                        "zellij".to_string()
                    } else {
                        "tmux".to_string()
                    }),
                    resume_id: None,
                    suspended_at_ms: None,
                };
                st.sessions.insert(leg_id.clone(), rec.clone());
                changed = true;
                updated.push(rec);
            }
        }

        active_legacy.push((leg_id, dp.pane_id.clone()));
    }

    // Clean up discovered-only panes that disappeared or are no longer running an agent.
    let discovered_panes: HashMap<&PaneId, AgentKind> = discovered
        .iter()
        .filter(|d| !d.pane_dead)
        .map(|d| (&d.pane_id, d.agent))
        .collect();
    let stale_legacy: Vec<(SessionId, Option<PaneId>)> = st
        .sessions
        .iter()
        .filter(|(_, s)| {
            if !s.discovered_only || !s.is_local_host() {
                return false;
            }
            match s.pane.as_ref() {
                None => true,
                Some(p) => {
                    if !live_pane_ids.contains(p) {
                        return true;
                    }
                    if s.state == SessionState::Suspended {
                        return false;
                    }
                    if discovered_panes.contains_key(p) {
                        return false;
                    }
                    // Give freshly spawned interactive sessions a brief startup window while
                    // the agent binary initializes inside the live pane.
                    !(s.state == SessionState::Starting
                        && now_ms.saturating_sub(s.state_since_ms) < LEGACY_SPAWN_GRACE_MS)
                }
            }
        })
        .map(|(id, s)| (id.clone(), s.pane.clone()))
        .collect();

    for (sid, pane) in stale_legacy {
        if let Some(mut removed) = st.sessions.remove(&sid) {
            removed.state = SessionState::Ended;
            removed.state_since_ms = now_ms;
            removed.last_event_ms = now_ms;
            updated.push(removed);
            changed = true;
        }
        if let Some(p) = pane {
            st.pane_watch.remove(&p);
        }
    }

    // Reap dead/exited real sessions when tmux pane truth is available, plus stale non-tmux orphans (>1h).
    if !live_pane_ids.is_empty() {
        let dead_real_ids: Vec<SessionId> = st
            .sessions
            .iter()
            .filter(|(_, s)| {
                if s.discovered_only
                    || s.is_subagent
                    || s.state == SessionState::Ended
                    || !s.is_local_host()
                {
                    return false;
                }
                match s.pane.as_ref() {
                    Some(p) => {
                        if !live_pane_ids.contains(p) {
                            return true;
                        }
                        if s.state == SessionState::Suspended {
                            return false;
                        }
                        if now_ms.saturating_sub(s.last_event_ms) < LEGACY_SPAWN_GRACE_MS {
                            return false;
                        }
                        if s.state == SessionState::Starting
                            && now_ms.saturating_sub(s.state_since_ms) < LEGACY_SPAWN_GRACE_MS
                        {
                            return false;
                        }
                        match discovered_panes.get(p) {
                            Some(&dp_agent) => {
                                dp_agent != s.agent
                                    && now_ms.saturating_sub(s.state_since_ms)
                                        >= LEGACY_SPAWN_GRACE_MS
                            }
                            None => true,
                        }
                    }
                    None => s.stage.is_none() && now_ms.saturating_sub(s.last_event_ms) > 3_600_000,
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        for sid in dead_real_ids {
            if let Some(rec) = st.sessions.get_mut(&sid) {
                rec.state = SessionState::Ended;
                if let Some(p) = rec.pane.take() {
                    st.pane_watch.remove(&p);
                }
                if rec.pane_resolution == PaneResolution::CwdMatch {
                    super::state::clear_tmux_binding(rec);
                }
                rec.state_since_ms = now_ms;
                updated.push(rec.clone());
                changed = true;
            }
        }
    }

    for rec in st.sessions.values_mut() {
        if !rec.discovered_only
            && !rec.is_subagent
            && rec.is_local_host()
            && rec.state != SessionState::Ended
            && rec.pane.is_none()
            && (rec.tmux_target.is_some()
                || rec.tmux_label.is_some()
                || rec.pane_resolution != PaneResolution::Unknown)
        {
            super::state::clear_tmux_binding(rec);
            updated.push(rec.clone());
            changed = true;
        }
    }

    SyncDelta {
        updated,
        active_legacy,
        changed,
    }
}

/// Pure state transition for a legacy pane's tail-hash poll.
/// Returns `Some((session, heuristic_event))` when state changed (including folder-trust
/// approval prompts mapped to `SessionState::NeedsPermission` and heuristic `TurnComplete`).
pub(crate) fn apply_legacy_tail_hash(
    st: &mut State,
    sid: &SessionId,
    pane: &PaneId,
    tail: &str,
    now_ms: u64,
) -> Option<(SessionRecord, Option<NormalizedEvent>)> {
    let h = hash_tail(tail);
    let rec = st.sessions.get_mut(sid)?;
    if !rec.discovered_only {
        return None;
    }

    if let Some(reason) = starfane_tmux::detect_folder_trust_prompt(rec.agent, tail) {
        st.pane_watch.insert(
            pane.clone(),
            PaneWatch {
                last_hash: h,
                stable_ticks: 0,
            },
        );
        if rec.state != SessionState::NeedsPermission || rec.last_error.as_deref() != Some(reason) {
            rec.state = SessionState::NeedsPermission;
            rec.last_error = Some(reason.to_string());
            rec.state_since_ms = now_ms;
            rec.last_event_ms = now_ms;
            let ev = NormalizedEvent {
                v: PROTO_VERSION,
                id: EventId::new(id::new_id(id::prefix::EVENT)),
                ts_ms: now_ms,
                agent: rec.agent,
                kind: EventKind::NeedsPermission,
                confidence: Confidence::Heuristic,
                session_id: rec.session_id.clone(),
                subagent_id: None,
                pane: rec.pane.clone(),
                cwd: rec.cwd.clone(),
                model: rec.model.clone(),
                native_event: "tmux_trust_prompt_heuristic".into(),
                transcript_path: None,
                bind_token: None,
                detail: EventDetail {
                    error: Some(reason.to_string()),
                    ..EventDetail::default()
                },
            };
            return Some((rec.clone(), Some(ev)));
        }
        return None;
    }

    if let Some(reason) = starfane_tmux::detect_codex_update_prompt(rec.agent, tail) {
        st.pane_watch.insert(
            pane.clone(),
            PaneWatch {
                last_hash: h,
                stable_ticks: 0,
            },
        );
        if rec.state != SessionState::NeedsInput || rec.last_error.as_deref() != Some(reason) {
            rec.state = SessionState::NeedsInput;
            rec.last_error = Some(reason.to_string());
            rec.state_since_ms = now_ms;
            rec.last_event_ms = now_ms;
            let ev = NormalizedEvent {
                v: PROTO_VERSION,
                id: EventId::new(id::new_id(id::prefix::EVENT)),
                ts_ms: now_ms,
                agent: rec.agent,
                kind: EventKind::NeedsInput,
                confidence: Confidence::Heuristic,
                session_id: rec.session_id.clone(),
                subagent_id: None,
                pane: rec.pane.clone(),
                cwd: rec.cwd.clone(),
                model: rec.model.clone(),
                native_event: "tmux_startup_modal_heuristic".into(),
                transcript_path: None,
                bind_token: None,
                detail: EventDetail {
                    error: Some(reason.to_string()),
                    ..EventDetail::default()
                },
            };
            return Some((rec.clone(), Some(ev)));
        }
        return None;
    }

    match st.pane_watch.get_mut(pane) {
        None => {
            st.pane_watch.insert(
                pane.clone(),
                PaneWatch {
                    last_hash: h,
                    stable_ticks: 0,
                },
            );
            if matches!(
                rec.state,
                SessionState::NeedsPermission | SessionState::NeedsInput
            ) {
                rec.state = SessionState::Idle;
                rec.last_error = None;
                rec.state_since_ms = now_ms;
                rec.last_event_ms = now_ms;
                return Some((rec.clone(), None));
            }
            None
        }
        Some(watch) => {
            if watch.last_hash != h {
                watch.last_hash = h;
                watch.stable_ticks = 0;
                if matches!(
                    rec.state,
                    SessionState::NeedsPermission | SessionState::NeedsInput
                ) {
                    rec.last_error = None;
                }
                if rec.state != SessionState::Working {
                    rec.state = SessionState::Working;
                    rec.state_since_ms = now_ms;
                    rec.last_event_ms = now_ms;
                    return Some((rec.clone(), None));
                }
                rec.last_event_ms = now_ms;
                None
            } else {
                watch.stable_ticks = watch.stable_ticks.saturating_add(1);
                if watch.stable_ticks >= LEGACY_STABLE_TICKS {
                    if rec.state == SessionState::Working {
                        rec.state = SessionState::Idle;
                        rec.state_since_ms = now_ms;
                        rec.last_event_ms = now_ms;
                        if rec.agent.is_shell() {
                            return Some((rec.clone(), None));
                        }
                        let ev = NormalizedEvent {
                            v: PROTO_VERSION,
                            id: EventId::new(id::new_id(id::prefix::EVENT)),
                            ts_ms: now_ms,
                            agent: rec.agent,
                            kind: EventKind::TurnComplete,
                            confidence: Confidence::Heuristic,
                            session_id: rec.session_id.clone(),
                            subagent_id: None,
                            pane: rec.pane.clone(),
                            cwd: rec.cwd.clone(),
                            model: rec.model.clone(),
                            native_event: "tmux_idle_heuristic".into(),
                            transcript_path: None,
                            bind_token: None,
                            detail: EventDetail::default(),
                        };
                        return Some((rec.clone(), Some(ev)));
                    }
                    if matches!(
                        rec.state,
                        SessionState::Starting
                            | SessionState::NeedsPermission
                            | SessionState::NeedsInput
                    ) {
                        rec.state = SessionState::Idle;
                        rec.last_error = None;
                        rec.state_since_ms = now_ms;
                        rec.last_event_ms = now_ms;
                        return Some((rec.clone(), None));
                    }
                }
                None
            }
        }
    }
}

/// Compute the next sequential `<agent>-<N>` window name inside a tmux session.
pub(crate) fn next_sequential_window_name(
    existing_window_names: &[String],
    agent: AgentKind,
) -> String {
    let prefix = format!("{}-", agent.as_str());
    let max_num = existing_window_names
        .iter()
        .filter_map(|name| name.trim().strip_prefix(&prefix))
        .filter_map(|rest| rest.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}{}", max_num + 1)
}

/// Default relative directory under `$HOME` used for direct interactive sessions
/// when the caller leaves `project_root` empty.
pub const DEFAULT_AGENT_SESSION_DIR: &str = ".agent_session";

fn expand_tilde_in(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn resolve_spawn_cwd_with<F>(
    home: Option<&Path>,
    project_root: Option<&str>,
    find_project: F,
) -> Result<String, ApiError>
where
    F: FnOnce(&str) -> Result<String, ApiError>,
{
    let default_dir = home.map(|h| h.join(DEFAULT_AGENT_SESSION_DIR));
    match project_root.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            let dir = default_dir.ok_or_else(|| ApiError::internal("HOME is not set"))?;
            std::fs::create_dir_all(&dir)
                .map_err(|e| ApiError::internal(format!("mkdir {}: {e}", dir.display())))?;
            Ok(dir.to_string_lossy().into_owned())
        }
        Some(trimmed) => {
            let expanded = expand_tilde_in(trimmed, home);
            if default_dir.as_ref().is_some_and(|d| d == &expanded) {
                std::fs::create_dir_all(&expanded).map_err(|e| {
                    ApiError::internal(format!("mkdir {}: {e}", expanded.display()))
                })?;
                return Ok(expanded.to_string_lossy().into_owned());
            }
            if expanded.is_absolute() || trimmed.starts_with('~') {
                if expanded.is_dir() {
                    return Ok(expanded.to_string_lossy().into_owned());
                }
                return Err(ApiError::bad_request(format!(
                    "working directory does not exist: {}",
                    expanded.display()
                )));
            }
            find_project(trimmed)
        }
    }
}

/// Incremental transcript output delta computed off-lock from `TranscriptIndexCache`.
#[derive(Debug, Clone)]
struct TranscriptOutputDelta {
    start_seq: Option<usize>,
    total_messages: Option<usize>,
    appended_messages: Vec<serde_json::Value>,
    dirty: bool,
}

#[derive(Debug)]
struct ComputedRootMetrics {
    sid: SessionId,
    agent: AgentKind,
    tr_model: Option<String>,
    tr_effort: Option<String>,
    st_model: Option<String>,
    st_effort: Option<String>,
    combined: Option<SessionMetrics>,
    output_delta: Option<TranscriptOutputDelta>,
    live_tail: Option<String>,
    has_transcript: bool,
}

#[derive(Debug)]
struct PendingOutputPush {
    sid: SessionId,
    start_seq: Option<usize>,
    total_messages: Option<usize>,
    appended_messages: Vec<serde_json::Value>,
    live_tail: Option<String>,
    metrics: Option<SessionMetrics>,
    dirty: bool,
}

fn apply_computed_root_metrics(
    rec: &mut SessionRecord,
    item: ComputedRootMetrics,
    authoritative_panes: &HashSet<PaneId>,
    pane_birth_by_pane: &HashMap<PaneId, u64>,
    home_dir: Option<&Path>,
    metrics_updated: &mut Vec<SessionRecord>,
    output_pushes: &mut Vec<PendingOutputPush>,
) {
    // Guard non-authoritative `discovered_only` sessions against any pre-birth
    // telemetry/model leakage from cc-switch or transcripts — without ever dropping
    // live terminal `live_tail` updates!
    let skip_pre_birth_telemetry = if rec.discovered_only {
        let is_auth = rec
            .pane
            .as_ref()
            .is_some_and(|p| authoritative_panes.contains(p));
        let min_birth = rec
            .pane
            .as_ref()
            .and_then(|p| pane_birth_by_pane.get(p).copied());
        !is_auth
            && min_birth.is_some_and(|b| {
                item.combined.as_ref().is_some_and(|comb| {
                    comb.source != "status_line" && !is_post_birth_timestamp(comb.updated_ms, b)
                })
            })
    } else {
        false
    };
    if !skip_pre_birth_telemetry {
        let mut changed_rec = false;
        let valid_tr = item
            .tr_model
            .as_deref()
            .map(str::trim)
            .filter(|s| {
                !s.is_empty()
                    && *s != "<synthetic>"
                    && !(item.agent == AgentKind::Agy && *s == "gemini-flash-worker-agy")
            })
            .map(str::to_string);
        if let Some(m) = valid_tr {
            if rec.model.as_deref() != Some(m.as_str()) {
                rec.model = Some(m);
                changed_rec = true;
            }
        } else if rec.model.is_none()
            || (!rec.is_subagent
                && rec.agent == AgentKind::Agy
                && rec.model.as_deref() == Some("gemini-flash-worker-agy"))
        {
            if let Some(m) = item.st_model {
                rec.model = Some(m);
                changed_rec = true;
            }
        }
        let next_effort = if item.agent == AgentKind::Codex && item.tr_effort.is_some() {
            item.tr_effort
        } else if item.agent == AgentKind::Agy {
            rec.model
                .as_deref()
                .and_then(extract_agy_effort_from_model)
                .or_else(|| rec.effort.clone())
        } else if rec.effort.is_some() {
            rec.effort.clone()
        } else if item.agent == AgentKind::Codex && item.st_effort.is_some() {
            item.st_effort
        } else {
            infer_session_effort(
                item.agent,
                rec.model.as_deref(),
                None,
                item.tr_effort.as_deref(),
                home_dir,
            )
        };
        if next_effort.is_some() && rec.effort != next_effort {
            rec.effort = next_effort;
            changed_rec = true;
        }
        if item.combined.is_some() && !metrics_meaningfully_equal(&rec.metrics, &item.combined) {
            rec.metrics = item.combined;
            changed_rec = true;
        }
        if changed_rec {
            metrics_updated.push(rec.clone());
        }
    }
    let effective_output_delta = if skip_pre_birth_telemetry {
        None
    } else {
        item.output_delta
    };
    if effective_output_delta.is_some() || item.live_tail.is_some() {
        let (start_seq, total_messages, appended_messages, delta_dirty) =
            match effective_output_delta {
                Some(d) => (d.start_seq, d.total_messages, d.appended_messages, d.dirty),
                None => (None, None, Vec::new(), !item.has_transcript),
            };
        output_pushes.push(PendingOutputPush {
            sid: item.sid,
            start_seq,
            total_messages,
            appended_messages,
            live_tail: item.live_tail,
            metrics: rec.metrics.clone(),
            dirty: delta_dirty,
        });
    }
}

fn sync_transcript_and_output_delta(
    core: &Core,
    sid: &SessionId,
    agent: AgentKind,
    path: &Path,
    now_ms: u64,
) -> (
    Option<SessionMetrics>,
    Option<String>,
    Option<String>,
    Option<TranscriptOutputDelta>,
) {
    sync_transcript_and_output_delta_with_label(core, sid, agent, path, now_ms, None)
}

/// Sync `path` in `TranscriptIndexCache`, update persistent `usage_ledger` when `(len, mtime)` changed,
/// and detect whether `TranscriptWatermark` changed since the last broadcast for `sid`.
fn sync_transcript_and_output_delta_with_label(
    core: &Core,
    sid: &SessionId,
    agent: AgentKind,
    path: &Path,
    now_ms: u64,
    label_hint: Option<&str>,
) -> (
    Option<SessionMetrics>,
    Option<String>,
    Option<String>,
    Option<TranscriptOutputDelta>,
) {
    let (metrics, model, effort, wm, file_len, mtime_ns, canon_path, fallback_ts) = {
        let mut tc = lock(&core.inner.transcript_index);
        let canon_path = tc.resolve_canonical(path);
        let Some(idx) = tc.get_or_sync(agent, path) else {
            return (None, None, None, None);
        };
        let (metrics, model, effort) = idx.telemetry_snapshot(now_ms);
        let wm = idx.watermark(path);
        let (file_len, mtime_ns) = idx.file_sig();
        let fallback_ts = idx.latest_event_ts().unwrap_or(now_ms);
        (
            metrics,
            model,
            effort,
            wm,
            file_len,
            mtime_ns,
            canon_path,
            fallback_ts,
        )
    };
    let prev_opt = lock(&core.inner.discovery_cache)
        .last_output_watermarks
        .insert(sid.clone(), wm);
    let need_usage_check = file_len > 0 && prev_opt.is_none_or(|p| p != wm);
    if need_usage_check {
        let key_str = canon_path.to_string_lossy();
        let already_recorded =
            lock(&core.inner.usage_ledger).source_matches_sig(&key_str, file_len, mtime_ns);
        if !already_recorded {
            let (tz_ms, _) = super::metrics::local_tz_offset_info();
            let daily_opt = {
                let mut tc = lock(&core.inner.transcript_index);
                tc.get_or_sync(agent, &canon_path)
                    .map(|idx| idx.daily_usage(tz_ms, Some(fallback_ts)))
            };
            if let Some(daily) = daily_opt {
                core.record_transcript_usage(super::usage::TranscriptUsageRecord {
                    source_path: &canon_path,
                    agent,
                    session_id: sid.as_str(),
                    label: label_hint,
                    model: model.as_deref(),
                    file_len,
                    mtime_ns,
                    last_active_ms: fallback_ts,
                    daily,
                });
            }
        }
    }
    let delta = {
        let mut tc = lock(&core.inner.transcript_index);
        let Some(idx) = tc.get_or_sync(agent, path) else {
            return (metrics, model, effort, None);
        };
        match prev_opt {
            Some(prev) if wm.total_messages > prev.total_messages => {
                let prev_last_mutated = prev.total_messages > 0
                    && (prev.has_pending_tools
                        || idx.turn_hash_at(path, prev.total_messages - 1)
                            != Some(prev.last_turn_hash));
                let start_from = if prev_last_mutated {
                    prev.total_messages - 1
                } else {
                    prev.total_messages
                };
                let count = wm.total_messages - start_from;
                if count <= super::MAX_INLINE_OUTPUT_MESSAGES {
                    let page = idx.slice_page(
                        path,
                        super::transcript::TranscriptQuery::tail(count),
                        None,
                        now_ms,
                    );
                    let appended_messages: Vec<serde_json::Value> = page
                        .messages
                        .iter()
                        .filter_map(|m| serde_json::to_value(m).ok())
                        .collect();
                    Some(TranscriptOutputDelta {
                        start_seq: Some(page.start_seq),
                        total_messages: Some(wm.total_messages),
                        appended_messages,
                        dirty: false,
                    })
                } else {
                    Some(TranscriptOutputDelta {
                        start_seq: Some(start_from),
                        total_messages: Some(wm.total_messages),
                        appended_messages: Vec::new(),
                        dirty: true,
                    })
                }
            }
            Some(prev)
                if wm.total_messages == prev.total_messages
                    && wm.total_messages > 0
                    && (wm.last_turn_hash != prev.last_turn_hash
                        || wm.has_pending_tools != prev.has_pending_tools) =>
            {
                let page = idx.slice_page(
                    path,
                    super::transcript::TranscriptQuery::tail(1),
                    None,
                    now_ms,
                );
                let appended_messages: Vec<serde_json::Value> = page
                    .messages
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                Some(TranscriptOutputDelta {
                    start_seq: Some(page.start_seq),
                    total_messages: Some(wm.total_messages),
                    appended_messages,
                    dirty: false,
                })
            }
            Some(prev) if wm.total_messages < prev.total_messages => Some(TranscriptOutputDelta {
                start_seq: None,
                total_messages: Some(wm.total_messages),
                appended_messages: Vec::new(),
                dirty: true,
            }),
            _ => None,
        }
    };
    (metrics, model, effort, delta)
}

fn check_live_tail_changed(core: &Core, sid: &SessionId, tail: &str) -> bool {
    let h = hash_tail(tail);
    let prev = lock(&core.inner.discovery_cache)
        .last_output_tail_hash
        .insert(sid.clone(), h);
    prev.is_some_and(|old| old != h)
}

impl Core {
    pub(crate) fn effective_home_dir(&self) -> Option<PathBuf> {
        if let Some(ov) = lock(&self.inner.discovery_cache).home_override.clone() {
            return Some(ov);
        }
        std::env::var_os("HOME").map(PathBuf::from)
    }

    /// Override `$HOME` resolution for isolated multi-session integration tests without mutating
    /// process-global environment variables.
    #[doc(hidden)]
    pub fn set_home_dir_override(&self, home: Option<PathBuf>) {
        lock(&self.inner.discovery_cache).home_override = home;
    }

    /// True when `session` is a non-authoritative `discovered_only` pane record whose attached
    /// `path` was created strictly before the pane's process/spawn birth time.
    pub(crate) fn is_legacy_transcript_pre_birth(
        &self,
        session: &SessionRecord,
        path: &Path,
    ) -> bool {
        if !session.discovered_only || session.is_subagent {
            return false;
        }
        let Some(pane) = session.pane.as_ref() else {
            return false;
        };
        let min_birth_ms = {
            let dc = lock(&self.inner.discovery_cache);
            if dc.authoritative_panes.contains(pane) {
                return false;
            }
            dc.pane_birth_ms
                .get(pane)
                .copied()
                .or_else(|| dc.spawned_panes.get(pane).map(|m| m.spawned_at_ms))
        };
        let Some(birth_ms) = min_birth_ms else {
            return false;
        };
        self.cached_transcript_birth_ms(path)
            .is_some_and(|tb| !is_post_birth_timestamp(tb, birth_ms))
    }

    fn cached_transcript_birth_ms(&self, path: &Path) -> Option<u64> {
        let (dev, ino, _mtime_ns, file_len) = file_identity(path)?;
        let cache_key = (path.to_path_buf(), dev, ino);
        if dev != 0 || ino != 0 {
            if let Some(&cached) = lock(&self.inner.discovery_cache)
                .transcript_births
                .get(&cache_key)
            {
                return cached;
            }
        }
        let computed = transcript_birth_ms(path);
        if (dev != 0 || ino != 0)
            && (computed.is_some() || file_len >= MIN_FINAL_EMPTY_BIRTH_FILE_LEN)
        {
            let mut dc = lock(&self.inner.discovery_cache);
            if dc.transcript_births.len() >= MAX_TRANSCRIPT_BIRTH_CACHE {
                dc.transcript_births.clear();
            }
            dc.transcript_births.insert(cache_key, computed);
        }
        computed
    }

    /// Scan tmux for existing agent panes, sync into `State`, and run heuristic completion checks.
    ///
    /// Hot-path guarantees:
    /// - Skips `ps -eo` when pane fingerprints are unchanged (refreshing periodically every 30s).
    /// - Skips `capture-pane` when `(history_size, cursor_x, cursor_y, in_mode, current_command)`
    ///   did not change, and batches all changed panes into a single `tmux` invocation.
    /// - Runs ALL SQLite (`codex`, `agy`), `lsof`, and O(delta) JSONL reads outside
    ///   `Mutex<State>` so control-plane lock hold times stay sub-millisecond.
    pub async fn sync_discovered_panes(&self) {
        let core = self.clone();
        let include_shell_panes = core.inner.cfg.discovery.include_shell_panes;
        let scan = tokio::task::spawn_blocking(move || {
            let panes = core.inner.mux.list_all_panes()?;
            let live_ids: HashSet<PaneId> = panes.iter().map(|p| p.pane_id.clone()).collect();
            let mut sig: Vec<PaneFingerprint> = panes
                .iter()
                .map(|p| PaneFingerprint {
                    pane_id: p.pane_id.clone(),
                    pane_pid: p.pane_pid,
                    current_command: p.current_command.clone(),
                    current_path: p.current_path.clone(),
                    dead: p.dead,
                })
                .collect();
            sig.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));

            let reused = {
                let mut dc = lock(&core.inner.discovery_cache);
                if !panes.is_empty()
                    && dc.include_shell_panes == include_shell_panes
                    && dc.panes_sig == sig
                    && dc.ticks_since_ps < PS_FORCE_REFRESH_TICKS
                {
                    dc.ticks_since_ps += 1;
                    let by_id: HashMap<&PaneId, &starfane_tmux::PaneInfo> =
                        panes.iter().map(|p| (&p.pane_id, p)).collect();
                    let mut updated = dc.cached_discovered.clone();
                    for dp in &mut updated {
                        if let Some(p) = by_id.get(&dp.pane_id) {
                            dp.tmux_target =
                                format!("{}:{}.{}", p.session_name, p.window_index, p.pane_index);
                            dp.tmux_label = p.tmux_label();
                            dp.pane_in_mode = p.in_mode;
                            dp.pane_dead = p.dead;
                            dp.history_size = p.history_size;
                            dp.cursor_x = p.cursor_x;
                            dp.cursor_y = p.cursor_y;
                        }
                    }
                    Some(updated)
                } else {
                    None
                }
            };

            let discovered = match reused {
                Some(d) => d,
                None => {
                    let fresh = core.inner.mux.discover_panes_from(
                        &panes,
                        starfane_tmux::DiscoverOptions {
                            include_shell_panes,
                        },
                    );
                    let mut dc = lock(&core.inner.discovery_cache);
                    dc.panes_sig = sig;
                    dc.include_shell_panes = include_shell_panes;
                    dc.cached_discovered = fresh.clone();
                    dc.ticks_since_ps = 0;
                    dc.tail_cache.retain(|pid, _| live_ids.contains(pid));
                    fresh
                }
            };
            Ok::<_, starfane_tmux::TmuxError>((discovered, live_ids))
        })
        .await;

        let (discovered, live_ids) = match scan {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::debug!("tmux discovery skipped: {e}");
                return;
            }
            Err(e) => {
                log::warn!("tmux discovery task failed: {e}");
                return;
            }
        };

        let now = id::now_ms();
        let home_dir = self.effective_home_dir();
        let restarted_panes = {
            let mut dc = lock(&self.inner.discovery_cache);
            dc.pane_agent_pid.retain(|pid, _| live_ids.contains(pid));
            dc.pane_first_seen_ms
                .retain(|pid, _| live_ids.contains(pid));
            dc.pane_birth_ms.retain(|pid, _| live_ids.contains(pid));
            dc.authoritative_panes.retain(|pid| live_ids.contains(pid));
            dc.codex_title_bindings
                .retain(|pid, _| live_ids.contains(pid));
            dc.spawned_panes.retain(|pid, _| live_ids.contains(pid));
            let mut restarted = HashSet::new();
            for dp in &discovered {
                if dp.pane_dead {
                    continue;
                }
                dc.pane_first_seen_ms
                    .entry(dp.pane_id.clone())
                    .or_insert(now);
                let eff_pid = dp
                    .agent_pid
                    .or_else(|| dp.subtree_pids.first().copied())
                    .filter(|&p| p > 0);
                if let Some(cur_pid) = eff_pid {
                    if let Some(prev_pid) = dc.pane_agent_pid.insert(dp.pane_id.clone(), cur_pid) {
                        if prev_pid != cur_pid {
                            restarted.insert(dp.pane_id.clone());
                            dc.pane_first_seen_ms.insert(dp.pane_id.clone(), now);
                            dc.tail_cache.remove(&dp.pane_id);
                            dc.codex_title_bindings.remove(&dp.pane_id);
                            dc.spawned_panes.remove(&dp.pane_id);
                        }
                    }
                }
            }
            restarted
        };

        let (
            delta,
            resolved_panes,
            claimed_claude_entries,
            live_claimed_codex_entries,
            claimed_agy_entries,
        ) = {
            let mut st = lock(&self.inner.state);
            let delta = reconcile_discovered_panes_with_restarts(
                &mut st,
                &discovered,
                &live_ids,
                &restarted_panes,
                now,
            );
            let resolved_panes: HashSet<PaneId> = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.is_local_host()
                        && s.state != SessionState::Ended
                        && s.transcript_path.is_some()
                        && s.metrics.is_some()
                })
                .filter_map(|s| s.pane.clone())
                .collect();
            let claimed_claude_entries = collect_claimed_agent_entries(&st, AgentKind::Claude);
            let live_claimed_codex_entries =
                collect_live_claimed_agent_entries(&st, AgentKind::Codex);
            let claimed_agy_entries = collect_claimed_agent_entries(&st, AgentKind::Agy);
            (
                delta,
                resolved_panes,
                claimed_claude_entries,
                live_claimed_codex_entries,
                claimed_agy_entries,
            )
        };

        if delta.changed {
            self.mark_dirty();
            for s in &delta.updated {
                self.publish_session(s.clone());
            }
            self.refresh_attention();
        }

        // Exact PID-subtree / resume-UUID transcript resolution + batched/fingerprint-cached
        // tail captures + off-lock SQLite, `ps -o etime=`, & `lsof` queries.
        struct PaneScanTarget {
            pane: PaneId,
            agent: AgentKind,
            cwd: String,
            agent_pid: Option<u32>,
            subtree_pids: Vec<u32>,
            resume_id: Option<String>,
            is_legacy: bool,
            pane_in_mode: bool,
            needs_tail_fallback: bool,
            history_size: u32,
            cursor_x: u32,
            cursor_y: u32,
            current_command: String,
        }
        let legacy_set: HashSet<PaneId> =
            delta.active_legacy.iter().map(|(_, p)| p.clone()).collect();
        let prev_codex_title_bindings: HashMap<PaneId, String> = lock(&self.inner.discovery_cache)
            .codex_title_bindings
            .clone();
        let pane_targets: Vec<PaneScanTarget> = discovered
            .iter()
            .filter(|d| !d.pane_dead)
            .map(|d| PaneScanTarget {
                is_legacy: legacy_set.contains(&d.pane_id),
                pane_in_mode: d.pane_in_mode,
                needs_tail_fallback: !resolved_panes.contains(&d.pane_id)
                    || prev_codex_title_bindings.contains_key(&d.pane_id),
                pane: d.pane_id.clone(),
                agent: d.agent,
                cwd: d.cwd.clone(),
                agent_pid: d.agent_pid,
                subtree_pids: if !d.subtree_pids.is_empty() {
                    d.subtree_pids.clone()
                } else {
                    d.agent_pid.into_iter().collect()
                },
                resume_id: d.codex_resume_id.clone(),
                history_size: d.history_size,
                cursor_x: d.cursor_x,
                cursor_y: d.cursor_y,
                current_command: d.pane_current_command.clone(),
            })
            .collect();

        let now_inst = std::time::Instant::now();
        let poll_now = id::now_ms();
        let core_cap = self.clone();
        let home_for_scan = home_dir.clone();

        struct OffLockScanResult {
            tails: HashMap<PaneId, String>,
            exact_paths: HashMap<PaneId, PathBuf>,
            authoritative_panes: HashSet<PaneId>,
            codex_cleared_panes: HashSet<PaneId>,
            agy_lock_conv_by_pane: HashMap<PaneId, String>,
            pane_birth_by_pane: HashMap<PaneId, u64>,
            spawned_meta_by_pane: HashMap<PaneId, SpawnedPaneMeta>,
        }

        let off_lock = tokio::task::spawn_blocking(move || {
            let mut tails: HashMap<PaneId, String> = HashMap::new();
            let mut exact_paths: HashMap<PaneId, PathBuf> = HashMap::new();
            let mut codex_needing_match: Vec<(PaneId, String, u64)> = Vec::new();
            let mut agy_targets: Vec<(PaneId, String, Vec<u32>, u64)> = Vec::new();
            let mut batch_capture_targets: Vec<(PaneId, usize)> = Vec::new();
            let mut target_meta: HashMap<PaneId, (u32, u32, u32, bool, String)> = HashMap::new();

            // 1. Refresh OS process birth timestamps (`ps -o pid=,etime=`) for newly seen live PIDs,
            //    pruning dead PIDs so recycled OS PIDs never hit stale birth timestamps.
            let all_live_pids: HashSet<u32> = pane_targets
                .iter()
                .flat_map(|t| t.subtree_pids.iter().copied().chain(t.agent_pid))
                .filter(|&p| p > 0)
                .collect();
            let missing_birth_pids: Vec<u32> = {
                let mut dc = lock(&core_cap.inner.discovery_cache);
                dc.pid_birth_ms.retain(|pid, _| all_live_pids.contains(pid));
                all_live_pids
                    .iter()
                    .copied()
                    .filter(|pid| !dc.pid_birth_ms.contains_key(pid))
                    .collect()
            };
            if !missing_birth_pids.is_empty() {
                let queried = query_pids_birth_ms(&missing_birth_pids, poll_now);
                let mut dc = lock(&core_cap.inner.discovery_cache);
                for (pid, bms) in queried {
                    dc.pid_birth_ms.insert(pid, bms);
                }
            }

            let all_agy_pids: Vec<u32> = pane_targets
                .iter()
                .filter(|t| t.agent == AgentKind::Agy)
                .flat_map(|t| t.subtree_pids.iter().copied())
                .collect();
            let stale_agy_pids = if all_agy_pids.is_empty() {
                Vec::new()
            } else {
                lock(&core_cap.inner.telemetry)
                    .agy_presence_cache
                    .stale_pids(&all_agy_pids, now_inst)
            };
            let lsof_map = if stale_agy_pids.is_empty() {
                HashMap::new()
            } else {
                query_lsof_agy_presence_all(&stale_agy_pids)
            };

            let mut pane_birth_by_pane: HashMap<PaneId, u64> = HashMap::new();
            let mut spawned_meta_by_pane: HashMap<PaneId, SpawnedPaneMeta> = HashMap::new();
            let mut authoritative_panes: HashSet<PaneId> = HashSet::new();

            {
                let mut dc = lock(&core_cap.inner.discovery_cache);
                let pid_births_snap = dc.pid_birth_ms.clone();
                for t in &pane_targets {
                    let proc_birth = t
                        .agent_pid
                        .and_then(|pid| pid_births_snap.get(&pid).copied())
                        .or_else(|| {
                            t.subtree_pids
                                .iter()
                                .filter_map(|pid| pid_births_snap.get(pid).copied())
                                .max()
                        });
                    let spawned_meta = dc.spawned_panes.get(&t.pane).cloned();
                    if let Some(sm) = &spawned_meta {
                        spawned_meta_by_pane.insert(t.pane.clone(), sm.clone());
                    }
                    let first_seen = dc
                        .pane_first_seen_ms
                        .get(&t.pane)
                        .copied()
                        .unwrap_or(poll_now);
                    let eff_birth_ms =
                        match (proc_birth, spawned_meta.as_ref().map(|m| m.spawned_at_ms)) {
                            (Some(pb), Some(sb)) => pb.max(sb),
                            (Some(pb), None) => pb,
                            (None, Some(sb)) => sb,
                            (None, None) => first_seen,
                        };
                    pane_birth_by_pane.insert(t.pane.clone(), eff_birth_ms);

                    if t.agent == AgentKind::Claude {
                        if let Some(home) = home_for_scan.as_deref() {
                            let exclude = excluded_ids_for_pane(&claimed_claude_entries, &t.pane);
                            if let Some((_, p)) = resolve_claude_transcript_by_pids_in(
                                home,
                                &t.subtree_pids,
                                &pid_births_snap,
                                &exclude,
                            ) {
                                exact_paths.insert(t.pane.clone(), p);
                                authoritative_panes.insert(t.pane.clone());
                            }
                        }
                    } else if t.agent == AgentKind::Codex {
                        if let Some(p) = t.resume_id.as_deref().and_then(|uid| {
                            dc.cached_codex_resume_path(home_for_scan.as_deref(), uid)
                        }) {
                            exact_paths.insert(t.pane.clone(), p);
                            authoritative_panes.insert(t.pane.clone());
                        } else {
                            codex_needing_match.push((t.pane.clone(), t.cwd.clone(), eff_birth_ms));
                        }
                    } else if t.agent == AgentKind::Agy {
                        agy_targets.push((
                            t.pane.clone(),
                            t.cwd.clone(),
                            t.subtree_pids.clone(),
                            eff_birth_ms,
                        ));
                    }
                    if should_capture_tail_for_scan(
                        t.pane_in_mode,
                        t.is_legacy,
                        t.agent,
                        t.needs_tail_fallback,
                    ) {
                        // Zellij does not report `history_size`, and tmux scrollback can saturate
                        // at `history-limit` or update in-place: bound cache reuse to `TAIL_CACHE_MAX_TICKS`.
                        let is_zellij = t.pane.as_str().starts_with("zj:");
                        let cached_tail = if is_zellij {
                            None
                        } else {
                            dc.tail_cache.get_mut(&t.pane).and_then(|c| {
                                if c.history_size == t.history_size
                                    && c.cursor_x == t.cursor_x
                                    && c.cursor_y == t.cursor_y
                                    && c.in_mode == t.pane_in_mode
                                    && c.current_command == t.current_command
                                    && c.ticks_cached < TAIL_CACHE_MAX_TICKS
                                {
                                    c.ticks_cached = c.ticks_cached.saturating_add(1);
                                    Some(c.tail.clone())
                                } else {
                                    None
                                }
                            })
                        };
                        if let Some(hit_tail) = cached_tail {
                            tails.insert(t.pane.clone(), hit_tail);
                        } else {
                            batch_capture_targets.push((t.pane.clone(), LEGACY_HASH_LINES));
                            target_meta.insert(
                                t.pane.clone(),
                                (
                                    t.history_size,
                                    t.cursor_x,
                                    t.cursor_y,
                                    t.pane_in_mode,
                                    t.current_command.clone(),
                                ),
                            );
                        }
                    }
                }
            }

            // Capture ALL changed panes in ONE single batched `tmux` invocation!
            if !batch_capture_targets.is_empty() {
                let refs: Vec<(&PaneId, usize)> =
                    batch_capture_targets.iter().map(|(p, n)| (p, *n)).collect();
                let fresh_tails = core_cap.inner.mux.capture_panes_tail_batch(&refs);
                let mut dc = lock(&core_cap.inner.discovery_cache);
                for (pid, text) in fresh_tails {
                    if let Some((hs, cx, cy, im, cmd)) = target_meta.remove(&pid) {
                        dc.tail_cache.insert(
                            pid.clone(),
                            TailCacheEntry {
                                history_size: hs,
                                cursor_x: cx,
                                cursor_y: cy,
                                in_mode: im,
                                current_command: cmd,
                                ticks_cached: 0,
                                tail: text.clone(),
                            },
                        );
                    }
                    tails.insert(pid, text);
                }
            }

            // Refresh SQLite caches + presence cache completely outside `Mutex<State>`.
            let mut agy_lock_conv_by_pane: HashMap<PaneId, String> = HashMap::new();
            let mut codex_cleared_panes: HashSet<PaneId> = HashSet::new();
            let mut codex_new_title_bindings: HashMap<PaneId, String> = HashMap::new();
            {
                let mut tc = lock(&core_cap.inner.telemetry);
                if !stale_agy_pids.is_empty() {
                    tc.agy_presence_cache
                        .update_all(&stale_agy_pids, &lsof_map, now_inst);
                }
                if !codex_needing_match.is_empty() {
                    tc.codex_threads_cache
                        .refresh_if_needed_in(home_for_scan.as_deref());
                    // Explicit status-bar title matching works across multiple Codex panes in the
                    // same CWD (blank siblings return `None` and never match) AND across interactive
                    // `/resume` threads (`created_at < birth_ms`), while still rejecting ambiguous
                    // same-`(cwd, title)` collisions between two live panes.
                    let mut codex_title_counts: HashMap<(String, String), usize> = HashMap::new();
                    for (pane, cwd, _) in &codex_needing_match {
                        if let Some(tail) = tails.get(pane) {
                            if let Some(title) = extract_codex_status_title(tail) {
                                *codex_title_counts
                                    .entry((cwd.trim_end_matches('/').to_string(), title))
                                    .or_insert(0) += 1;
                            }
                        }
                    }
                    let mut newly_claimed_codex_ids: HashSet<String> = HashSet::new();
                    for (pane, cwd, _birth_ms) in &codex_needing_match {
                        let Some(tail) = tails.get(pane) else {
                            if let Some(old_title) = prev_codex_title_bindings.get(pane) {
                                codex_new_title_bindings.insert(pane.clone(), old_title.clone());
                                authoritative_panes.insert(pane.clone());
                            }
                            continue;
                        };
                        let cur_title = extract_codex_status_title(tail);
                        if prev_codex_title_bindings
                            .get(pane)
                            .is_some_and(|old| cur_title.as_deref() != Some(old.as_str()))
                        {
                            codex_cleared_panes.insert(pane.clone());
                        }
                        let Some(title) = cur_title else {
                            continue;
                        };
                        let unique_title = codex_title_counts
                            .get(&(cwd.trim_end_matches('/').to_string(), title.clone()))
                            .copied()
                            .unwrap_or(0)
                            == 1;
                        if !unique_title {
                            continue;
                        }
                        let mut exclude = excluded_ids_for_pane(&live_claimed_codex_entries, pane);
                        exclude.extend(newly_claimed_codex_ids.iter().cloned());
                        if let Some((cid, p)) = tc
                            .codex_threads_cache
                            .match_pane_by_title(cwd, tail, &exclude)
                        {
                            newly_claimed_codex_ids.insert(cid);
                            exact_paths.insert(pane.clone(), p);
                            authoritative_panes.insert(pane.clone());
                            codex_new_title_bindings.insert(pane.clone(), title);
                            codex_cleared_panes.remove(pane);
                        }
                    }
                }
                if !agy_targets.is_empty() {
                    tc.agy_convs_cache
                        .refresh_if_needed_in(home_for_scan.as_deref());
                    let mut newly_claimed_agy_ids: HashSet<String> = HashSet::new();
                    let mut unmatched_agy: Vec<(PaneId, String, u64)> = Vec::new();
                    for (pane, cwd, pids, birth_ms) in &agy_targets {
                        if let Some(conv_id) = tc
                            .agy_presence_cache
                            .find_for_pids_preferring_roots(pids, &tc.agy_convs_cache)
                        {
                            newly_claimed_agy_ids.insert(conv_id.clone());
                            agy_lock_conv_by_pane.insert(pane.clone(), conv_id.clone());
                            authoritative_panes.insert(pane.clone());
                            if let Some(p) = resolve_agy_transcript_by_id_opt_home(
                                home_for_scan.as_deref(),
                                &conv_id,
                            ) {
                                exact_paths.insert(pane.clone(), p);
                            }
                        } else {
                            unmatched_agy.push((pane.clone(), cwd.clone(), *birth_ms));
                        }
                    }
                    if !unmatched_agy.is_empty() {
                        let mut unclaimed_cwd_counts: HashMap<String, usize> = HashMap::new();
                        for (_, cwd, _) in &unmatched_agy {
                            *unclaimed_cwd_counts
                                .entry(cwd.trim_end_matches('/').to_string())
                                .or_insert(0) += 1;
                        }
                        for (pane, cwd, birth_ms) in &unmatched_agy {
                            let unique = unclaimed_cwd_counts
                                .get(cwd.trim_end_matches('/'))
                                .copied()
                                .unwrap_or(0)
                                == 1;
                            let mut exclude = excluded_ids_for_pane(&claimed_agy_entries, pane);
                            exclude.extend(newly_claimed_agy_ids.iter().cloned());
                            if let Some((cid, p)) =
                                tc.agy_convs_cache.match_unique_cwd_excluding_in(
                                    home_for_scan.as_deref(),
                                    cwd,
                                    unique,
                                    &exclude,
                                    Some(*birth_ms),
                                )
                            {
                                newly_claimed_agy_ids.insert(cid);
                                exact_paths.entry(pane.clone()).or_insert(p);
                            }
                        }
                    }
                }
            }

            {
                let mut dc = lock(&core_cap.inner.discovery_cache);
                dc.pane_birth_ms = pane_birth_by_pane.clone();
                dc.authoritative_panes = authoritative_panes.clone();
                dc.codex_title_bindings = codex_new_title_bindings;
            }

            OffLockScanResult {
                tails,
                exact_paths,
                authoritative_panes,
                codex_cleared_panes,
                agy_lock_conv_by_pane,
                pane_birth_by_pane,
                spawned_meta_by_pane,
            }
        })
        .await;

        let Ok(OffLockScanResult {
            tails,
            exact_paths,
            authoritative_panes,
            codex_cleared_panes,
            agy_lock_conv_by_pane,
            pane_birth_by_pane,
            spawned_meta_by_pane,
        }) = off_lock
        else {
            return;
        };

        // Short State mutation #1: promote AGY roots, backfill exact transcript paths, apply legacy
        // tail-hash transitions, and collect lightweight snapshots for off-lock AGY-child + telemetry IO.
        struct AgyRootRef {
            sid: SessionId,
            cwd: Option<String>,
            git_root: Option<String>,
            ext_id: String,
        }
        struct RootMetricsInput {
            sid: SessionId,
            agent: AgentKind,
            pane: Option<PaneId>,
            model: Option<String>,
            label: Option<String>,
            tp: Option<PathBuf>,
            ext_sid: Option<String>,
        }
        struct SubagentTranscriptInput {
            sid: SessionId,
            agent: AgentKind,
            tp: PathBuf,
        }

        let mut transitions = Vec::new();
        let mut metrics_updated = Vec::new();
        let (agy_roots, live_roots, live_subagents, live_shell_panes, live_local_sids) = {
            let mut st = lock(&self.inner.state);

            for (pane, conv_id) in &agy_lock_conv_by_pane {
                let leg_id = legacy_session_id(pane);
                let real_id = SessionId::new(conv_id.clone());
                let prev_legacy = st.sessions.get(&leg_id).cloned();
                if let Some(old_parent) = st.promote_misfolded_agy_root(&real_id, &leg_id, pane) {
                    if let Some(mut dead) = prev_legacy {
                        dead.state = SessionState::Ended;
                        dead.state_since_ms = poll_now;
                        dead.last_event_ms = poll_now;
                        metrics_updated.push(dead);
                    }
                    if let Some(op) = old_parent {
                        if let Some(op_rec) = st.sessions.get(&op) {
                            metrics_updated.push(op_rec.clone());
                        }
                    }
                    if let Some(rec) = st.sessions.get(&real_id) {
                        metrics_updated.push(rec.clone());
                    }
                }
                for s in st.sessions.values_mut() {
                    if s.session_id != real_id
                        && s.session_id != leg_id
                        && !s.is_subagent
                        && s.agent == AgentKind::Agy
                        && s.pane.as_ref() == Some(pane)
                        && s.state != SessionState::Ended
                    {
                        s.state = SessionState::Ended;
                        super::state::clear_tmux_binding(s);
                        s.state_since_ms = poll_now;
                        metrics_updated.push(s.clone());
                    }
                }
            }

            let discovered_by_pane: HashMap<PaneId, &DiscoveredPane> = discovered
                .iter()
                .filter(|d| !d.pane_dead)
                .map(|d| (d.pane_id.clone(), d))
                .collect();
            for (pane, path) in &exact_paths {
                let path_str = path.to_string_lossy().into_owned();
                let pane_agent = discovered_by_pane.get(pane).map(|d| d.agent);
                let leg_id = legacy_session_id(pane);
                let allow_revive = authoritative_panes.contains(pane);
                if let Some((real_id, revived)) = find_matching_or_revivable_real_session(
                    &st,
                    pane,
                    pane_agent,
                    &path_str,
                    allow_revive,
                ) {
                    let prev_legacy = st.sessions.get(&leg_id).cloned();
                    let merged = st.merge_legacy_into_real(&leg_id, &real_id);
                    if merged {
                        if let Some(mut dead) = prev_legacy {
                            dead.state = SessionState::Ended;
                            dead.state_since_ms = poll_now;
                            dead.last_event_ms = poll_now;
                            metrics_updated.push(dead);
                        }
                    }
                    if let Some(rec) = st.sessions.get_mut(&real_id) {
                        let mut c = merged;
                        if revived && rec.state == SessionState::Ended {
                            rec.state = SessionState::Idle;
                            rec.state_since_ms = poll_now;
                            rec.last_error = None;
                            c = true;
                        }
                        if rec.pane.as_ref() != Some(pane) {
                            rec.pane = Some(pane.clone());
                            rec.pane_resolution = PaneResolution::PidTree;
                            c = true;
                        }
                        if rec.transcript_path.as_deref() != Some(path_str.as_str()) {
                            rec.transcript_path = Some(path_str.clone());
                            c = true;
                        }
                        if let Some(dp) = discovered_by_pane.get(pane) {
                            if rec.tmux_target.as_deref() != Some(dp.tmux_target.as_str()) {
                                rec.tmux_target = Some(dp.tmux_target.clone());
                                c = true;
                            }
                            if rec.tmux_label.as_deref() != Some(dp.tmux_label.as_str()) {
                                rec.tmux_label = Some(dp.tmux_label.clone());
                                c = true;
                            }
                            if rec.git_root.is_none() && dp.git_root.is_some() {
                                rec.git_root = dp.git_root.clone();
                                c = true;
                            }
                            if rec.model.is_none() && dp.model.is_some() {
                                rec.model = dp.model.clone();
                                c = true;
                            }
                            if rec.effort.is_none() && dp.effort.is_some() {
                                rec.effort = dp.effort.clone();
                                c = true;
                            }
                        }
                        if c {
                            metrics_updated.push(rec.clone());
                        }
                    }
                    continue;
                }

                for rec in st.sessions.values_mut() {
                    if !rec.is_subagent
                        && rec.state != SessionState::Ended
                        && rec.pane.as_ref() == Some(pane)
                        && rec.transcript_path.as_deref() != Some(path_str.as_str())
                    {
                        if rec.transcript_path.is_some() {
                            rec.metrics = None;
                        }
                        rec.transcript_path = Some(path_str.clone());
                        metrics_updated.push(rec.clone());
                    }
                }
            }

            // Actively self-heal any `discovered_only` session holding an unverified pre-birth or
            // cross-session-claimed `transcript_path` / `metrics` (e.g. from pre-fix snapshots),
            // as well as Codex title-bound panes whose user switched to a blank `/new` prompt.
            let claim_refs = snapshot_claim_refs(&st);
            for rec in st.sessions.values_mut() {
                if !rec.discovered_only
                    || rec.is_subagent
                    || !rec.is_local_host()
                    || rec.state == SessionState::Ended
                {
                    continue;
                }
                let Some(pane) = rec.pane.clone() else {
                    continue;
                };
                let is_auth = authoritative_panes.contains(&pane);
                let title_cleared = rec.agent == AgentKind::Codex
                    && codex_cleared_panes.contains(&pane)
                    && !exact_paths.contains_key(&pane);
                let min_birth = pane_birth_by_pane.get(&pane).copied();
                let invalid_tp = rec.transcript_path.as_deref().is_some_and(|tp| {
                    if title_cleared {
                        return true;
                    }
                    let claimed_elsewhere = is_transcript_claimed_in_refs(
                        &claim_refs,
                        &rec.session_id,
                        Some(&pane),
                        rec.agent,
                        tp,
                    );
                    let p = Path::new(tp);
                    let missing = !p.is_file();
                    let pre_birth = !is_auth
                        && !missing
                        && min_birth.is_some_and(|b| {
                            self.cached_transcript_birth_ms(p)
                                .is_none_or(|tb| !is_post_birth_timestamp(tb, b))
                        });
                    claimed_elsewhere || missing || pre_birth
                });

                let mut scrubbed = false;
                if invalid_tp {
                    rec.transcript_path = None;
                    rec.metrics = None;
                    scrubbed = true;
                } else if rec.transcript_path.is_none() {
                    let invalid_metrics = rec.metrics.as_ref().is_some_and(|m| {
                        m.source == "cc_switch"
                            || m.source == "transcript"
                            || m.source == "agy_db"
                            || (!is_auth
                                && min_birth
                                    .is_some_and(|b| !is_post_birth_timestamp(m.updated_ms, b)))
                    });
                    if invalid_metrics {
                        rec.metrics = None;
                        scrubbed = true;
                    }
                }

                if scrubbed {
                    let (st_model, st_effort) = if rec.agent == AgentKind::Codex {
                        tails
                            .get(&pane)
                            .map(|t| parse_codex_status_line_model_effort(t))
                            .unwrap_or((None, None))
                    } else {
                        (None, None)
                    };
                    let fallback_model = spawned_meta_by_pane
                        .get(&pane)
                        .and_then(|sm| sm.model.clone())
                        .or_else(|| {
                            discovered_by_pane
                                .get(&pane)
                                .and_then(|dp| dp.model.clone())
                        })
                        .or(st_model);
                    let fallback_effort = spawned_meta_by_pane
                        .get(&pane)
                        .and_then(|sm| sm.effort.clone())
                        .or_else(|| {
                            discovered_by_pane
                                .get(&pane)
                                .and_then(|dp| dp.effort.clone())
                        })
                        .or(st_effort)
                        .or_else(|| {
                            (rec.agent == AgentKind::Agy)
                                .then(|| {
                                    fallback_model
                                        .as_deref()
                                        .and_then(extract_agy_effort_from_model)
                                })
                                .flatten()
                        });
                    rec.model = fallback_model;
                    rec.effort = fallback_effort;
                    metrics_updated.push(rec.clone());
                }
            }

            for (sid, pane) in &delta.active_legacy {
                if let Some(tail) = tails.get(pane) {
                    if let Some(tr) = apply_legacy_tail_hash(&mut st, sid, pane, tail, poll_now) {
                        transitions.push(tr);
                    }
                }
            }

            let agy_roots: Vec<AgyRootRef> = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.is_local_host()
                        && s.agent == AgentKind::Agy
                        && !matches!(s.state, SessionState::Ended | SessionState::Suspended)
                })
                .filter_map(|s| {
                    let ext_id = canonical_external_session_id(s)?;
                    Some(AgyRootRef {
                        sid: s.session_id.clone(),
                        cwd: s.cwd.clone(),
                        git_root: s.git_root.clone(),
                        ext_id,
                    })
                })
                .collect();

            let live_roots: Vec<RootMetricsInput> = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.is_local_host()
                        && !s.agent.is_shell()
                        && !matches!(s.state, SessionState::Ended | SessionState::Suspended)
                })
                .map(|s| RootMetricsInput {
                    sid: s.session_id.clone(),
                    agent: s.agent,
                    pane: s.pane.clone(),
                    model: s.model.clone(),
                    label: s.tmux_label.clone().or_else(|| s.tmux_target.clone()),
                    tp: resolve_transcript_path_with_home(home_dir.as_deref(), s),
                    ext_sid: canonical_external_session_id(s),
                })
                .collect();

            let live_subagents: Vec<SubagentTranscriptInput> = st
                .sessions
                .values()
                .filter(|s| {
                    s.is_subagent
                        && s.is_local_host()
                        && !s.agent.is_shell()
                        && !matches!(s.state, SessionState::Ended | SessionState::Suspended)
                })
                .filter_map(|s| {
                    resolve_transcript_path_with_home(home_dir.as_deref(), s).map(|tp| {
                        SubagentTranscriptInput {
                            sid: s.session_id.clone(),
                            agent: s.agent,
                            tp,
                        }
                    })
                })
                .collect();

            let live_shell_panes: Vec<(SessionId, PaneId)> = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.is_local_host()
                        && s.agent.is_shell()
                        && !matches!(s.state, SessionState::Ended | SessionState::Suspended)
                })
                .filter_map(|s| s.pane.clone().map(|p| (s.session_id.clone(), p)))
                .collect();

            let live_local_sids: HashSet<SessionId> = st
                .sessions
                .values()
                .filter(|s| s.is_local_host() && s.state != SessionState::Ended)
                .map(|s| s.session_id.clone())
                .collect();

            (
                agy_roots,
                live_roots,
                live_subagents,
                live_shell_panes,
                live_local_sids,
            )
        };

        // Off-lock pass #2: fetch AGY child summaries + O(delta) transcript telemetry + cc-switch lookup
        // + incremental `DaemonEvent::Output` deltas completely outside `Mutex<State>`.
        struct ComputedSubagentOutput {
            sid: SessionId,
            metrics: Option<SessionMetrics>,
            output_delta: Option<TranscriptOutputDelta>,
        }
        struct AgyChildrenBatch {
            root_sid: SessionId,
            cwd: Option<String>,
            git_root: Option<String>,
            kids: Vec<AgyDiscoveredChild>,
            root_is_idle: bool,
        }

        let core_tel = self.clone();
        let home_for_children = home_dir.clone();
        let (agy_batches, computed_metrics, computed_subagents, shell_output_pushes) =
            tokio::task::spawn_blocking(move || {
                let mut live_local_sids = live_local_sids;
                let agy_batches: Vec<AgyChildrenBatch> = {
                    let tc = lock(&core_tel.inner.telemetry);
                    agy_roots
                        .into_iter()
                        .map(|r| {
                            let kids = tc
                                .agy_convs_cache
                                .children_of_in(home_for_children.as_deref(), &r.ext_id);
                            let root_is_idle = tc.agy_convs_cache.root_is_idle(&r.ext_id);
                            AgyChildrenBatch {
                                root_sid: r.sid,
                                cwd: r.cwd,
                                git_root: r.git_root,
                                kids,
                                root_is_idle,
                            }
                        })
                        .collect()
                };

                let agy_kid_count: usize = agy_batches.iter().map(|b| b.kids.len()).sum();
                let working_set = live_roots
                    .len()
                    .saturating_add(live_subagents.len())
                    .saturating_add(agy_kid_count)
                    .saturating_add(64);
                lock(&core_tel.inner.transcript_index).ensure_min_capacity(working_set);

                let mut computed = Vec::with_capacity(live_roots.len());
                for item in live_roots {
                    let has_transcript = item.tp.is_some();
                    let (mut tr_metrics, tr_model, tr_effort, output_delta) =
                        match item.tp.as_deref() {
                            Some(p) => sync_transcript_and_output_delta_with_label(
                                &core_tel,
                                &item.sid,
                                item.agent,
                                p,
                                poll_now,
                                item.label.as_deref(),
                            ),
                            None => (None, None, None, None),
                        };
                    if item.agent == AgentKind::Agy {
                        let conv_id = item.ext_sid.clone().or_else(|| {
                            item.tp.as_deref().and_then(|p| {
                                extract_session_id_from_transcript_path(AgentKind::Agy, p)
                            })
                        });
                        if let Some(cid) = conv_id.as_deref() {
                            let model_hint = tr_model.as_deref().or(item.model.as_deref());
                            let db_enriched = {
                                let mut tc = lock(&core_tel.inner.telemetry);
                                let timings = tc
                                    .agy_step_timings_cache
                                    .get_or_refresh_in(home_for_children.as_deref(), cid);
                                agy_step_timings_to_metrics(
                                    timings,
                                    model_hint,
                                    tr_metrics.clone(),
                                    poll_now,
                                )
                            };
                            if db_enriched.is_some() {
                                tr_metrics = db_enriched;
                            }
                        }
                    }
                    let pane_tail = item.pane.as_ref().and_then(|p| tails.get(p));
                    let (st_model, st_effort) = if item.agent == AgentKind::Codex {
                        pane_tail
                            .map(|t| parse_codex_status_line_model_effort(t))
                            .unwrap_or((None, None))
                    } else {
                        (None, None)
                    };
                    let st_metrics =
                        pane_tail.and_then(|t| parse_status_line_metrics(item.agent, t, poll_now));
                    let live_tail = pane_tail.and_then(|t| {
                        check_live_tail_changed(&core_tel, &item.sid, t).then(|| t.clone())
                    });
                    let combined = merge_metrics(tr_metrics, st_metrics);
                    computed.push(ComputedRootMetrics {
                        sid: item.sid,
                        agent: item.agent,
                        tr_model,
                        tr_effort,
                        st_model,
                        st_effort,
                        combined,
                        output_delta,
                        live_tail,
                        has_transcript,
                    });
                }

                let mut scanned_subagent_sids: HashSet<SessionId> =
                    HashSet::with_capacity(live_subagents.len());
                let mut subagent_outputs = Vec::with_capacity(live_subagents.len());
                for sub in live_subagents {
                    scanned_subagent_sids.insert(sub.sid.clone());
                    let (sub_metrics, _, _, output_delta) =
                        sync_transcript_and_output_delta_with_label(
                            &core_tel, &sub.sid, sub.agent, &sub.tp, poll_now, None,
                        );
                    subagent_outputs.push(ComputedSubagentOutput {
                        sid: sub.sid,
                        metrics: sub_metrics,
                        output_delta,
                    });
                }
                for batch in &agy_batches {
                    for kid in &batch.kids {
                        if kid.state == SessionState::Ended {
                            continue;
                        }
                        let ksid = SessionId::new(kid.conversation_id.clone());
                        if !scanned_subagent_sids.insert(ksid.clone()) {
                            continue;
                        }
                        live_local_sids.insert(ksid.clone());
                        if kid.transcript_path.is_file() {
                            let (sub_metrics, _, _, output_delta) =
                                sync_transcript_and_output_delta_with_label(
                                    &core_tel,
                                    &ksid,
                                    AgentKind::Agy,
                                    &kid.transcript_path,
                                    poll_now,
                                    kid.agent_name.as_deref(),
                                );
                            subagent_outputs.push(ComputedSubagentOutput {
                                sid: ksid,
                                metrics: sub_metrics,
                                output_delta,
                            });
                        }
                    }
                }

                let mut shell_pushes = Vec::new();
                for (sid, pane) in live_shell_panes {
                    if let Some(tail) = tails.get(&pane) {
                        if check_live_tail_changed(&core_tel, &sid, tail) {
                            shell_pushes.push(PendingOutputPush {
                                sid,
                                start_seq: None,
                                total_messages: None,
                                appended_messages: Vec::new(),
                                live_tail: Some(tail.clone()),
                                metrics: None,
                                dirty: true,
                            });
                        }
                    }
                }
                {
                    let mut dc = lock(&core_tel.inner.discovery_cache);
                    dc.last_output_watermarks
                        .retain(|sid, _| live_local_sids.contains(sid));
                    dc.last_output_tail_hash
                        .retain(|sid, _| live_local_sids.contains(sid));
                }
                (agy_batches, computed, subagent_outputs, shell_pushes)
            })
            .await
            .unwrap_or_default();

        // Short State mutation #2: apply precomputed AGY subagent & metrics updates in microseconds.
        let mut output_pushes: Vec<PendingOutputPush> = shell_output_pushes;
        {
            let mut st = lock(&self.inner.state);

            for batch in agy_batches {
                let AgyChildrenBatch {
                    root_sid,
                    cwd,
                    git_root,
                    kids,
                    root_is_idle,
                } = batch;
                if !kids.is_empty() {
                    let mut total_kids: u32 = 0;
                    let mut active_kids: u32 = 0;
                    for kid in kids {
                        total_kids += 1;
                        if is_subagent_active(kid.state) {
                            active_kids += 1;
                        }
                        let ksid = SessionId::new(kid.conversation_id);
                        let tr_str = kid.transcript_path.to_string_lossy().into_owned();
                        match st.sessions.get_mut(&ksid) {
                            Some(existing) => {
                                let mut c = false;
                                if existing.parent_session_id.as_ref() != Some(&root_sid) {
                                    existing.parent_session_id = Some(root_sid.clone());
                                    c = true;
                                }
                                if !existing.is_subagent {
                                    existing.is_subagent = true;
                                    c = true;
                                }
                                if existing.transcript_path.as_deref() != Some(tr_str.as_str()) {
                                    existing.transcript_path = Some(tr_str);
                                    c = true;
                                }
                                if let Some(name) = kid.agent_name {
                                    if existing.tmux_label.as_deref() != Some(name.as_str()) {
                                        existing.tmux_label = Some(name);
                                        c = true;
                                    }
                                }
                                if existing.discovered_only && existing.state != kid.state {
                                    existing.state = kid.state;
                                    existing.state_since_ms = poll_now;
                                    c = true;
                                }
                                if c {
                                    metrics_updated.push(existing.clone());
                                }
                            }
                            None => {
                                let rec = SessionRecord {
                                    session_id: ksid.clone(),
                                    agent: AgentKind::Agy,
                                    pane: None,
                                    pane_resolution: PaneResolution::Unknown,
                                    cwd: cwd.clone(),
                                    git_root: git_root.clone(),
                                    model: None,
                                    effort: None,
                                    state: kid.state,
                                    state_since_ms: poll_now,
                                    last_event_id: None,
                                    last_event_ms: poll_now,
                                    last_message: None,
                                    last_error: None,
                                    parent_session_id: Some(root_sid.clone()),
                                    is_subagent: true,
                                    subagent_count: 0,
                                    active_subagents: 0,
                                    stage: None,
                                    unread_inbox: 0,
                                    confidence: Confidence::Heuristic,
                                    tmux_target: None,
                                    tmux_label: kid.agent_name,
                                    discovered_only: true,
                                    transcript_path: Some(tr_str),
                                    metrics: None,
                                    host_id: None,
                                    host_display_name: None,
                                    mux_kind: None,
                                    resume_id: None,
                                    suspended_at_ms: None,
                                };
                                st.sessions.insert(ksid, rec.clone());
                                metrics_updated.push(rec);
                            }
                        }
                    }
                    if let Some(parent) = st.sessions.get_mut(&root_sid) {
                        let mut pchanged = false;
                        if parent.subagent_count < total_kids
                            || parent.active_subagents != active_kids
                        {
                            parent.subagent_count = parent.subagent_count.max(total_kids);
                            parent.active_subagents = active_kids;
                            pchanged = true;
                        }
                        if parent.state == SessionState::Working
                            && poll_now.saturating_sub(parent.last_event_ms) >= 10_000
                            && parent.active_subagents == 0
                            && root_is_idle
                        {
                            parent.state = SessionState::Idle;
                            parent.state_since_ms = poll_now;
                            pchanged = true;
                        }
                        if pchanged {
                            metrics_updated.push(parent.clone());
                        }
                    }
                } else if root_is_idle {
                    if let Some(rec) = st.sessions.get_mut(&root_sid) {
                        if rec.state == SessionState::Working
                            && poll_now.saturating_sub(rec.last_event_ms) >= 10_000
                            && rec.active_subagents == 0
                        {
                            rec.state = SessionState::Idle;
                            rec.state_since_ms = poll_now;
                            metrics_updated.push(rec.clone());
                        }
                    }
                }
            }

            for item in computed_metrics {
                if let Some(rec) = st.sessions.get_mut(&item.sid) {
                    apply_computed_root_metrics(
                        rec,
                        item,
                        &authoritative_panes,
                        &pane_birth_by_pane,
                        home_dir.as_deref(),
                        &mut metrics_updated,
                        &mut output_pushes,
                    );
                }
            }

            for sub in computed_subagents {
                if let Some(rec) = st.sessions.get_mut(&sub.sid) {
                    if sub.metrics.is_some()
                        && !metrics_meaningfully_equal(&rec.metrics, &sub.metrics)
                    {
                        rec.metrics = sub.metrics;
                        metrics_updated.push(rec.clone());
                    }
                    if let Some(d) = sub.output_delta {
                        output_pushes.push(PendingOutputPush {
                            sid: sub.sid,
                            start_seq: d.start_seq,
                            total_messages: d.total_messages,
                            appended_messages: d.appended_messages,
                            live_tail: None,
                            metrics: rec.metrics.clone(),
                            dirty: d.dirty,
                        });
                    }
                }
            }
        }

        if !metrics_updated.is_empty() {
            self.mark_dirty();
            let mut latest_by_sid: Vec<SessionRecord> = Vec::with_capacity(metrics_updated.len());
            let mut seen = HashSet::new();
            for s in metrics_updated.into_iter().rev() {
                if seen.insert(s.session_id.clone()) {
                    latest_by_sid.push(s);
                }
            }
            for s in latest_by_sid.into_iter().rev() {
                self.publish_session(s);
            }
        }

        for push in output_pushes {
            self.publish_output(
                push.sid,
                push.start_seq,
                push.total_messages,
                push.appended_messages,
                push.live_tail,
                push.metrics,
                push.dirty,
            );
        }

        if !transitions.is_empty() {
            self.mark_dirty();
            for (session, heuristic_ev) in transitions {
                self.publish_session(session.clone());
                self.mirror_tmux_status(&session);
                if let Some(ev) = heuristic_ev {
                    self.publish(|seq| DaemonEvent::Event {
                        seq,
                        event: ev.clone(),
                    });
                    self.on_session_event(&session, &ev, None, false).await;
                }
            }
            self.refresh_attention();
        }
    }

    /// Lightweight cached metrics refresh for a single session (called on Hook Stop/Start).
    /// Runs SQLite & JSONL IO completely outside `Mutex<State>`.
    pub async fn refresh_session_metrics(&self, sid: &SessionId) {
        let snap = lock(&self.inner.state).sessions.get(sid).cloned();
        let Some(session) = snap else { return };
        if session.is_subagent || session.state == SessionState::Ended {
            return;
        }
        let home_dir = self.effective_home_dir();
        let ext_sid = canonical_external_session_id(&session);
        let path = resolve_transcript_path_with_home(home_dir.as_deref(), &session);
        if path.is_none() && ext_sid.is_none() {
            return;
        }
        let agent = session.agent;
        let session_model = session.model.clone();
        let now_ms = id::now_ms();
        let core = self.clone();
        let home_for_db = home_dir.clone();
        let sid_owned = sid.clone();
        let (tr, tr_model, tr_effort, output_delta) = tokio::task::spawn_blocking(move || {
            let (mut tr, tr_model, tr_effort, output_delta) = match path.as_deref() {
                Some(p) => sync_transcript_and_output_delta(&core, &sid_owned, agent, p, now_ms),
                None => (None, None, None, None),
            };
            if agent == AgentKind::Agy {
                let conv_id = ext_sid.or_else(|| {
                    path.as_deref()
                        .and_then(|p| extract_session_id_from_transcript_path(AgentKind::Agy, p))
                });
                if let Some(cid) = conv_id.as_deref() {
                    let model_hint = tr_model.as_deref().or(session_model.as_deref());
                    let db_enriched = {
                        let mut tc = lock(&core.inner.telemetry);
                        let timings = tc
                            .agy_step_timings_cache
                            .get_or_refresh_in(home_for_db.as_deref(), cid);
                        agy_step_timings_to_metrics(timings, model_hint, tr.clone(), now_ms)
                    };
                    if db_enriched.is_some() {
                        tr = db_enriched;
                    }
                }
            }
            (tr, tr_model, tr_effort, output_delta)
        })
        .await
        .unwrap_or((None, None, None, None));

        let (updated, final_metrics) = {
            let mut st = lock(&self.inner.state);
            let Some(rec) = st.sessions.get_mut(sid) else {
                return;
            };
            let mut changed = false;
            if rec.model.is_none() && tr_model.is_some() {
                rec.model = tr_model;
                changed = true;
            }
            let next_effort = if agent == AgentKind::Codex && tr_effort.is_some() {
                tr_effort
            } else if agent == AgentKind::Agy {
                rec.model
                    .as_deref()
                    .and_then(extract_agy_effort_from_model)
                    .or_else(|| rec.effort.clone())
            } else if rec.effort.is_some() {
                rec.effort.clone()
            } else {
                infer_session_effort(
                    agent,
                    rec.model.as_deref(),
                    None,
                    tr_effort.as_deref(),
                    home_dir.as_deref(),
                )
            };
            if next_effort.is_some() && rec.effort != next_effort {
                rec.effort = next_effort;
                changed = true;
            }
            let prev_non_cc = rec
                .metrics
                .as_ref()
                .filter(|m| m.source != "cc_switch")
                .cloned();
            let merged = merge_metrics(tr, prev_non_cc);
            if merged.is_some() && !metrics_meaningfully_equal(&rec.metrics, &merged) {
                rec.metrics = merged;
                changed = true;
            }
            (changed.then(|| rec.clone()), rec.metrics.clone())
        };

        if let Some(s) = updated {
            self.mark_dirty();
            self.publish_session(s);
        }
        if let Some(delta) = output_delta {
            self.publish_output(
                sid.clone(),
                delta.start_seq,
                delta.total_messages,
                delta.appended_messages,
                None,
                final_metrics,
                delta.dirty,
            );
        }
    }

    /// Fetch live terminal output (tail, cursor-paginated scrollback, or full `-S -`) for `pane`
    /// across local tmux/zellij or federated SSH/Peer hosts.
    pub async fn capture_pane(
        &self,
        pane: &PaneId,
        req: CaptureRequest,
    ) -> Result<PanePreview, ApiError> {
        let (host_opt, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
        if let Some(host) = host_opt {
            let hid = HostId::new(host);
            if let Some(provider) = self.inner.remote_nodes.get(&hid).cloned() {
                return provider.capture_pane(&PaneId::new(local_pane), &req).await;
            }
            return Err(ApiError::not_found(format!("unknown remote host {host}")));
        }
        let core = self.clone();
        let pane = pane.clone();
        tokio::task::spawn_blocking(move || core.inner.mux.capture_pane(&pane, &req))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| match e {
                starfane_tmux::TmuxError::InvalidRequest(m) => ApiError::bad_request(m),
                starfane_tmux::TmuxError::Command { stderr, .. }
                    if stderr.contains("not found") || stderr.contains("can't find pane") =>
                {
                    ApiError::not_found(stderr)
                }
                other => ApiError::new(ErrorCode::Unavailable, other.to_string()),
            })
    }

    /// Fetch live terminal tail preview for `pane`.
    pub async fn preview_pane(&self, pane: &PaneId, lines: usize) -> Result<PanePreview, ApiError> {
        self.capture_pane(pane, CaptureRequest::tail(lines)).await
    }

    /// Resolve the working directory for direct interactive spawns (`~/.agent_session` by default,
    /// auto-created on first use, or the explicitly specified directory / project prefix).
    pub(crate) fn resolve_spawn_cwd_in(
        &self,
        home: Option<&Path>,
        project_root: Option<&str>,
    ) -> Result<String, ApiError> {
        resolve_spawn_cwd_with(home, project_root, |needle| self.find_project(needle))
    }

    /// Default project root fallback when the caller leaves `project_root` empty.
    pub fn default_project_root(&self) -> String {
        {
            let st = lock(&self.inner.state);
            if let Some(root) = st
                .sessions
                .values()
                .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
                .max_by_key(|s| s.last_event_ms)
                .and_then(|s| s.git_root.clone().or_else(|| s.cwd.clone()))
                .filter(|p| Path::new(p).is_dir())
            {
                return root;
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(root) = util::git_root(&cwd) {
                return root;
            }
            return cwd.to_string_lossy().into_owned();
        }
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    }

    /// List live tmux session names (sorted, empty when tmux server is not running).
    /// When `host_id` is `Some(h)` and `h != "local"`, proxies to the configured remote host.
    pub async fn list_tmux_sessions(&self, host_id: Option<&str>) -> Result<Vec<String>, ApiError> {
        if let Some(raw) = host_id
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != LOCAL_HOST_ID)
        {
            let hid = HostId::new(raw);
            let provider = self
                .inner
                .remote_nodes
                .get(&hid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {raw}")))?;
            return provider.list_tmux_sessions().await;
        }
        let core = self.clone();
        tokio::task::spawn_blocking(move || core.inner.tmux.list_sessions())
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("tmux list-sessions: {e}")))
    }

    /// Spawn a standalone interactive session directly in tmux (L4, no pipeline required),
    /// targeting either the local host (`None` / `"local"`) or a configured remote host.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_direct_session(
        &self,
        agent: AgentKind,
        host_id: Option<HostId>,
        project_root: Option<String>,
        tmux_session: Option<String>,
        model: Option<String>,
        effort: Option<Effort>,
        prompt: Option<String>,
        name: Option<String>,
    ) -> Result<serde_json::Value, ApiError> {
        if agent.is_shell() {
            return Err(ApiError::bad_request(
                "cannot spawn plain shell via coding-agent launcher",
            ));
        }
        let remote_host_id = host_id
            .as_ref()
            .map(|h| h.as_str().trim())
            .filter(|s| !s.is_empty() && *s != LOCAL_HOST_ID)
            .map(HostId::new);
        if let Some(hid) = remote_host_id {
            let provider = self
                .inner
                .remote_nodes
                .get(&hid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {hid}")))?;
            let (display_name, known_host_projects) = {
                let st = lock(&self.inner.state);
                let display = st
                    .host_display_names
                    .get(&hid)
                    .cloned()
                    .unwrap_or_else(|| provider.default_display_name().to_string());
                let mut projs: Vec<String> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                for s in st.sessions.values() {
                    if s.is_subagent || s.effective_host_id() != hid.as_str() {
                        continue;
                    }
                    for p in [s.git_root.as_deref(), s.cwd.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        let t = p.trim();
                        if !t.is_empty() && seen.insert(t.to_string()) {
                            projs.push(t.to_string());
                        }
                    }
                }
                (display, projs)
            };
            let target_session = match tmux_session
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(s) => {
                    if s.contains([':', '.']) || s.chars().any(char::is_control) {
                        return Err(ApiError::bad_request(format!(
                            "invalid tmux session name {s:?}"
                        )));
                    }
                    s.to_string()
                }
                None => starfane_tmux::DEFAULT_DETACHED_SESSION.to_string(),
            };
            let req = crate::nodes::RemoteSpawnReq {
                agent,
                project_root,
                tmux_session: target_session,
                window_name: name,
                model,
                effort,
                prompt,
                known_host_projects,
            };
            let outcome = provider.spawn_session(&display_name, &req).await?;
            lock(&self.inner.state)
                .sessions
                .insert(outcome.session.session_id.clone(), outcome.session.clone());
            self.mark_dirty();
            self.publish_session(outcome.session);
            self.refresh_attention();
            self.flush_snapshot_async().await;
            return Ok(outcome.response);
        }
        let home_env = self.effective_home_dir();
        let cwd = self.resolve_spawn_cwd_in(home_env.as_deref(), project_root.as_deref())?;

        let target_session = match tmux_session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                if s.contains([':', '.']) || s.chars().any(char::is_control) {
                    return Err(ApiError::bad_request(format!(
                        "invalid tmux session name {s:?}"
                    )));
                }
                s.to_string()
            }
            None => starfane_tmux::DEFAULT_DETACHED_SESSION.to_string(),
        };

        let explicit_name = name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let win_label = match explicit_name {
            Some(w) => {
                if w.chars().any(char::is_control) {
                    return Err(ApiError::bad_request("window name contains control chars"));
                }
                w
            }
            None => {
                let core_list = self.clone();
                let sess_for_scan = target_session.clone();
                let existing = tokio::task::spawn_blocking(move || {
                    core_list
                        .inner
                        .tmux
                        .list_panes()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|p| p.session_name == sess_for_scan)
                        .map(|p| p.window_name)
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                next_sequential_window_name(&existing, agent)
            }
        };

        let bin = PathBuf::from(self.inner.cfg.agent_bin(agent));
        let mut extra = crate::config::TeamConfig::load(Path::new(&cwd))
            .map(|t| t.spawn_args.get(&agent).cloned().unwrap_or_default())
            .unwrap_or_default();
        starfane_adapters::ensure_bypass_permission_arg(agent, &mut extra);
        starfane_adapters::ensure_ancestor_trust_args(
            agent,
            Path::new(&cwd),
            home_env.as_deref(),
            &mut extra,
        );
        starfane_adapters::ensure_codex_mcp_env_vars_arg(agent, home_env.as_deref(), &mut extra);

        let prompt_clean = prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| util::sanitize_line(s, self.inner.cfg.gate.max_pointer_len.max(1000)));

        let effective_model = model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|m| {
                if agent == AgentKind::Agy {
                    starfane_adapters::resolve_agy_model_id(m, effort)
                } else {
                    m.to_string()
                }
            });

        let argv = if let Some(p) = prompt_clean {
            starfane_adapters::build_l4_spawn_cmd(&starfane_adapters::SpawnRequest {
                agent,
                bin,
                cwd: PathBuf::from(&cwd),
                model: effective_model.clone(),
                effort,
                name: Some(win_label.clone()),
                prompt: p,
                extra_args: extra,
            })
            .map_err(|e| ApiError::bad_request(format!("spawn command: {e}")))?
        } else {
            build_bare_interactive_cmd(
                agent,
                &bin,
                &cwd,
                effective_model.as_deref(),
                effort,
                Some(&win_label),
                &extra,
            )
        };

        let spawn_ms = id::now_ms();
        let core = self.clone();
        let cwd_clone = cwd.clone();
        let sess_clone = target_session.clone();
        let win_name = win_label.clone();
        let (pane, tmux_target, tmux_label) = tokio::task::spawn_blocking(move || {
            let pane = core.inner.tmux.spawn_window(&starfane_tmux::SpawnWindow {
                target_session: Some(sess_clone),
                create_session: true,
                window_name: win_name,
                cwd: PathBuf::from(&cwd_clone),
                env: vec![("STARFANE_SPAWNED_AT_MS".into(), spawn_ms.to_string())],
                argv,
            })?;
            let (tmux_target, tmux_label) = core
                .inner
                .tmux
                .list_panes()
                .ok()
                .and_then(|panes| {
                    panes.into_iter().find(|p| p.pane_id == pane).map(|p| {
                        (
                            format!("{}:{}.{}", p.session_name, p.window_index, p.pane_index),
                            Some(p.tmux_label()),
                        )
                    })
                })
                .unwrap_or_else(|| (pane.0.clone(), None));
            Ok::<_, starfane_tmux::TmuxError>((pane, tmux_target, tmux_label))
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("tmux spawn: {e}")))?;

        let git_root = {
            let c = cwd.clone();
            tokio::task::spawn_blocking(move || util::git_root(Path::new(&c)))
                .await
                .unwrap_or(None)
        };
        let sid = legacy_session_id(&pane);
        let now = id::now_ms();
        let initial_effort = infer_session_effort(
            agent,
            effective_model.as_deref(),
            effort.map(|e| e.as_str()),
            None,
            home_env.as_deref(),
        );
        {
            let mut dc = lock(&self.inner.discovery_cache);
            dc.pane_first_seen_ms.insert(pane.clone(), spawn_ms);
            dc.pane_birth_ms.insert(pane.clone(), spawn_ms);
            dc.spawned_panes.insert(
                pane.clone(),
                SpawnedPaneMeta {
                    spawned_at_ms: spawn_ms,
                    model: effective_model.clone(),
                    effort: initial_effort.clone(),
                },
            );
            dc.last_output_watermarks
                .insert(sid.clone(), TranscriptWatermark::default());
        }
        let rec = SessionRecord {
            session_id: sid.clone(),
            agent,
            pane: Some(pane.clone()),
            pane_resolution: PaneResolution::Env,
            cwd: Some(cwd),
            git_root,
            model: effective_model,
            effort: initial_effort,
            state: SessionState::Starting,
            state_since_ms: now,
            last_event_id: None,
            last_event_ms: now,
            last_message: None,
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Heuristic,
            tmux_target: Some(tmux_target.clone()),
            tmux_label: tmux_label.clone(),
            discovered_only: true,
            transcript_path: None,
            metrics: None,
            host_id: None,
            host_display_name: None,
            mux_kind: Some(if pane.as_str().starts_with("zj:") {
                "zellij".to_string()
            } else {
                "tmux".to_string()
            }),
            resume_id: None,
            suspended_at_ms: None,
        };
        let resp_model = rec.model.clone();
        let resp_effort = rec.effort.clone();
        let resp_cwd = rec.cwd.clone();
        lock(&self.inner.state)
            .sessions
            .insert(sid.clone(), rec.clone());
        self.mark_dirty();
        self.publish_session(rec);
        self.refresh_attention();

        Ok(serde_json::json!({
            "session_id": sid,
            "pane_id": pane,
            "host_id": LOCAL_HOST_ID,
            "tmux_target": tmux_target,
            "tmux_label": tmux_label,
            "agent": agent,
            "model": resp_model,
            "effort": resp_effort,
            "cwd": resp_cwd,
        }))
    }

    /// Safely close a single session's terminal pane/window (local tmux/zellij or federated remote
    /// host) without killing sibling split panes or whole multiplexer sessions.
    ///
    /// Immediately marks the session `SessionState::Ended`, clears `last_error` and pending
    /// attention items, broadcasts SSE `event: session`, and persists the state snapshot.
    pub async fn close_session_window(
        &self,
        session_id: &SessionId,
    ) -> Result<serde_json::Value, ApiError> {
        let (pane, tmux_target, host_id) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(session_id)
                .ok_or_else(|| ApiError::not_found(format!("session {session_id}")))?;
            if s.is_subagent {
                return Err(ApiError::bad_request(
                    "cannot close subagent or session without terminal pane",
                ));
            }
            let Some(pane) = s.pane.clone() else {
                return Err(ApiError::bad_request(
                    "cannot close subagent or session without terminal pane",
                ));
            };
            (
                pane,
                s.tmux_target.clone(),
                HostId::new(s.effective_host_id()),
            )
        };

        let mut response_tmux_target = tmux_target;
        if host_id.as_str() != LOCAL_HOST_ID {
            let provider = self
                .inner
                .remote_nodes
                .get(&host_id)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {host_id}")))?;
            let (_, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
            let remote_sid = session_id
                .as_str()
                .strip_prefix(&format!("{}:", host_id.as_str()))
                .map(SessionId::new)
                .unwrap_or_else(|| session_id.clone());
            let remote_res = provider
                .close_pane(&remote_sid, &PaneId::new(local_pane))
                .await?;
            if response_tmux_target.is_none() {
                response_tmux_target = remote_res
                    .get("tmux_target")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
        } else {
            let core = self.clone();
            let pane_to_kill = pane.clone();
            tokio::task::spawn_blocking(move || core.inner.mux.kill_pane(&pane_to_kill))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .map_err(|e| match e {
                    starfane_tmux::TmuxError::InvalidRequest(m) => ApiError::bad_request(m),
                    other => ApiError::new(ErrorCode::Unavailable, format!("close pane: {other}")),
                })?;
        }

        let now_ms = id::now_ms();
        let leg_id = (host_id.as_str() == LOCAL_HOST_ID).then(|| legacy_session_id(&pane));
        let updated_sessions = {
            let mut st = lock(&self.inner.state);
            let mut updated = Vec::new();
            if let Some(rec) = st.sessions.get_mut(session_id) {
                rec.state = SessionState::Ended;
                rec.state_since_ms = now_ms;
                rec.last_event_ms = now_ms;
                rec.last_error = None;
                rec.active_subagents = 0;
                updated.push(rec.clone());
            }
            for child in st.sessions.values_mut() {
                if child.is_subagent
                    && child.parent_session_id.as_ref() == Some(session_id)
                    && child.state != SessionState::Ended
                {
                    child.state = SessionState::Ended;
                    child.state_since_ms = now_ms;
                    child.last_event_ms = now_ms;
                    child.last_error = None;
                    updated.push(child.clone());
                }
            }
            st.pane_watch.remove(&pane);
            st.last_mirrored_tmux.remove(&pane);
            st.pending_away_notices.remove(session_id);
            if let Some(ref leg) = leg_id {
                st.pending_away_notices.remove(leg);
            }
            st.unconfirmed.retain(|a| {
                a.session_id.as_ref() != Some(session_id)
                    && leg_id
                        .as_ref()
                        .is_none_or(|leg| a.session_id.as_ref() != Some(leg))
            });
            updated
        };
        if host_id.as_str() == LOCAL_HOST_ID {
            let mut dc = lock(&self.inner.discovery_cache);
            dc.evict_pane(&pane);
            dc.last_output_watermarks.remove(session_id);
            for s in &updated_sessions {
                dc.last_output_watermarks.remove(&s.session_id);
            }
            dc.last_output_tail_hash.remove(session_id);
        }
        lock(&self.inner.acks).remove(session_id);

        self.mark_dirty();
        for s in updated_sessions {
            self.publish_session(s);
        }
        self.refresh_attention();
        self.flush_snapshot_async().await;
        self.audit(
            "web",
            serde_json::json!({
                "action": "close_session_window",
                "session_id": session_id,
                "pane": pane,
                "tmux_target": response_tmux_target,
                "host_id": host_id,
            }),
        );

        Ok(serde_json::json!({
            "closed": true,
            "session_id": session_id,
            "pane": pane,
            "tmux_target": response_tmux_target,
        }))
    }

    fn resolve_session_resume_id(&self, s: &SessionRecord) -> Option<String> {
        s.resume_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| canonical_external_session_id(s))
            .or_else(|| {
                s.transcript_path
                    .as_deref()
                    .and_then(|p| extract_session_id_from_transcript_path(s.agent, Path::new(p)))
            })
    }

    /// Suspend an active coding-agent session (`claude` / `codex` / `agy`) to reclaim CPU/RAM
    /// while keeping its terminal pane/window and conversation history intact for resume on demand.
    pub async fn suspend_session(
        &self,
        session_id: &SessionId,
    ) -> Result<serde_json::Value, ApiError> {
        let (rec_snapshot, pane, host_id, mut resume_id_opt, bypass_perms, local_bin) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(session_id)
                .ok_or_else(|| ApiError::not_found(format!("session {session_id}")))?;
            if s.is_subagent || s.agent.is_shell() {
                return Err(ApiError::bad_request(
                    "only root claude/codex/agy sessions can be suspended",
                ));
            }
            if s.state == SessionState::Ended {
                return Err(ApiError::bad_request("cannot suspend an ended session"));
            }
            if s.state == SessionState::Suspended {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    "session is already suspended",
                ));
            }
            let Some(pane) = s.pane.clone() else {
                return Err(ApiError::bad_request(
                    "session has no live terminal pane to suspend",
                ));
            };
            let host_id = HostId::new(s.effective_host_id());
            let rid = self.resolve_session_resume_id(s);
            let bypass = self.inner.cfg.tmux.backup.restore_bypass_permissions;
            let bin = (host_id.as_str() == LOCAL_HOST_ID)
                .then(|| PathBuf::from(self.inner.cfg.agent_bin(s.agent)));
            (s.clone(), pane, host_id, rid, bypass, bin)
        };

        if resume_id_opt.is_none()
            && host_id.as_str() == LOCAL_HOST_ID
            && rec_snapshot.agent == AgentKind::Claude
        {
            let core = self.clone();
            let target_pane = pane.clone();
            let home_dir = self.effective_home_dir();
            resume_id_opt = tokio::task::spawn_blocking(move || {
                let panes = core.inner.mux.list_all_panes().ok()?;
                let p = panes.into_iter().find(|p| p.pane_id == target_pane)?;
                if p.pane_pid == 0 {
                    return None;
                }
                let home = home_dir?;
                let birth = lock(&core.inner.discovery_cache).pid_birth_ms.clone();
                let discovered = core.inner.mux.discover_panes_from(
                    std::slice::from_ref(&p),
                    starfane_tmux::DiscoverOptions::default(),
                );
                let pids: Vec<u32> = discovered
                    .first()
                    .map(|d| d.subtree_pids.clone())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| vec![p.pane_pid]);
                resolve_claude_transcript_by_pids_in(&home, &pids, &birth, &HashSet::new())
                    .map(|(cid, _)| cid)
            })
            .await
            .ok()
            .flatten();
        }

        let Some(resume_id) = resume_id_opt else {
            return Err(ApiError::bad_request(
                "cannot suspend session: no resumable conversation ID resolved yet",
            ));
        };

        let resume_argv = starfane_adapters::build_resume_cmd(&starfane_adapters::ResumeRequest {
            agent: rec_snapshot.agent,
            bin: local_bin,
            resume_id: resume_id.clone(),
            cwd: rec_snapshot.cwd.clone(),
            model: rec_snapshot.model.clone(),
            effort: rec_snapshot.effort.clone(),
            bypass_permissions: bypass_perms,
        })
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

        let waiter_script = starfane_adapters::build_tmux_suspend_waiter_script(
            rec_snapshot.agent,
            &resume_id,
            &resume_argv,
        );

        let new_pane = if host_id.as_str() != LOCAL_HOST_ID {
            let provider = self
                .inner
                .remote_nodes
                .get(&host_id)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {host_id}")))?;
            let (_, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
            let remote_sid = session_id
                .as_str()
                .strip_prefix(&format!("{}:", host_id.as_str()))
                .map(SessionId::new)
                .unwrap_or_else(|| session_id.clone());
            let new_local = provider
                .suspend_pane(
                    &remote_sid,
                    &PaneId::new(local_pane),
                    rec_snapshot.cwd.as_deref(),
                    &waiter_script,
                )
                .await?;
            starfane_tmux::qualify_host_pane(&host_id, &new_local)
        } else {
            let core = self.clone();
            let req = starfane_tmux::SuspendPaneReq {
                pane: pane.clone(),
                cwd: rec_snapshot.cwd.clone(),
                pane_name: rec_snapshot
                    .tmux_label
                    .clone()
                    .or_else(|| rec_snapshot.tmux_target.clone()),
                waiter_script,
                resume_argv,
            };
            tokio::task::spawn_blocking(move || core.inner.mux.suspend_pane(&req))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .map_err(|e| match e {
                    starfane_tmux::TmuxError::InvalidRequest(m) => ApiError::bad_request(m),
                    other => {
                        ApiError::new(ErrorCode::Unavailable, format!("suspend pane: {other}"))
                    }
                })?
        };

        let now_ms = id::now_ms();
        let mut updated_main: Option<SessionRecord> = None;
        let updated_sessions = {
            let mut st = lock(&self.inner.state);
            let mut updated = Vec::new();
            if let Some(rec) = st.sessions.get_mut(session_id) {
                rec.state = SessionState::Suspended;
                rec.resume_id = Some(resume_id.clone());
                rec.suspended_at_ms = Some(now_ms);
                if rec.pane.as_ref() != Some(&new_pane) {
                    if new_pane.as_str().starts_with("zj:") {
                        rec.tmux_target = Some(new_pane.0.clone());
                    }
                    rec.pane = Some(new_pane.clone());
                }
                rec.state_since_ms = now_ms;
                rec.last_event_ms = now_ms;
                rec.last_error = None;
                rec.active_subagents = 0;
                updated_main = Some(rec.clone());
                updated.push(rec.clone());
            }
            for child in st.sessions.values_mut() {
                if child.is_subagent
                    && child.parent_session_id.as_ref() == Some(session_id)
                    && child.state != SessionState::Ended
                {
                    child.state = SessionState::Ended;
                    child.state_since_ms = now_ms;
                    child.last_event_ms = now_ms;
                    child.last_error = None;
                    updated.push(child.clone());
                }
            }
            st.pane_watch.remove(&pane);
            st.pane_watch.remove(&new_pane);
            st.last_mirrored_tmux.remove(&pane);
            st.last_mirrored_tmux.remove(&new_pane);
            st.pending_away_notices.remove(session_id);
            st.unconfirmed
                .retain(|a| a.session_id.as_ref() != Some(session_id));
            updated
        };
        if host_id.as_str() == LOCAL_HOST_ID {
            let mut dc = lock(&self.inner.discovery_cache);
            dc.evict_pane(&pane);
            dc.evict_pane(&new_pane);
        }

        self.mark_dirty();
        for s in updated_sessions {
            self.publish_session(s);
        }
        self.refresh_attention();
        self.flush_snapshot_async().await;
        self.audit(
            "web",
            serde_json::json!({
                "action": "suspend_session",
                "session_id": session_id,
                "pane": new_pane,
                "resume_id": resume_id,
                "host_id": host_id,
            }),
        );

        Ok(serde_json::json!({
            "suspended": true,
            "session_id": session_id,
            "pane": new_pane,
            "resume_id": resume_id,
            "session": updated_main,
        }))
    }

    /// Wake a previously suspended session back into its live CLI conversation.
    pub async fn resume_session(
        &self,
        session_id: &SessionId,
    ) -> Result<serde_json::Value, ApiError> {
        let (rec_snapshot, pane, host_id, resume_id, bypass_perms, local_bin) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(session_id)
                .ok_or_else(|| ApiError::not_found(format!("session {session_id}")))?;
            if s.is_subagent || s.agent.is_shell() {
                return Err(ApiError::bad_request(
                    "only root coding-agent sessions can be resumed",
                ));
            }
            if s.state != SessionState::Suspended {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    "session is not suspended",
                ));
            }
            let Some(pane) = s.pane.clone() else {
                return Err(ApiError::bad_request(
                    "suspended session has no terminal pane",
                ));
            };
            let Some(rid) = self.resolve_session_resume_id(s) else {
                return Err(ApiError::bad_request(
                    "suspended session is missing resume_id",
                ));
            };
            let host_id = HostId::new(s.effective_host_id());
            let bypass = self.inner.cfg.tmux.backup.restore_bypass_permissions;
            let bin = (host_id.as_str() == LOCAL_HOST_ID)
                .then(|| PathBuf::from(self.inner.cfg.agent_bin(s.agent)));
            (s.clone(), pane, host_id, rid, bypass, bin)
        };

        let resume_argv = starfane_adapters::build_resume_cmd(&starfane_adapters::ResumeRequest {
            agent: rec_snapshot.agent,
            bin: local_bin,
            resume_id,
            cwd: rec_snapshot.cwd.clone(),
            model: rec_snapshot.model.clone(),
            effort: rec_snapshot.effort.clone(),
            bypass_permissions: bypass_perms,
        })
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

        let new_pane = if host_id.as_str() != LOCAL_HOST_ID {
            let provider = self
                .inner
                .remote_nodes
                .get(&host_id)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {host_id}")))?;
            let (_, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
            let remote_sid = session_id
                .as_str()
                .strip_prefix(&format!("{}:", host_id.as_str()))
                .map(SessionId::new)
                .unwrap_or_else(|| session_id.clone());
            let new_local = provider
                .wake_suspended_pane(&remote_sid, &PaneId::new(local_pane))
                .await?;
            starfane_tmux::qualify_host_pane(&host_id, &new_local)
        } else {
            let core = self.clone();
            let req = starfane_tmux::WakePaneReq {
                pane: pane.clone(),
                cwd: rec_snapshot.cwd.clone(),
                pane_name: rec_snapshot
                    .tmux_label
                    .clone()
                    .or_else(|| rec_snapshot.tmux_target.clone()),
                resume_argv,
            };
            tokio::task::spawn_blocking(move || core.inner.mux.wake_suspended_pane(&req))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .map_err(|e| match e {
                    starfane_tmux::TmuxError::InvalidRequest(m) => ApiError::bad_request(m),
                    other => ApiError::new(ErrorCode::Unavailable, format!("resume pane: {other}")),
                })?
        };

        let now_ms = id::now_ms();
        let updated_rec = {
            let mut st = lock(&self.inner.state);
            let rec = st
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| ApiError::not_found(format!("session {session_id}")))?;
            rec.state = SessionState::Starting;
            rec.suspended_at_ms = None;
            rec.pane = Some(new_pane.clone());
            rec.state_since_ms = now_ms;
            rec.last_event_ms = now_ms;
            rec.last_error = None;
            let snap = rec.clone();
            st.pane_watch.remove(&pane);
            st.pane_watch.remove(&new_pane);
            snap
        };
        if host_id.as_str() == LOCAL_HOST_ID {
            let mut dc = lock(&self.inner.discovery_cache);
            dc.evict_pane(&pane);
            dc.evict_pane(&new_pane);
            dc.pane_first_seen_ms.insert(new_pane.clone(), now_ms);
            dc.pane_birth_ms.insert(new_pane.clone(), now_ms);
            dc.spawned_panes.insert(
                new_pane.clone(),
                SpawnedPaneMeta {
                    spawned_at_ms: now_ms,
                    model: updated_rec.model.clone(),
                    effort: updated_rec.effort.clone(),
                },
            );
        }

        self.mark_dirty();
        self.publish_session(updated_rec.clone());
        self.refresh_attention();
        self.flush_snapshot_async().await;
        self.audit(
            "web",
            serde_json::json!({
                "action": "resume_session",
                "session_id": session_id,
                "pane": new_pane,
                "host_id": host_id,
            }),
        );

        Ok(serde_json::json!({
            "resumed": true,
            "session_id": session_id,
            "pane": new_pane,
            "session": updated_rec,
        }))
    }
}

fn build_bare_interactive_cmd(
    agent: AgentKind,
    bin: &Path,
    cwd: &str,
    model: Option<&str>,
    effort: Option<Effort>,
    name: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut effective_extra = extra.to_vec();
    starfane_adapters::ensure_bypass_permission_arg(agent, &mut effective_extra);
    let mut argv = vec![bin.to_string_lossy().into_owned()];
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    match agent {
        AgentKind::Claude => {
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(m.into());
            }
            if let Some(e) = effort {
                argv.push("--effort".into());
                argv.push(e.as_str().into());
            }
            if let Some(n) = name {
                argv.push("-n".into());
                argv.push(n.into());
            }
            argv.extend(effective_extra);
        }
        AgentKind::Codex => {
            if let Some(m) = model {
                argv.push("-m".into());
                argv.push(m.into());
            }
            if let Some(e) = effort {
                let mapped = match e {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh | Effort::Max => "xhigh",
                };
                argv.push("-c".into());
                argv.push(format!("model_reasoning_effort=\"{mapped}\""));
            }
            argv.push("-C".into());
            argv.push(cwd.into());
            argv.extend(effective_extra);
        }
        AgentKind::Agy => {
            let fixed = model.is_some_and(starfane_adapters::is_fixed_effort_agy_model);
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(starfane_adapters::resolve_agy_model_id(m, effort));
            }
            if let Some(e) = effort {
                if !fixed {
                    argv.push("--effort".into());
                    argv.push(e.as_str().into());
                }
            }
            argv.extend(effective_extra);
        }
        AgentKind::Shell => {}
    }
    argv
}

/// Helper map of pane_id -> tmux_target from live non-Ended sessions (for hook ingest enrichment).
pub(crate) fn known_tmux_target(st: &State, pane: &PaneId) -> Option<String> {
    st.sessions
        .values()
        .find(|s| {
            s.state != SessionState::Ended
                && s.pane.as_ref() == Some(pane)
                && s.tmux_target.is_some()
        })
        .and_then(|s| s.tmux_target.clone())
}

/// Helper map of pane_id -> tmux_label (`session-window`) from live non-Ended sessions.
pub(crate) fn known_tmux_label(st: &State, pane: &PaneId) -> Option<String> {
    st.sessions
        .values()
        .find(|s| {
            s.state != SessionState::Ended
                && s.pane.as_ref() == Some(pane)
                && s.tmux_label.is_some()
        })
        .and_then(|s| s.tmux_label.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_dp(pane: &str, target: &str, agent: AgentKind, model: Option<&str>) -> DiscoveredPane {
        let sess = target.split(':').next().unwrap_or("work");
        DiscoveredPane {
            pane_id: PaneId::new(pane),
            tmux_target: target.into(),
            tmux_label: format!("{sess}-main"),
            agent,
            cwd: "/tmp/repo".into(),
            git_root: Some("/tmp/repo".into()),
            model: model.map(str::to_string),
            effort: None,
            agent_pid: None,
            subtree_pids: Vec::new(),
            codex_resume_id: None,
            pane_in_mode: false,
            pane_dead: false,
            pane_current_command: agent.as_str().into(),
            history_size: 0,
            cursor_x: 0,
            cursor_y: 0,
        }
    }

    #[test]
    fn reconcile_upserts_merges_and_cleans_stale_legacy_panes() {
        let mut st = State::default();
        let live: HashSet<PaneId> = [PaneId::new("%9"), PaneId::new("%24")]
            .into_iter()
            .collect();
        let discovered = vec![
            mk_dp("%9", "learning:2.1", AgentKind::Claude, Some("opus")),
            mk_dp("%24", "teleop:1.0", AgentKind::Codex, Some("gpt-6.2-sol")),
        ];

        let d1 = reconcile_discovered_panes(&mut st, &discovered, &live, 1_000);
        assert!(d1.changed);
        assert_eq!(st.sessions.len(), 2);
        let s9 = st.sessions.get(&SessionId::new("tmux-%9")).unwrap();
        assert!(s9.discovered_only);
        assert_eq!(s9.tmux_target.as_deref(), Some("learning:2.1"));
        assert_eq!(s9.model.as_deref(), Some("opus"));

        // Queue an inbox item on legacy tmux-%9, then simulate a real hook session arriving on %9.
        st.push_inbox(
            &SessionId::new("tmux-%9"),
            InboxItem {
                id: "M-1".into(),
                kind: InboxKind::Message,
                from: None,
                created_ms: 1_500,
                title: "hi".into(),
                body_md: "hello".into(),
                stage: None,
                artifact: None,
                handoff_id: None,
                review_id: None,
            },
        );
        st.sessions.insert(
            SessionId::new("real-c9"),
            SessionRecord {
                session_id: SessionId::new("real-c9"),
                agent: AgentKind::Claude,
                pane: Some(PaneId::new("%9")),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp/repo".into()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 2_000,
                last_event_id: None,
                last_event_ms: 2_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: None,
                tmux_label: None,
                discovered_only: false,
                transcript_path: None,
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: None,
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        let d2 = reconcile_discovered_panes(&mut st, &discovered, &live, 2_500);
        assert!(d2.changed);
        assert!(!st.sessions.contains_key(&SessionId::new("tmux-%9")));
        let real = st.sessions.get(&SessionId::new("real-c9")).unwrap();
        assert!(!real.discovered_only);
        assert_eq!(real.tmux_target.as_deref(), Some("learning:2.1"));
        assert_eq!(real.model.as_deref(), Some("opus"));
        assert_eq!(real.unread_inbox, 1);

        // Close %24 -> stale legacy session is removed and emitted as Ended.
        let live_after: HashSet<PaneId> = [PaneId::new("%9")].into_iter().collect();
        let d3 = reconcile_discovered_panes(&mut st, &discovered[..1], &live_after, 3_000);
        assert!(d3.changed);
        assert!(!st.sessions.contains_key(&SessionId::new("tmux-%24")));
        assert!(d3
            .updated
            .iter()
            .any(|s| s.session_id.as_str() == "tmux-%24" && s.state == SessionState::Ended));
    }

    #[test]
    fn reconcile_preserves_newly_spawned_starting_pane_during_grace_window() {
        let mut st = State::default();
        let pane = PaneId::new("%55");
        let sid = legacy_session_id(&pane);
        st.sessions.insert(
            sid.clone(),
            SessionRecord {
                session_id: sid.clone(),
                agent: AgentKind::Claude,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp".into()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Starting,
                state_since_ms: 10_000,
                last_event_id: None,
                last_event_ms: 10_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: Some("work:2.0".into()),
                tmux_label: Some("work-main".into()),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: None,
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
        // Within 15s grace window while agent binary is still launching -> kept.
        let d1 = reconcile_discovered_panes(&mut st, &[], &live, 15_000);
        assert!(!d1.changed);
        assert!(st.sessions.contains_key(&sid));

        // After 15s grace window with still no agent process -> pruned.
        let d2 = reconcile_discovered_panes(&mut st, &[], &live, 30_000);
        assert!(d2.changed);
        assert!(!st.sessions.contains_key(&sid));
    }

    #[test]
    fn heuristic_tail_hash_detects_working_and_turn_complete_after_3_stable_ticks() {
        let mut st = State::default();
        let pane = PaneId::new("%24");
        let sid = legacy_session_id(&pane);
        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
        reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%24", "teleop:1.0", AgentKind::Codex, None)],
            &live,
            1_000,
        );

        // Poll 0: baseline hash recorded, remains Idle.
        assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, "prompt>", 2_000).is_none());
        assert_eq!(st.sessions[&sid].state, SessionState::Idle);

        // Poll 1: output changes -> Working.
        let (s_work, ev_work) =
            apply_legacy_tail_hash(&mut st, &sid, &pane, "thinking...\nstep 1", 5_000).unwrap();
        assert_eq!(s_work.state, SessionState::Working);
        assert!(ev_work.is_none());

        // Poll 2 & 3: unchanged twice -> still Working.
        assert!(
            apply_legacy_tail_hash(&mut st, &sid, &pane, "thinking...\nstep 1", 8_000).is_none()
        );
        assert!(
            apply_legacy_tail_hash(&mut st, &sid, &pane, "thinking...\nstep 1", 11_000).is_none()
        );
        assert_eq!(st.sessions[&sid].state, SessionState::Working);

        // Poll 4 (3rd unchanged tick): transitions to Idle + emits Heuristic TurnComplete.
        let (s_idle, ev_done) =
            apply_legacy_tail_hash(&mut st, &sid, &pane, "thinking...\nstep 1", 14_000).unwrap();
        assert_eq!(s_idle.state, SessionState::Idle);
        let ev = ev_done.expect("expected heuristic TurnComplete");
        assert_eq!(ev.kind, EventKind::TurnComplete);
        assert_eq!(ev.confidence, Confidence::Heuristic);
        assert_eq!(ev.session_id, sid);

        // Poll 5: further unchanged ticks do NOT re-emit TurnComplete.
        assert!(
            apply_legacy_tail_hash(&mut st, &sid, &pane, "thinking...\nstep 1", 17_000).is_none()
        );
    }

    #[test]
    fn bare_interactive_cmd_maps_effort_for_claude_codex_and_agy() {
        let cl = build_bare_interactive_cmd(
            AgentKind::Claude,
            Path::new("claude"),
            "/repo",
            Some("fable"),
            Some(Effort::Max),
            Some("demo"),
            &[],
        );
        assert_eq!(
            cl,
            vec![
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "-n",
                "demo",
                "--dangerously-skip-permissions",
            ]
        );

        let cx = build_bare_interactive_cmd(
            AgentKind::Codex,
            Path::new("codex"),
            "/repo",
            Some("gpt-6.2-sol"),
            Some(Effort::Xhigh),
            None,
            &[],
        );
        assert_eq!(
            cx,
            vec![
                "codex",
                "-m",
                "gpt-6.2-sol",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-C",
                "/repo",
                "--dangerously-bypass-approvals-and-sandbox",
            ]
        );

        let ag = build_bare_interactive_cmd(
            AgentKind::Agy,
            Path::new("agy"),
            "/repo",
            Some("gemini-4-neon"),
            Some(Effort::High),
            None,
            &[],
        );
        assert_eq!(
            ag,
            vec![
                "agy",
                "--model",
                "gemini-4-neon-high",
                "--effort",
                "high",
                "--dangerously-skip-permissions",
            ]
        );

        let orch = build_bare_interactive_cmd(
            AgentKind::Agy,
            Path::new("agy"),
            "/repo",
            Some("gemini-pro-orchestrator-neon"),
            Some(Effort::High),
            None,
            &[],
        );
        assert_eq!(
            orch,
            vec![
                "agy",
                "--model",
                "gemini-pro-orchestrator-neon",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn background_tail_capture_skips_copy_mode_and_resolved_sessions() {
        // Active copy-mode panes must never be background-captured regardless of legacy/agent.
        assert!(!should_capture_tail_for_scan(
            true,
            true,
            AgentKind::Codex,
            true
        ));
        assert!(!should_capture_tail_for_scan(
            true,
            false,
            AgentKind::Claude,
            true
        ));
        // Legacy pane not in copy-mode still needs tail hashing for heuristic completion.
        assert!(should_capture_tail_for_scan(
            false,
            true,
            AgentKind::Agy,
            false
        ));
        // Hook-backed Claude/Codex skips capture once transcript+metrics are resolved.
        assert!(!should_capture_tail_for_scan(
            false,
            false,
            AgentKind::Codex,
            false
        ));
        assert!(!should_capture_tail_for_scan(
            false,
            false,
            AgentKind::Claude,
            false
        ));
        // Unresolved Claude/Codex pane still gets tail capture for initial title/status fallback.
        assert!(should_capture_tail_for_scan(
            false,
            false,
            AgentKind::Codex,
            true
        ));
    }

    #[test]
    fn spawn_cwd_defaults_to_agent_session_and_honors_override() {
        let home = std::env::temp_dir().join(format!(
            "starfane-spawn-home-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&home).unwrap();
        let expected_default = home.join(DEFAULT_AGENT_SESSION_DIR);
        assert!(!expected_default.exists());

        let fail_lookup = |needle: &str| -> Result<String, ApiError> {
            Err(ApiError::not_found(format!("unexpected lookup: {needle}")))
        };

        // 1. Blank or None creates & returns `<home>/.agent_session`.
        let r_none = resolve_spawn_cwd_with(Some(&home), None, fail_lookup).unwrap();
        assert_eq!(PathBuf::from(&r_none), expected_default);
        assert!(expected_default.is_dir());

        let r_blank = resolve_spawn_cwd_with(Some(&home), Some("   "), fail_lookup).unwrap();
        assert_eq!(PathBuf::from(&r_blank), expected_default);

        // 2. Explicit `~/.agent_session` resolves cleanly even if removed before call.
        std::fs::remove_dir_all(&expected_default).unwrap();
        let r_tilde_default =
            resolve_spawn_cwd_with(Some(&home), Some("~/.agent_session"), fail_lookup).unwrap();
        assert_eq!(PathBuf::from(&r_tilde_default), expected_default);
        assert!(expected_default.is_dir());

        // 3. Explicit custom directory (tilde-expanded or absolute) overrides default.
        let custom = home.join("custom_proj");
        std::fs::create_dir_all(&custom).unwrap();
        let r_tilde_custom =
            resolve_spawn_cwd_with(Some(&home), Some("~/custom_proj"), fail_lookup).unwrap();
        assert_eq!(PathBuf::from(&r_tilde_custom), custom);

        let custom_str = custom.to_string_lossy().into_owned();
        let r_abs = resolve_spawn_cwd_with(Some(&home), Some(&custom_str), fail_lookup).unwrap();
        assert_eq!(r_abs, custom_str);

        // 4. Non-existent explicit path returns BadRequest rather than falling back.
        let missing = resolve_spawn_cwd_with(Some(&home), Some("~/missing_dir"), fail_lookup);
        assert_eq!(missing.unwrap_err().code, ErrorCode::BadRequest);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn revives_hot_reloaded_codex_session_when_authoritative_rollout_matches() {
        let mut st = State::default();
        let pane = PaneId::new("%24");
        let uuid = "01a1572d-8227-7972-873d-d0848f641c83";
        let real_sid = SessionId::new(uuid);
        let leg_sid = legacy_session_id(&pane);
        let rollout_path =
            format!("/tmp/.codex/sessions/2026/11/24/rollout-2026-11-24T09-00-00-{uuid}.jsonl");

        st.sessions.insert(
            real_sid.clone(),
            SessionRecord {
                session_id: real_sid.clone(),
                agent: AgentKind::Codex,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp/repo".into()),
                git_root: Some("/tmp/repo".into()),
                model: Some("gpt-6.1-sol".into()),
                effort: Some("high".into()),
                state: SessionState::Ended,
                state_since_ms: 900,
                last_event_id: None,
                last_event_ms: 900,
                last_message: None,
                last_error: Some("stale".into()),
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("teleop:1.0".into()),
                tmux_label: Some("teleop-qs".into()),
                discovered_only: false,
                transcript_path: Some(rollout_path.clone()),
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: None,
                resume_id: None,
                suspended_at_ms: None,
            },
        );
        st.feishu_session_threads
            .insert(leg_sid.clone(), "om_legacy_root".into());
        st.sessions.insert(
            leg_sid.clone(),
            SessionRecord {
                session_id: leg_sid.clone(),
                agent: AgentKind::Codex,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp/repo".into()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 1_000,
                last_event_id: None,
                last_event_ms: 1_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: Some("teleop:1.0".into()),
                tmux_label: Some("teleop-qs".into()),
                discovered_only: true,
                transcript_path: Some(rollout_path.clone()),
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: None,
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        // Without authoritative match, Ended session is NOT revived.
        assert_eq!(
            find_matching_or_revivable_real_session(
                &st,
                &pane,
                Some(AgentKind::Codex),
                &rollout_path,
                false,
            ),
            None
        );

        // With authoritative rollout match, Ended real session is selected for revival and merges legacy state.
        assert_eq!(
            find_matching_or_revivable_real_session(
                &st,
                &pane,
                Some(AgentKind::Codex),
                &rollout_path,
                true,
            ),
            Some((real_sid.clone(), true))
        );
        assert!(st.merge_legacy_into_real(&leg_sid, &real_sid));
        assert!(!st.sessions.contains_key(&leg_sid));
        assert_eq!(
            st.feishu_session_threads.get(&real_sid).map(String::as_str),
            Some("om_legacy_root")
        );
    }

    #[test]
    fn hash_tail_ignores_volatile_codex_and_claude_footer_hints() {
        let a = "codex output\n› Ask Codex to do anything\n  ← for agents · ? for shortcuts  ⚠ 3 warnings";
        let b = "codex output\n› Ask Codex to do anything\n  ← for agents · ? for shortcuts  ⚠ 4 warnings";
        assert_eq!(hash_tail(a), hash_tail(b));
        let c = "codex output changed\n› Ask Codex to do anything";
        assert_ne!(hash_tail(a), hash_tail(c));
    }

    #[test]
    fn sequential_window_naming_increments_per_agent() {
        assert_eq!(
            next_sequential_window_name(&[], AgentKind::Claude),
            "claude-1"
        );
        let existing = vec![
            "claude-1".into(),
            "claude-3".into(),
            "agy-1".into(),
            "claude-custom".into(),
        ];
        assert_eq!(
            next_sequential_window_name(&existing, AgentKind::Claude),
            "claude-4"
        );
        assert_eq!(
            next_sequential_window_name(&existing, AgentKind::Agy),
            "agy-2"
        );
        assert_eq!(
            next_sequential_window_name(&existing, AgentKind::Codex),
            "codex-1"
        );
    }

    #[test]
    fn shell_panes_track_current_command_and_never_emit_turn_complete() {
        let mut st = State::default();
        let pane = PaneId::new("%22");
        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();

        // Discover plain shell pane (spot:0.0 running zsh).
        let d1 = reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%22", "spot:0.0", AgentKind::Shell, Some("zsh"))],
            &live,
            1_000,
        );
        assert!(d1.changed);
        let sid = legacy_session_id(&pane);
        assert_eq!(st.sessions[&sid].agent, AgentKind::Shell);
        assert_eq!(st.sessions[&sid].model.as_deref(), Some("zsh"));

        // Command change in shell updates model live.
        let d2 = reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%22", "spot:0.0", AgentKind::Shell, Some("python3"))],
            &live,
            2_000,
        );
        assert!(d2.changed);
        assert_eq!(st.sessions[&sid].model.as_deref(), Some("python3"));

        // Tail activity updates Working/Idle state without synthesizing TurnComplete events.
        assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, "initial\n", 3_000).is_none());
        let tr_work = apply_legacy_tail_hash(&mut st, &sid, &pane, "running step 1\n", 4_000);
        assert!(tr_work.is_some());
        assert_eq!(st.sessions[&sid].state, SessionState::Working);
        assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, "running step 1\n", 5_000).is_none());
        assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, "running step 1\n", 6_000).is_none());
        let (rec_idle, ev_opt) =
            apply_legacy_tail_hash(&mut st, &sid, &pane, "running step 1\n", 7_000).unwrap();
        assert_eq!(rec_idle.state, SessionState::Idle);
        assert!(ev_opt.is_none());

        // Upgrading pane from Shell to Codex cleanly replaces model/agent on the same pane record.
        let d3 = reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%22", "spot:0.0", AgentKind::Codex, Some("gpt-5"))],
            &live,
            8_000,
        );
        assert!(d3.changed);
        assert_eq!(st.sessions[&sid].agent, AgentKind::Codex);
        assert_eq!(st.sessions[&sid].model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn shell_discovery_does_not_duplicate_live_real_agent_on_same_pane_or_touch_remote_sessions() {
        let mut st = State::default();
        let pane = PaneId::new("%77");
        let real_sid = SessionId::new("real-claude-77");
        st.sessions.insert(
            real_sid.clone(),
            SessionRecord {
                session_id: real_sid.clone(),
                agent: AgentKind::Claude,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp".into()),
                git_root: None,
                model: Some("opus".into()),
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 1000,
                last_event_id: None,
                last_event_ms: 1000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("dev:0.0".into()),
                tmux_label: Some("dev-claude".into()),
                discovered_only: false,
                transcript_path: None,
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: Some("tmux".into()),
                resume_id: None,
                suspended_at_ms: None,
            },
        );
        // Also seed a remote peer session with `pane: None` and `tmux_label: Some(...)`.
        let remote_sid = SessionId::new("mac-xiling:s1");
        st.sessions.insert(
            remote_sid.clone(),
            SessionRecord {
                session_id: remote_sid.clone(),
                agent: AgentKind::Claude,
                pane: None,
                pane_resolution: PaneResolution::Env,
                cwd: Some("/Users/xiling/app".into()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 1000,
                last_event_id: None,
                last_event_ms: 1000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("work:0.0".into()),
                tmux_label: Some("work-app".into()),
                discovered_only: false,
                transcript_path: None,
                metrics: None,
                host_id: Some(HostId::new("mac-xiling")),
                host_display_name: Some("Mac Xiling".into()),
                mux_kind: Some("tmux".into()),
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
        // Shell discovery sees `python3` in `%77` -> must NOT create a duplicate `tmux-%77` Shell session!
        reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%77", "dev:0.0", AgentKind::Shell, Some("python3"))],
            &live,
            2_000,
        );
        assert!(
            !st.sessions.contains_key(&legacy_session_id(&pane)),
            "must not duplicate live real agent on %77 with a discovered_only Shell placeholder"
        );
        assert!(st.sessions.contains_key(&real_sid));
        // Remote session's tmux metadata must remain completely untouched by local discovery.
        assert_eq!(
            st.sessions[&remote_sid].tmux_label.as_deref(),
            Some("work-app")
        );
    }

    #[test]
    fn legacy_tail_hash_marks_codex_claude_and_agy_folder_trust_screens_as_needs_permission() {
        let cases = [
            (
                "%31",
                AgentKind::Codex,
                "  Folder access\n  /Users/wujie/Work/repo\n\n  Trust this folder? Codex can read, edit, and run files here.\n\n› 1. Trust and continue\n  2. Back to Agent Command Center\n  enter continue · esc back\n",
                "codex waiting for folder trust approval",
                "› Ask Codex to do anything\n  GPT-6.2-Sol xhigh · ~/Work/repo\n  ← for agents · ? for shortcuts\n",
            ),
            (
                "%32",
                AgentKind::Claude,
                " Accessing workspace:\n /Users/wujie/Work/repo\n Quick safety check: Is this a project you created or one you trust?\n Claude Code'll be able to read, edit, and execute files here.\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel\n",
                "claude waiting for folder trust approval",
                "❯ \n? for shortcuts\n",
            ),
            (
                "%33",
                AgentKind::Agy,
                "Accessing workspace:\n/Users/wujie/Work/repo\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\n  No, exit\n  ↑/↓ Navigate · enter Confirm\n",
                "agy waiting for folder trust approval",
                "> Auto mode: edits & safe commands auto-approved\n? for shortcuts    auto · Gemini 4 Neon · xhigh\n",
            ),
        ];

        for (pane_str, agent, trust_tail, expected_reason, normal_tail) in cases {
            let mut st = State::default();
            let pane = PaneId::new(pane_str);
            let sid = legacy_session_id(&pane);
            let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
            reconcile_discovered_panes(
                &mut st,
                &[mk_dp(pane_str, "agent-session:0.0", agent, None)],
                &live,
                1_000,
            );
            assert_eq!(st.sessions[&sid].state, SessionState::Idle);

            // Trust prompt immediately flips Idle -> NeedsPermission + emits heuristic event.
            let (rec, ev_opt) = apply_legacy_tail_hash(&mut st, &sid, &pane, trust_tail, 2_000)
                .expect("expected NeedsPermission transition");
            assert_eq!(rec.state, SessionState::NeedsPermission, "{agent}");
            assert_eq!(rec.last_error.as_deref(), Some(expected_reason), "{agent}");
            let ev = ev_opt.expect("expected NeedsPermission event");
            assert_eq!(ev.kind, EventKind::NeedsPermission);
            assert_eq!(ev.native_event, "tmux_trust_prompt_heuristic");
            assert_eq!(ev.detail.error.as_deref(), Some(expected_reason));

            // Subsequent identical polls while waiting on trust do NOT re-emit duplicate events.
            assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, trust_tail, 5_000).is_none());
            assert_eq!(st.sessions[&sid].state, SessionState::NeedsPermission);

            // Once user approves trust manually and normal TUI appears, last_error clears and state recovers.
            let (rec_work, ev_work) =
                apply_legacy_tail_hash(&mut st, &sid, &pane, normal_tail, 8_000)
                    .expect("expected recovery transition");
            assert_eq!(rec_work.state, SessionState::Working, "{agent}");
            assert_eq!(rec_work.last_error, None, "{agent}");
            assert!(ev_work.is_none());

            // After LEGACY_STABLE_TICKS unchanged normal ticks -> Idle cleanly.
            assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, normal_tail, 11_000).is_none());
            assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, normal_tail, 14_000).is_none());
            let (rec_idle, _) =
                apply_legacy_tail_hash(&mut st, &sid, &pane, normal_tail, 17_000).unwrap();
            assert_eq!(rec_idle.state, SessionState::Idle, "{agent}");
            assert_eq!(rec_idle.last_error, None, "{agent}");
        }
    }

    #[test]
    fn legacy_tail_hash_marks_codex_update_modal_as_needs_input_and_recovers() {
        let mut st = State::default();
        let pane = PaneId::new("%34");
        let sid = legacy_session_id(&pane);
        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
        reconcile_discovered_panes(
            &mut st,
            &[mk_dp("%34", "agent-session:0.0", AgentKind::Codex, None)],
            &live,
            1_000,
        );

        let update_tail = "  Update available · 0.167.2 → 0.172.0\n  Release notes: https://github.com/openai/codex/releases/latest\n\n› 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  3. Skip until next version\n\n  enter continue · esc skip\n";
        let (rec, ev_opt) = apply_legacy_tail_hash(&mut st, &sid, &pane, update_tail, 2_000)
            .expect("expected NeedsInput transition");
        assert_eq!(rec.state, SessionState::NeedsInput);
        assert_eq!(
            rec.last_error.as_deref(),
            Some("codex waiting on update prompt")
        );
        let ev = ev_opt.expect("expected NeedsInput event");
        assert_eq!(ev.kind, EventKind::NeedsInput);
        assert_eq!(ev.native_event, "tmux_startup_modal_heuristic");

        // No duplicate event while still on the update modal.
        assert!(apply_legacy_tail_hash(&mut st, &sid, &pane, update_tail, 5_000).is_none());

        // Dismissed -> clears last_error and settles to Idle.
        let ready_tail = "› Ask Codex to do anything\n  GPT-6.2-Sol high · ~/Work/workflow\n  ← for agents · ? for shortcuts\n";
        let (rec_work, _) = apply_legacy_tail_hash(&mut st, &sid, &pane, ready_tail, 8_000)
            .expect("recovery transition");
        assert_eq!(rec_work.state, SessionState::Working);
        assert_eq!(rec_work.last_error, None);
    }

    #[tokio::test]
    async fn local_transcript_advance_broadcasts_output_event_and_enforces_32kib_cap() {
        let root = std::env::temp_dir().join(format!("starfane-disc-out-{}", id::new_id("T")));
        let tr_path = root.join("claude-session.jsonl");
        std::fs::create_dir_all(&root).unwrap();
        let line1 =
            r#"{"type":"user","timestamp":"2026-11-25T10:00:00Z","message":{"content":"hello"}}"#;
        std::fs::write(&tr_path, format!("{line1}\n")).unwrap();

        let mut cfg = crate::config::StarfaneConfig::default();
        cfg.paths.tmux = "/usr/bin/false".into();
        cfg.zellij.enabled = false;
        let core = Core::new(&crate::DaemonConfig {
            state_dir: root.join("state"),
            socket: root.join("state/starfaned.sock"),
            config: cfg,
            web_dir: None,
            foreground: true,
        })
        .unwrap();

        let sid = SessionId::new("local-claude-1");
        lock(&core.inner.state).sessions.insert(
            sid.clone(),
            SessionRecord {
                session_id: sid.clone(),
                agent: AgentKind::Claude,
                pane: Some(PaneId::new("%1")),
                pane_resolution: PaneResolution::Env,
                cwd: Some(root.to_string_lossy().into_owned()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Working,
                state_since_ms: 1_000,
                last_event_id: None,
                last_event_ms: 1_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("work:0.0".into()),
                tmux_label: Some("work-main".into()),
                discovered_only: false,
                transcript_path: Some(tr_path.to_string_lossy().into_owned()),
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: Some("tmux".into()),
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        let mut rx = core.subscribe();

        // First sync establishes baseline (1 message) without flooding history.
        core.refresh_session_metrics(&sid).await;
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, DaemonEvent::Output { .. }),
                "baseline sync must not flood Output: {ev:?}"
            );
        }

        // Append a small assistant turn -> broadcasts inline DaemonEvent::Output (<= 32 KiB, dirty=false).
        let line2 = r#"{"type":"assistant","timestamp":"2026-11-25T10:00:01Z","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"inline reply"}]}}"#;
        std::fs::write(&tr_path, format!("{line1}\n{line2}\n")).unwrap();
        core.refresh_session_metrics(&sid).await;

        let mut saw_small = None;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, DaemonEvent::Output { .. }) {
                saw_small = Some(ev);
            }
        }
        match saw_small.expect("expected inline DaemonEvent::Output") {
            DaemonEvent::Output {
                session_id,
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(2));
                assert_eq!(appended_messages.len(), 1);
                assert_eq!(appended_messages[0]["text"], "inline reply");
                assert!(!dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        // Repeated sync with no new messages emits NO duplicate DaemonEvent::Output.
        core.refresh_session_metrics(&sid).await;
        while let Ok(ev) = rx.try_recv() {
            assert!(!matches!(ev, DaemonEvent::Output { .. }));
        }

        // Append an oversized (> 32 KiB) turn -> omits appended_messages and sets dirty=true!
        let big_text = "x".repeat(40 * 1024);
        let line3 = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-11-25T10:00:02Z",
            "message": {
                "id": "m2",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": big_text}]
            }
        })
        .to_string();
        std::fs::write(&tr_path, format!("{line1}\n{line2}\n{line3}\n")).unwrap();
        core.refresh_session_metrics(&sid).await;

        let mut saw_large = None;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, DaemonEvent::Output { .. }) {
                saw_large = Some(ev);
            }
        }
        match saw_large.expect("expected dirty DaemonEvent::Output for >32 KiB turn") {
            DaemonEvent::Output {
                session_id,
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(start_seq, Some(2));
                assert_eq!(total_messages, Some(3));
                assert!(appended_messages.is_empty());
                assert!(dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    fn test_core(root: &Path) -> Core {
        let mut cfg = crate::config::StarfaneConfig::default();
        cfg.paths.tmux = "/usr/bin/false".into();
        cfg.zellij.enabled = false;
        Core::new(&crate::DaemonConfig {
            state_dir: root.join("state"),
            socket: root.join("state/starfaned.sock"),
            config: cfg,
            web_dir: None,
            foreground: true,
        })
        .unwrap()
    }

    #[test]
    fn sync_transcript_delta_detects_in_place_mutations_hydration_growth_overlap_and_shrink() {
        let root = std::env::temp_dir().join(format!("starfane-disc-wm-{}", id::new_id("T")));
        std::fs::create_dir_all(&root).unwrap();
        let core = test_core(&root);

        // 1. AGY: trailing pending_tools -> final PLANNER_RESPONSE (N -> N) + late hydration (N -> N)
        let agy_dir = root.join("agy-conv");
        std::fs::create_dir_all(&agy_dir).unwrap();
        let agy_compact = agy_dir.join("transcript.jsonl");
        let agy_full = agy_dir.join("transcript_full.jsonl");
        let u1 = r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-26T00:00:00Z","content":"check build"}"#;
        let p_tools = r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:01Z","tool_calls":[{"name":"run_command","args":{"CommandLine":"cargo check"}}]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n")).unwrap();

        let agy_sid = SessionId::new("agy-wm-1");
        // First observation seeds baseline -> None.
        let (_, _, _, d0) =
            sync_transcript_and_output_delta(&core, &agy_sid, AgentKind::Agy, &agy_compact, 1_000);
        assert!(d0.is_none());

        // Final PLANNER_RESPONSE replaces synthetic pending_tools at index 1 (total 2 -> 2).
        let p_final_trunc = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"Truncated summary... <truncated 100 chars>","truncated_fields":["content"]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n{p_final_trunc}\n")).unwrap();
        let (_, _, _, d1) =
            sync_transcript_and_output_delta(&core, &agy_sid, AgentKind::Agy, &agy_compact, 2_000);
        let d1 = d1.expect("expected Output delta on synthetic pending_tools -> final");
        assert_eq!(d1.start_seq, Some(1));
        assert_eq!(d1.total_messages, Some(2));
        assert!(!d1.dirty);
        assert_eq!(d1.appended_messages.len(), 1);
        assert_eq!(
            d1.appended_messages[0]["text"],
            "Truncated summary... <truncated 100 chars>"
        );

        // Late transcript_full.jsonl hydration changes last_turn_hash at same total_messages = 2.
        let p_final_full = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"Build succeeded with zero warnings."}"#;
        std::fs::write(&agy_full, format!("{p_final_full}\n")).unwrap();
        let (_, _, _, d2) =
            sync_transcript_and_output_delta(&core, &agy_sid, AgentKind::Agy, &agy_compact, 3_000);
        let d2 = d2.expect("expected Output delta on late transcript_full.jsonl hydration");
        assert_eq!(d2.start_seq, Some(1));
        assert_eq!(d2.total_messages, Some(2));
        assert!(!d2.dirty);
        assert_eq!(
            d2.appended_messages[0]["text"],
            "Build succeeded with zero warnings."
        );

        // 2. Growth where previous last turn was synthetic pending_tools overlaps by 1 (start_seq = old_total - 1).
        let agy_growth = root.join("agy-growth.jsonl");
        let p_tools2 = r#"{"step_index":3,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:03Z","tool_calls":[{"name":"read_file","args":{"Path":"x"}}]}"#;
        std::fs::write(&agy_growth, format!("{u1}\n{p_tools2}\n")).unwrap();
        let growth_sid = SessionId::new("agy-wm-growth");
        assert!(sync_transcript_and_output_delta(
            &core,
            &growth_sid,
            AgentKind::Agy,
            &agy_growth,
            4_000,
        )
        .3
        .is_none());
        // Append USER_INPUT + PLANNER_RESPONSE -> pending_tools attaches to the next assistant turn
        // or clears on USER_INPUT, increasing total_messages from 2 to 3 while index 1 changed!
        let u2 = r#"{"step_index":4,"type":"USER_INPUT","created_at":"2026-11-26T00:00:04Z","content":"next step"}"#;
        let p_final2 = r#"{"step_index":5,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:05Z","content":"Next step complete."}"#;
        std::fs::write(&agy_growth, format!("{u1}\n{p_tools2}\n{u2}\n{p_final2}\n")).unwrap();
        let (_, _, _, dg) = sync_transcript_and_output_delta(
            &core,
            &growth_sid,
            AgentKind::Agy,
            &agy_growth,
            5_000,
        );
        let dg = dg.expect("expected growth delta overlapping previous pending_tools");
        assert_eq!(dg.start_seq, Some(1));
        assert_eq!(dg.total_messages, Some(3));
        assert_eq!(dg.appended_messages.len(), 2);
        assert_eq!(dg.appended_messages[0]["text"], "next step");
        assert_eq!(dg.appended_messages[1]["text"], "Next step complete.");
        assert!(!dg.dirty);

        // 3. Claude: same-msg_id in-place append (N -> N) + shrink (new_total < old_total) -> dirty=true.
        let claude_path = root.join("claude-wm.jsonl");
        let cu =
            r#"{"type":"user","timestamp":"2026-11-26T00:00:00Z","message":{"content":"hello"}}"#;
        let ca1 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:01Z","message":{"id":"m1","stop_reason":null,"content":[{"type":"text","text":"Part 1"}]}}"#;
        std::fs::write(&claude_path, format!("{cu}\n{ca1}\n")).unwrap();
        let c_sid = SessionId::new("claude-wm-1");
        assert!(sync_transcript_and_output_delta(
            &core,
            &c_sid,
            AgentKind::Claude,
            &claude_path,
            6_000,
        )
        .3
        .is_none());
        let ca2 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:02Z","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"Part 2"}]}}"#;
        std::fs::write(&claude_path, format!("{cu}\n{ca1}\n{ca2}\n")).unwrap();
        let (_, _, _, dc1) =
            sync_transcript_and_output_delta(&core, &c_sid, AgentKind::Claude, &claude_path, 7_000);
        let dc1 = dc1.expect("expected Output delta on same-msg_id append");
        assert_eq!(dc1.start_seq, Some(1));
        assert_eq!(dc1.total_messages, Some(2));
        assert!(!dc1.dirty);
        assert_eq!(dc1.appended_messages[0]["text"], "Part 1\n\nPart 2");

        // Shrink from 2 -> 1 emits dirty: true.
        std::fs::write(&claude_path, format!("{cu}\n")).unwrap();
        let (_, _, _, dc_shrink) =
            sync_transcript_and_output_delta(&core, &c_sid, AgentKind::Claude, &claude_path, 8_000);
        let dc_shrink = dc_shrink.expect("expected dirty delta on shrink");
        assert_eq!(dc_shrink.start_seq, None);
        assert_eq!(dc_shrink.total_messages, Some(1));
        assert!(dc_shrink.dirty);
        assert!(dc_shrink.appended_messages.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn local_subagent_transcript_advance_emits_output_via_sync_discovered_panes() {
        let root = std::env::temp_dir().join(format!("starfane-sub-out-{}", id::new_id("T")));
        let sub_path = root.join("subagent.jsonl");
        std::fs::create_dir_all(&root).unwrap();
        let u1 =
            r#"{"type":"user","timestamp":"2026-11-26T00:00:00Z","message":{"content":"audit"}}"#;
        std::fs::write(&sub_path, format!("{u1}\n")).unwrap();

        let core = test_core(&root);
        let sub_sid = SessionId::new("root-1/agent-sub1");
        lock(&core.inner.state).sessions.insert(
            sub_sid.clone(),
            SessionRecord {
                session_id: sub_sid.clone(),
                agent: AgentKind::Claude,
                pane: None,
                pane_resolution: PaneResolution::Unknown,
                cwd: Some(root.to_string_lossy().into_owned()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Working,
                state_since_ms: 1_000,
                last_event_id: None,
                last_event_ms: 1_000,
                last_message: None,
                last_error: None,
                parent_session_id: Some(SessionId::new("root-1")),
                is_subagent: true,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: None,
                tmux_label: Some("auditor".into()),
                discovered_only: false,
                transcript_path: Some(sub_path.to_string_lossy().into_owned()),
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: None,
                resume_id: None,
                suspended_at_ms: None,
            },
        );

        let mut rx = core.subscribe();
        // Tick 1: list_all_panes fails with /usr/bin/false? Wait: verify sync_discovered_panes vs direct scan:
        // Use sync_transcript_and_output_delta to check subagent path resolution + watermark directly if tmux is disabled,
        // and verify resolve_transcript_path_with_home works for subagents.
        let rec = lock(&core.inner.state)
            .sessions
            .get(&sub_sid)
            .unwrap()
            .clone();
        let resolved = resolve_transcript_path_with_home(None, &rec).unwrap();
        assert_eq!(resolved, sub_path);
        assert!(
            sync_transcript_and_output_delta(&core, &sub_sid, rec.agent, &resolved, 1_000)
                .3
                .is_none()
        );
        let a1 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:01Z","message":{"id":"sm1","stop_reason":"end_turn","content":[{"type":"text","text":"subagent done"}]}}"#;
        std::fs::write(&sub_path, format!("{u1}\n{a1}\n")).unwrap();
        let (_, _, _, delta) =
            sync_transcript_and_output_delta(&core, &sub_sid, rec.agent, &resolved, 2_000);
        let d = delta.expect("expected subagent output delta");
        assert_eq!(d.start_seq, Some(1));
        assert_eq!(d.total_messages, Some(2));
        assert_eq!(d.appended_messages[0]["text"], "subagent done");
        core.publish_output(
            sub_sid.clone(),
            d.start_seq,
            d.total_messages,
            d.appended_messages,
            None,
            None,
            d.dirty,
        );
        let ev = rx.try_recv().unwrap();
        match ev {
            DaemonEvent::Output {
                session_id,
                start_seq,
                total_messages,
                appended_messages,
                ..
            } => {
                assert_eq!(session_id, sub_sid);
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(2));
                assert_eq!(appended_messages[0]["text"], "subagent done");
            }
            other => panic!("unexpected {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pre_birth_guard_skips_telemetry_and_output_delta_but_preserves_live_tail() {
        let pane = PaneId::new("%55");
        let sid = legacy_session_id(&pane);
        let mut rec = SessionRecord {
            session_id: sid.clone(),
            agent: AgentKind::Claude,
            pane: Some(pane.clone()),
            pane_resolution: PaneResolution::Env,
            cwd: Some("/tmp".into()),
            git_root: None,
            model: None,
            effort: None,
            state: SessionState::Working,
            state_since_ms: 10_000,
            last_event_id: None,
            last_event_ms: 10_000,
            last_message: None,
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Heuristic,
            tmux_target: Some("dev:0.0".into()),
            tmux_label: Some("dev".into()),
            discovered_only: true,
            transcript_path: None,
            metrics: None,
            host_id: None,
            host_display_name: None,
            mux_kind: Some("tmux".into()),
            resume_id: None,
            suspended_at_ms: None,
        };

        let mut pane_birth = HashMap::new();
        pane_birth.insert(pane.clone(), 50_000);
        let auth = HashSet::new();
        let mut metrics_updated = Vec::new();
        let mut output_pushes = Vec::new();

        // Pre-birth transcript/cc telemetry (updated_ms = 1_000 < birth 50_000) must NOT apply
        // model/effort/metrics or output_delta, yet MUST preserve live_tail!
        apply_computed_root_metrics(
            &mut rec,
            ComputedRootMetrics {
                sid: sid.clone(),
                agent: AgentKind::Claude,
                tr_model: Some("stale-tr-model".into()),
                tr_effort: Some("high".into()),
                st_model: None,
                st_effort: None,
                combined: Some(SessionMetrics {
                    context_used_tokens: Some(100),
                    context_window_tokens: Some(200_000),
                    context_used_pct: Some(0.05),
                    tps: None,
                    ttft_ms: None,
                    avg_ttft_ms: None,
                    last_output_tokens: None,
                    last_duration_ms: None,
                    total_input_tokens: None,
                    total_output_tokens: None,
                    total_cache_read_tokens: None,
                    recent_turns: Vec::new(),
                    source: "transcript".into(),
                    updated_ms: 1_000,
                }),
                output_delta: Some(TranscriptOutputDelta {
                    start_seq: Some(0),
                    total_messages: Some(1),
                    appended_messages: vec![serde_json::json!({"text": "stale"})],
                    dirty: false,
                }),
                live_tail: Some("fresh terminal prompt".into()),
                has_transcript: false,
            },
            &auth,
            &pane_birth,
            None,
            &mut metrics_updated,
            &mut output_pushes,
        );

        assert_eq!(rec.model, None);
        assert_eq!(rec.metrics, None);
        assert!(metrics_updated.is_empty());
        assert_eq!(output_pushes.len(), 1);
        assert_eq!(
            output_pushes[0].live_tail.as_deref(),
            Some("fresh terminal prompt")
        );
        assert!(output_pushes[0].appended_messages.is_empty());
        assert!(output_pushes[0].dirty);
    }

    #[test]
    fn codex_resume_path_cache_and_transcript_birth_cache_preserve_freshness() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-disc-cache-{}",
            starfane_proto::id::new_id("T")
        ));
        let rollout_dir = dir.join(".codex/sessions/2026/11/27");
        std::fs::create_dir_all(&rollout_dir).unwrap();
        let uuid = "019a0000-1111-2222-3333-444455556666";
        let rollout = rollout_dir.join(format!("rollout-2026-11-27T12-00-00-{uuid}.jsonl"));
        std::fs::write(
            &rollout,
            r#"{"type":"session_meta","timestamp":"2026-11-27T12:00:00Z","payload":{"id":"s1"}}
"#,
        )
        .unwrap();

        let mut dc = DiscoveryScanCache::default();
        let p1 = dc.cached_codex_resume_path(Some(&dir), uuid);
        assert_eq!(p1.as_deref(), Some(rollout.as_path()));
        let p2 = dc.cached_codex_resume_path(Some(&dir), uuid);
        assert_eq!(p2.as_deref(), Some(rollout.as_path()));

        // Relocating the rollout file invalidates the stale cached path and resolves the new day dir.
        let rollout_dir2 = dir.join(".codex/sessions/2026/11/28");
        std::fs::create_dir_all(&rollout_dir2).unwrap();
        let rollout2 = rollout_dir2.join(format!("rollout-2026-11-28T00-00-00-{uuid}.jsonl"));
        std::fs::rename(&rollout, &rollout2).unwrap();
        let p3 = dc.cached_codex_resume_path(Some(&dir), uuid);
        assert_eq!(p3.as_deref(), Some(rollout2.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suspended_session_survives_shell_discovery_wakes_with_transcript_and_ends_when_pane_closed()
    {
        let mut st = State::default();
        let pane = PaneId::new("%88");
        let sid = SessionId::new("real-claude-88");
        st.sessions.insert(
            sid.clone(),
            SessionRecord {
                session_id: sid.clone(),
                agent: AgentKind::Claude,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp/proj".into()),
                git_root: None,
                model: Some("opus".into()),
                effort: Some("high".into()),
                state: SessionState::Suspended,
                state_since_ms: 10_000,
                last_event_id: None,
                last_event_ms: 10_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("work:0.0".into()),
                tmux_label: Some("work-proj".into()),
                discovered_only: false,
                transcript_path: Some("/tmp/tr-88.jsonl".into()),
                metrics: Some(SessionMetrics {
                    context_used_tokens: Some(40_000),
                    context_window_tokens: Some(200_000),
                    context_used_pct: Some(20.0),
                    tps: Some(50.0),
                    ttft_ms: None,
                    avg_ttft_ms: None,
                    last_output_tokens: None,
                    last_duration_ms: None,
                    total_input_tokens: Some(80_000),
                    total_output_tokens: Some(2_000),
                    total_cache_read_tokens: Some(50_000),
                    recent_turns: Vec::new(),
                    source: "transcript".into(),
                    updated_ms: 9_000,
                }),
                host_id: None,
                host_display_name: None,
                mux_kind: Some("tmux".into()),
                resume_id: Some("uuid-88".into()),
                suspended_at_ms: Some(10_000),
            },
        );

        let live: HashSet<PaneId> = [pane.clone()].into_iter().collect();
        let restarted: HashSet<PaneId> = [pane.clone()].into_iter().collect();

        // 1. Even with `include_shell_panes = true` (shell running waiter) & no coding-agent match,
        //    Suspended session stays alive and NO duplicate `tmux-%88` Shell session is created.
        let d_shell = reconcile_discovered_panes_with_restarts(
            &mut st,
            &[mk_dp("%88", "work:0.0", AgentKind::Shell, Some("sh"))],
            &live,
            &restarted,
            20_000,
        );
        assert!(!d_shell.changed);
        assert_eq!(st.sessions[&sid].state, SessionState::Suspended);
        assert!(!st.sessions.contains_key(&SessionId::new("tmux-%88")));

        // Also survives when discovered is empty (`include_shell_panes = false`) as long as `pane` is in `live`.
        let d_empty =
            reconcile_discovered_panes_with_restarts(&mut st, &[], &live, &restarted, 25_000);
        assert!(!d_empty.changed);
        assert_eq!(st.sessions[&sid].state, SessionState::Suspended);

        // 2. Pressing Enter in terminal wakes `claude` -> same session wakes cleanly to `Idle`
        //    preserving `transcript_path` and `metrics`!
        let d_wake = reconcile_discovered_panes_with_restarts(
            &mut st,
            &[mk_dp("%88", "work:0.0", AgentKind::Claude, Some("opus"))],
            &live,
            &restarted,
            30_000,
        );
        assert!(d_wake.changed);
        assert_eq!(st.sessions[&sid].state, SessionState::Idle);
        assert_eq!(st.sessions[&sid].suspended_at_ms, None);
        assert_eq!(
            st.sessions[&sid].transcript_path.as_deref(),
            Some("/tmp/tr-88.jsonl")
        );
        assert!(st.sessions[&sid].metrics.is_some());

        // Re-suspend and close the terminal pane (`!live.contains("%88")`) -> marks Ended cleanly.
        st.sessions.get_mut(&sid).unwrap().state = SessionState::Suspended;
        let d_closed = reconcile_discovered_panes_with_restarts(
            &mut st,
            &[],
            &[PaneId::new("%999")].into_iter().collect(),
            &HashSet::new(),
            40_000,
        );
        assert!(d_closed.changed);
        assert_eq!(st.sessions[&sid].state, SessionState::Ended);
        assert_eq!(st.sessions[&sid].pane, None);
    }

    #[tokio::test]
    async fn suspend_session_rejects_shell_and_missing_resume_id() {
        let root = std::env::temp_dir().join(format!(
            "starfane-susp-err-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let core = test_core(&root);
        let sid = SessionId::new("tmux-%50");
        lock(&core.inner.state).sessions.insert(
            sid.clone(),
            SessionRecord {
                session_id: sid.clone(),
                agent: AgentKind::Claude,
                pane: Some(PaneId::new("%50")),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp".into()),
                git_root: None,
                model: None,
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 1_000,
                last_event_id: None,
                last_event_ms: 1_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: Some("w:0.0".into()),
                tmux_label: Some("w".into()),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: Some("tmux".into()),
                resume_id: None,
                suspended_at_ms: None,
            },
        );
        let err = core.suspend_session(&sid).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
        assert_eq!(
            lock(&core.inner.state).sessions[&sid].state,
            SessionState::Idle
        );

        let shell_sid = SessionId::new("tmux-%51");
        lock(&core.inner.state).sessions.insert(
            shell_sid.clone(),
            SessionRecord {
                session_id: shell_sid.clone(),
                agent: AgentKind::Shell,
                pane: Some(PaneId::new("%51")),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/tmp".into()),
                git_root: None,
                model: Some("zsh".into()),
                effort: None,
                state: SessionState::Idle,
                state_since_ms: 1_000,
                last_event_id: None,
                last_event_ms: 1_000,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: Some("w:0.1".into()),
                tmux_label: Some("w".into()),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: None,
                host_display_name: None,
                mux_kind: Some("tmux".into()),
                resume_id: None,
                suspended_at_ms: None,
            },
        );
        let shell_err = core.suspend_session(&shell_sid).await.unwrap_err();
        assert_eq!(shell_err.code, ErrorCode::BadRequest);

        let _ = std::fs::remove_dir_all(&root);
    }
}
