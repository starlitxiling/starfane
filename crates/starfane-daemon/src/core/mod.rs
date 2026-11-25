//! `Core`: shared daemon state + command dispatch (contracts §7).

mod backup;
mod delivery;
pub(crate) mod discover;
mod engine;
mod ingest;
pub(crate) mod metrics;
pub(crate) mod models;
mod review;
pub(crate) mod router;
pub(crate) mod slash;
pub(crate) mod state;
mod tasks;
pub(crate) mod transcript;
pub use transcript::TranscriptQuery;
pub(crate) mod uploads;
pub(crate) mod util;

use crate::access::AccessGate;
use crate::config::{StarfaneConfig, TeamConfig};
use crate::nodes::{
    build_remote_providers_with_transcript_cache, HostProbeOutcome, LocalMuxPool, NodeProvider,
    NodeSnapshot,
};
use crate::notice::{FeishuSender, Notice, NoticeKind};
use crate::notify::{NotifyRouteContext, NotifyRouter};
use crate::store::{write_private, Store};
use crate::{DaemonConfig, DaemonError};
use starfane_proto::*;
use state::State;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(3);
const SPOOL_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVER_INTERVAL: Duration = Duration::from_secs(3);
const REMOTE_SYNC_BUSY_FAST_INTERVAL: Duration = Duration::from_secs(3);
const REMOTE_SYNC_BUSY_SLOW_INTERVAL: Duration = Duration::from_secs(5);
const REMOTE_SYNC_RECENT_IDLE_INTERVAL: Duration = Duration::from_secs(5);
const REMOTE_SYNC_IDLE_INTERVAL: Duration = Duration::from_secs(8);
const REMOTE_IDLE_LONG_MS: u64 = 60_000;
const REMOTE_BACKOFF_SLACK_MS: u64 = 25;
const REMOTE_RETRY_MAX_SLEEP_MS: u64 = 65_000;
const EVENT_CHANNEL: usize = 1024;
const FEISHU_RATE_WINDOW_MS: u64 = 60_000;
const MAX_FEISHU_NOTICES_PER_WINDOW: usize = 6;

/// Off-state IO caches (SQLite, `lsof`, transcript telemetry) kept outside `Mutex<State>`
/// so blocking disk/subprocess queries never contend with control-plane reads/writes.
#[derive(Debug, Default)]
pub(crate) struct TelemetryCaches {
    pub cc_switch_cache: metrics::CcSwitchCache,
    pub codex_threads_cache: metrics::CodexThreadCache,
    pub agy_convs_cache: metrics::AgyConvCache,
    pub agy_presence_cache: metrics::AgyPresenceCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeishuRateDecision {
    Allow,
    EmitThrottleSummary,
    Drop,
}

/// Global sliding-window rate limiter protecting Feishu against unexpected notification storms.
pub(crate) fn check_feishu_rate_limit(st: &mut State, now_ms: u64) -> FeishuRateDecision {
    while let Some(&oldest) = st.feishu_sent_ms.front() {
        if now_ms.saturating_sub(oldest) >= FEISHU_RATE_WINDOW_MS {
            st.feishu_sent_ms.pop_front();
        } else {
            break;
        }
    }
    if st.feishu_sent_ms.len() < MAX_FEISHU_NOTICES_PER_WINDOW {
        st.feishu_sent_ms.push_back(now_ms);
        return FeishuRateDecision::Allow;
    }
    if st
        .feishu_rate_warned_window_ms
        .is_some_and(|w| now_ms.saturating_sub(w) < FEISHU_RATE_WINDOW_MS)
    {
        return FeishuRateDecision::Drop;
    }
    st.feishu_rate_warned_window_ms = Some(now_ms);
    FeishuRateDecision::EmitThrottleSummary
}

#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub cfg: StarfaneConfig,
    pub state_dir: PathBuf,
    pub web_dir: Option<PathBuf>,
    pub started_ms: u64,
    pub web_token: String,
    pub state: Mutex<State>,
    pub events: broadcast::Sender<DaemonEvent>,
    pub replay: Mutex<VecDeque<DaemonEvent>>,
    pub seq: AtomicU64,
    pub notice_tx: mpsc::UnboundedSender<Notice>,
    pub notice_rx: Mutex<Option<mpsc::UnboundedReceiver<Notice>>>,
    pub feishu: RwLock<Option<Arc<dyn FeishuSender>>>,
    pub tmux: starfane_tmux::Tmux,
    pub mux: LocalMuxPool,
    pub access_gate: AccessGate,
    pub notify_router: NotifyRouter,
    pub remote_nodes: HashMap<HostId, Arc<dyn NodeProvider>>,
    pub remote_wake: HashMap<HostId, Arc<Notify>>,
    pub store: Store,
    pub acks: Mutex<HashMap<SessionId, Vec<oneshot::Sender<()>>>>,
    pub dirty: AtomicBool,
    pub snapshot_in_flight: AtomicBool,
    pub shutdown: Notify,
    pub last_attention: Mutex<Vec<AttentionItem>>,
    pub model_catalog: Mutex<Option<models::ModelCatalog>>,
    pub slash_cache: Mutex<slash::SlashCache>,
    pub telemetry: Mutex<TelemetryCaches>,
    pub transcript_index: Arc<Mutex<transcript::TranscriptIndexCache>>,
    pub discovery_cache: Mutex<discover::DiscoveryScanCache>,
}

/// Lock helper: a poisoned mutex means a panic happened while mutating state;
/// keep serving with the inner data rather than taking the daemon down.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Core {
    /// Build a core (loads the snapshot if present).
    pub fn new(cfg: &DaemonConfig) -> Result<Core, DaemonError> {
        let store = Store::new(cfg.state_dir.clone());
        store.ensure_dirs()?;
        let mut state: State = match store.load_snapshot::<State>() {
            Ok(Some(s)) => s,
            Ok(None) => State::default(),
            Err(e) => {
                // Keep the corrupt file for inspection and start fresh.
                let bad = store.snapshot_path().with_extension("corrupt.json");
                log::error!("snapshot unreadable ({e}); moved to {}", bad.display());
                std::fs::rename(store.snapshot_path(), &bad)?;
                State::default()
            }
        };
        let startup_now = id::now_ms();
        // Purge legacy Codex memory/suggestion probes, stale CwdMatch hijackers on claimed panes,
        // and >24h Ended non-pipeline sessions from snapshots loaded at startup.
        let mut purged_ids: Vec<SessionId> = state
            .sessions
            .iter()
            .filter(|(_, s)| {
                ingest::is_codex_memory_task(
                    s.agent,
                    s.cwd.as_deref(),
                    s.transcript_path.as_deref(),
                ) || (s.agent == AgentKind::Codex
                    && s.last_message.as_deref().map(str::trim) == Some("{\"suggestions\":[]}"))
                    || (s.state == SessionState::Ended
                        && s.stage.is_none()
                        && startup_now.saturating_sub(s.last_event_ms) > 86_400_000)
            })
            .map(|(id, _)| id.clone())
            .collect();
        purged_ids.sort();
        purged_ids.dedup();
        let mut purged_any = !purged_ids.is_empty();
        for sid in purged_ids {
            state.sessions.remove(&sid);
            state.inbox.remove(&sid);
            state.feishu_session_threads.remove(&sid);
            state.feishu_session_thread_order.retain(|x| x != &sid);
        }
        // Retire non-Ended CwdMatch hijackers when an exact live owner holds the same pane or >1h stale.
        let exact_claimed_panes: std::collections::HashSet<PaneId> = state
            .sessions
            .values()
            .filter(|s| {
                !s.is_subagent
                    && !s.discovered_only
                    && s.state != SessionState::Ended
                    && state::is_exact_pane_resolution(s.pane_resolution)
            })
            .filter_map(|s| s.pane.clone())
            .collect();
        for s in state.sessions.values_mut() {
            if !s.is_subagent
                && !s.discovered_only
                && s.state != SessionState::Ended
                && s.pane_resolution == PaneResolution::CwdMatch
            {
                let stolen_pane = s
                    .pane
                    .as_ref()
                    .is_some_and(|p| exact_claimed_panes.contains(p));
                let stale_orphan = startup_now.saturating_sub(s.last_event_ms) > 3_600_000;
                if stolen_pane || stale_orphan {
                    s.state = SessionState::Ended;
                    state::clear_tmux_binding(s);
                    s.state_since_ms = startup_now;
                    purged_any = true;
                }
            }
        }
        if state.sanitize_orphan_tmux_metadata() {
            purged_any = true;
        }
        if state.dedup_split_sessions() {
            purged_any = true;
        }
        if state.cleanup_stale_message_inbox() {
            purged_any = true;
        }
        if state.migrate_legacy_feishu_mute(startup_now) {
            purged_any = true;
        }
        if state.prune_expired_mutes(startup_now) {
            purged_any = true;
        }
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let tmux = starfane_tmux::Tmux::new(
            PathBuf::from(cfg.config.tmux_bin()),
            cfg.config.tmux_socket(),
        );
        let mux = LocalMuxPool::new(
            tmux.clone(),
            PathBuf::from(cfg.config.zellij_bin()),
            cfg.config.zellij.enabled,
        );
        let access_gate = AccessGate::from_config(&cfg.config.web);
        let notify_router = NotifyRouter::from_config(&cfg.config);
        let transcript_index = Arc::new(Mutex::new(transcript::TranscriptIndexCache::default()));
        let remote_nodes = build_remote_providers_with_transcript_cache(
            &cfg.config.nodes,
            &cfg.state_dir,
            transcript_index.clone(),
        );
        let remote_wake: HashMap<HostId, Arc<Notify>> = remote_nodes
            .keys()
            .map(|hid| (hid.clone(), Arc::new(Notify::new())))
            .collect();

        // Seed runtime host registry (local + configured remote hosts).
        let local_id = HostId::new(LOCAL_HOST_ID);
        let local_display = state
            .host_display_names
            .get(&local_id)
            .cloned()
            .unwrap_or_else(|| {
                let d = cfg.config.node.display_name.trim();
                if d.is_empty() {
                    "Local".to_string()
                } else {
                    d.to_string()
                }
            });
        state.hosts.insert(
            local_id.clone(),
            HostRecord {
                id: local_id,
                display_name: local_display,
                kind: HostKind::Local,
                status: HostStatus::Online,
                active_endpoint: None,
                last_seen_ms: Some(startup_now),
                error: None,
                session_count: 0,
                rtt_ms: None,
                next_retry_ms: None,
            },
        );
        for (hid, provider) in &remote_nodes {
            let display = state
                .host_display_names
                .get(hid)
                .cloned()
                .unwrap_or_else(|| provider.default_display_name().to_string());
            state.hosts.insert(
                hid.clone(),
                HostRecord {
                    id: hid.clone(),
                    display_name: display,
                    kind: provider.kind(),
                    status: HostStatus::Offline,
                    active_endpoint: None,
                    last_seen_ms: None,
                    error: None,
                    session_count: 0,
                    rtt_ms: None,
                    next_retry_ms: None,
                },
            );
        }

        let web_token = id::new_id("W").to_lowercase().replace('-', "");
        Ok(Core {
            inner: Arc::new(Inner {
                cfg: cfg.config.clone(),
                state_dir: cfg.state_dir.clone(),
                web_dir: cfg.web_dir.clone(),
                started_ms: id::now_ms(),
                web_token,
                state: Mutex::new(state),
                events,
                replay: Mutex::new(VecDeque::with_capacity(SSE_REPLAY_CAPACITY)),
                seq: AtomicU64::new(0),
                notice_tx,
                notice_rx: Mutex::new(Some(notice_rx)),
                feishu: RwLock::new(None),
                tmux,
                mux,
                access_gate,
                notify_router,
                remote_nodes,
                remote_wake,
                store,
                acks: Mutex::new(HashMap::new()),
                dirty: AtomicBool::new(purged_any),
                snapshot_in_flight: AtomicBool::new(false),
                shutdown: Notify::new(),
                last_attention: Mutex::new(Vec::new()),
                model_catalog: Mutex::new(None),
                slash_cache: Mutex::new(slash::SlashCache::default()),
                telemetry: Mutex::new(TelemetryCaches::default()),
                transcript_index,
                discovery_cache: Mutex::new(discover::DiscoveryScanCache::default()),
            }),
        })
    }

    // ------------------------------------------------------------------
    // Public surface (web / lark / uds)
    // ------------------------------------------------------------------

    pub fn config(&self) -> &StarfaneConfig {
        &self.inner.cfg
    }
    pub fn access_gate(&self) -> &AccessGate {
        &self.inner.access_gate
    }
    pub fn notify_router(&self) -> &NotifyRouter {
        &self.inner.notify_router
    }
    pub fn started_ms(&self) -> u64 {
        self.inner.started_ms
    }
    pub fn web_token(&self) -> &str {
        &self.inner.web_token
    }
    pub fn web_dir(&self) -> Option<&Path> {
        self.inner.web_dir.as_deref()
    }
    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.inner.events.subscribe()
    }

    /// Buffered events with `seq > last_seq` (for `Last-Event-ID`).
    pub fn replay_since(&self, last_seq: u64) -> Vec<DaemonEvent> {
        lock(&self.inner.replay)
            .iter()
            .filter(|e| e.seq() > last_seq)
            .cloned()
            .collect()
    }

    pub fn take_notice_rx(&self) -> Option<mpsc::UnboundedReceiver<Notice>> {
        lock(&self.inner.notice_rx).take()
    }

    pub fn set_feishu(&self, sender: Arc<dyn FeishuSender>) {
        match self.inner.feishu.write() {
            Ok(mut g) => *g = Some(sender),
            Err(p) => *p.into_inner() = Some(sender),
        }
    }

    pub fn record_feishu_message(
        &self,
        pipeline: Option<&PipelineId>,
        chat_id: &str,
        message_id: &str,
    ) {
        let Some(pid) = pipeline else { return };
        let updated = {
            let mut st = lock(&self.inner.state);
            st.feishu_msgs.insert(message_id.to_string(), pid.clone());
            match st.pipelines.get_mut(pid) {
                Some(p) if p.feishu_thread.is_none() => {
                    p.feishu_thread = Some(FeishuThreadRef {
                        chat_id: chat_id.to_string(),
                        root_message_id: message_id.to_string(),
                    });
                    Some(p.clone())
                }
                _ => None,
            }
        };
        self.mark_dirty();
        if let Some(p) = updated {
            self.persist_pipeline(&p);
        }
    }

    /// Explicitly replace a pipeline's thread root after the previous root message was deleted.
    pub fn replace_feishu_thread(&self, pipeline: &PipelineId, chat_id: &str, message_id: &str) {
        let updated = {
            let mut st = lock(&self.inner.state);
            st.feishu_msgs
                .insert(message_id.to_string(), pipeline.clone());
            match st.pipelines.get_mut(pipeline) {
                Some(p) => {
                    p.feishu_thread = Some(FeishuThreadRef {
                        chat_id: chat_id.to_string(),
                        root_message_id: message_id.to_string(),
                    });
                    Some(p.clone())
                }
                None => None,
            }
        };
        self.mark_dirty();
        if let Some(p) = updated {
            self.persist_pipeline(&p);
        }
    }

    pub fn feishu_thread(&self, pipeline: &PipelineId) -> Option<FeishuThreadRef> {
        lock(&self.inner.state)
            .pipelines
            .get(pipeline)
            .and_then(|p| p.feishu_thread.clone())
    }

    pub fn pipeline_for_message(&self, message_id: &str) -> Option<PipelineId> {
        let st = lock(&self.inner.state);
        st.feishu_msgs.get(message_id).cloned().or_else(|| {
            st.pipelines
                .values()
                .find(|p| {
                    p.feishu_thread
                        .as_ref()
                        .is_some_and(|t| t.root_message_id == message_id)
                })
                .map(|p| p.id.clone())
        })
    }

    /// Record a sent Feishu session-notification card `message_id` → session target (bounded to 512 entries).
    pub fn record_feishu_session_message(
        &self,
        message_id: &str,
        target: crate::notice::SessionReplyTarget,
    ) {
        const MAX_SESSION_MSGS: usize = 512;
        if message_id.trim().is_empty() {
            return;
        }
        {
            let mut st = lock(&self.inner.state);
            let mid = message_id.to_string();
            if !st.feishu_session_msgs.contains_key(&mid) {
                st.feishu_session_msg_order.push(mid.clone());
            }
            st.feishu_session_msgs.insert(mid, target);
            while st.feishu_session_msg_order.len() > MAX_SESSION_MSGS {
                let oldest = st.feishu_session_msg_order.remove(0);
                st.feishu_session_msgs.remove(&oldest);
            }
        }
        self.mark_dirty();
    }

    pub fn session_for_feishu_message(
        &self,
        message_id: &str,
    ) -> Option<crate::notice::SessionReplyTarget> {
        let st = lock(&self.inner.state);
        if let Some(hit) = st.feishu_session_msgs.get(message_id) {
            return Some(hit.clone());
        }
        // Fallback: if `message_id` is a persisted session-thread root whose msg entry was evicted,
        // reconstruct the reply target from live session state so thread replies keep working.
        let sid = st
            .feishu_session_threads
            .iter()
            .find(|(_, root)| root.as_str() == message_id)
            .map(|(sid, _)| sid.clone())?;
        let s = st.sessions.get(&sid)?;
        Some(crate::notice::SessionReplyTarget {
            session_id: sid,
            pane_id: s.pane.clone(),
            tmux_label: s.tmux_label.clone(),
            agent: s.agent,
        })
    }

    /// Return the root Feishu `message_id` for a session's continuous thread, if one exists.
    pub fn feishu_session_thread(&self, sid: &SessionId) -> Option<String> {
        lock(&self.inner.state)
            .feishu_session_threads
            .get(sid)
            .cloned()
    }

    /// Record or update the root Feishu `message_id` for a session's continuous thread (bounded to 512).
    pub fn record_feishu_session_thread(&self, sid: &SessionId, root_message_id: &str) {
        const MAX_SESSION_THREADS: usize = 512;
        if root_message_id.trim().is_empty() {
            return;
        }
        {
            let mut st = lock(&self.inner.state);
            let mid = root_message_id.to_string();
            if !st.feishu_session_threads.contains_key(sid) {
                st.feishu_session_thread_order.push(sid.clone());
            }
            st.feishu_session_threads.insert(sid.clone(), mid);
            while st.feishu_session_thread_order.len() > MAX_SESSION_THREADS {
                let oldest = st.feishu_session_thread_order.remove(0);
                st.feishu_session_threads.remove(&oldest);
            }
        }
        self.mark_dirty();
    }

    /// Return curated native `/` commands + 60s-cached local skills + Starfane shortcuts.
    pub async fn slash_commands(
        &self,
        agent: Option<AgentKind>,
    ) -> Result<slash::SlashCommandsResponse, ApiError> {
        let core = self.clone();
        let is_en = self.inner.cfg.web.is_en();
        tokio::task::spawn_blocking(move || {
            let home = slash::user_home_dir();
            let mut cache = lock(&core.inner.slash_cache);
            let scanned = cache.get_or_refresh(home.as_deref());
            slash::build_catalog(agent, scanned, is_en)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))
    }

    /// Cached model catalog for `GET /api/models` (computed once via `spawn_blocking`).
    pub async fn model_catalog(&self) -> Result<models::ModelCatalog, ApiError> {
        if let Some(hit) = lock(&self.inner.model_catalog).clone() {
            return Ok(hit);
        }
        let agy_bin = self.inner.cfg.agent_bin(AgentKind::Agy);
        let cat = tokio::task::spawn_blocking(move || models::discover_model_catalog(&agy_bin))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
        *lock(&self.inner.model_catalog) = Some(cat.clone());
        Ok(cat)
    }

    /// Snapshot active global & per-channel notification mutes (pruning expired deadlines).
    pub fn mute_snapshot(&self) -> crate::notify::MuteSnapshot {
        let now_ms = id::now_ms();
        let (snap, pruned) = {
            let mut st = lock(&self.inner.state);
            let pruned = st.prune_expired_mutes(now_ms);
            (
                crate::notify::MuteSnapshot {
                    global_mute_until_ms: st.global_mute_until_ms,
                    channel_mutes: st.channel_mutes.clone(),
                },
                pruned,
            )
        };
        if pruned {
            self.mark_dirty();
        }
        snap
    }

    fn publish_notifiers(&self, snap: &crate::notify::MuteSnapshot) {
        let global_mute_until_ms = snap.global_mute_until_ms;
        let channel_mutes = snap.channel_mutes.clone();
        self.publish(|seq| DaemonEvent::Notifiers {
            seq,
            global_mute_until_ms,
            channel_mutes,
        });
    }

    /// Set or clear global mute (`Some(until_ms)` mutes all channels; `None` unmutes global AND all per-channel mutes).
    pub fn set_global_mute(&self, until_ms: Option<u64>) -> crate::notify::MuteSnapshot {
        let now_ms = id::now_ms();
        let snap = {
            let mut st = lock(&self.inner.state);
            st.prune_expired_mutes(now_ms);
            match until_ms {
                Some(ts) if ts > now_ms => {
                    st.global_mute_until_ms = Some(ts);
                }
                _ => {
                    st.global_mute_until_ms = None;
                    st.feishu_muted_until = None;
                    st.channel_mutes.clear();
                }
            }
            st.prune_expired_mutes(now_ms);
            crate::notify::MuteSnapshot {
                global_mute_until_ms: st.global_mute_until_ms,
                channel_mutes: st.channel_mutes.clone(),
            }
        };
        self.mark_dirty();
        self.publish_notifiers(&snap);
        snap
    }

    /// Set or clear a per-channel mute for a configured notifier ID.
    pub fn set_channel_mute(
        &self,
        id: &str,
        until_ms: Option<u64>,
    ) -> Result<crate::notify::MuteSnapshot, ApiError> {
        let id = id.trim();
        if !self.inner.notify_router.has_id(id) {
            return Err(ApiError::not_found(format!("unknown notifier {id}")));
        }
        let now_ms = id::now_ms();
        let snap = {
            let mut st = lock(&self.inner.state);
            st.prune_expired_mutes(now_ms);
            match until_ms {
                Some(ts) if ts > now_ms => {
                    st.channel_mutes.insert(id.to_string(), ts);
                }
                _ => {
                    st.channel_mutes.remove(id);
                }
            }
            st.prune_expired_mutes(now_ms);
            crate::notify::MuteSnapshot {
                global_mute_until_ms: st.global_mute_until_ms,
                channel_mutes: st.channel_mutes.clone(),
            }
        };
        self.mark_dirty();
        self.publish_notifiers(&snap);
        Ok(snap)
    }

    /// Secret-free JSON snapshot for `GET /api/notifiers`.
    pub fn notifiers_status_json(&self) -> serde_json::Value {
        let now_ms = id::now_ms();
        let mutes = self.mute_snapshot();
        let ids = self.inner.notify_router.list_ids();
        let details = self.inner.notify_router.list_statuses(&mutes, now_ms);
        serde_json::json!({
            "notifiers": ids,
            "details": details,
            "global_mute_until_ms": mutes.global_mute_until_ms,
            "channel_mutes": mutes.channel_mutes,
            "any_muted": mutes.any_muted(now_ms),
        })
    }

    /// Backward-compatible Feishu mute API (updates global mute & emits SSE).
    pub fn set_feishu_mute(&self, until_ms: Option<u64>) {
        let _ = self.set_global_mute(until_ms);
    }

    /// Backward-compatible effective mute deadline for `feishu-default` (global OR per-channel).
    pub fn feishu_muted_until(&self) -> Option<u64> {
        let now_ms = id::now_ms();
        self.mute_snapshot()
            .effective_mute_until(crate::notify::DEFAULT_FEISHU_NOTIFIER_ID, now_ms)
    }

    /// Resolve a project by git-root basename / unique prefix / full path.
    pub fn find_project(&self, name_or_prefix: &str) -> Result<String, ApiError> {
        let needle = name_or_prefix.trim();
        if needle.is_empty() {
            return Err(ApiError::bad_request("project name is empty"));
        }
        let roots = lock(&self.inner.state).project_roots();
        if let Some(r) = roots.iter().find(|r| r.as_str() == needle) {
            return Ok(r.clone());
        }
        let exact: Vec<&String> = roots
            .iter()
            .filter(|r| util::project_name(r) == needle)
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0].clone());
        }
        let prefixed: Vec<&String> = roots
            .iter()
            .filter(|r| util::project_name(r).starts_with(needle))
            .collect();
        match prefixed.len() {
            1 => Ok(prefixed[0].clone()),
            0 if Path::new(needle).is_absolute() && Path::new(needle).is_dir() => {
                Ok(needle.to_string())
            }
            0 => Err(ApiError::not_found(format!(
                "unknown project {needle:?}; known: {}",
                roots
                    .iter()
                    .map(|r| util::project_name(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            _ => Err(ApiError::new(
                ErrorCode::Conflict,
                format!(
                    "ambiguous project {needle:?}: {}",
                    prefixed
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    pub fn audit(&self, source: &str, entry: serde_json::Value) {
        let line = serde_json::json!({"ts_ms": id::now_ms(), "source": source, "entry": entry});
        if let Err(e) = self.inner.store.append_audit(&line) {
            log::error!("audit write failed: {e}");
        }
    }

    /// Focus the pane's tmux/zellij window/pane (local or federated remote host).
    pub async fn focus_session(&self, id: &SessionId) -> Result<serde_json::Value, ApiError> {
        let (pane, host_id) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(id)
                .ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
            let pane = s
                .pane
                .clone()
                .ok_or_else(|| ApiError::not_found(format!("session {id} has no known pane")))?;
            (pane, HostId::new(s.effective_host_id()))
        };
        if host_id.as_str() != LOCAL_HOST_ID {
            if let Some(provider) = self.inner.remote_nodes.get(&host_id).cloned() {
                let (_, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
                let remote_sid = id
                    .as_str()
                    .strip_prefix(&format!("{}:", host_id.as_str()))
                    .map(SessionId::new)
                    .unwrap_or_else(|| id.clone());
                return provider
                    .focus_pane(&remote_sid, &PaneId::new(local_pane))
                    .await;
            }
        }
        let is_zellij = LocalMuxPool::is_zellij_pane(&pane);
        let core = self.clone();
        let outcome = tokio::task::spawn_blocking(move || core.inner.mux.focus_pane(&pane))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, e.to_string()))?;
        let mut payload = serde_json::to_value(&outcome)
            .map_err(|e| ApiError::internal(format!("serialize focus outcome: {e}")))?;
        if outcome.switched_client.is_none() {
            if let Some(obj) = payload.as_object_mut() {
                let attach = if is_zellij {
                    format!("zellij attach {}", outcome.session_name)
                } else {
                    format!("tmux attach -t {}", outcome.session_name)
                };
                obj.insert("attach_command".into(), serde_json::Value::String(attach));
            }
        }
        Ok(payload)
    }

    /// Return all known hosts (local + federated remote hosts) with live session counts.
    pub fn hosts(&self) -> Vec<HostRecord> {
        let st = lock(&self.inner.state);
        let mut counts: HashMap<String, usize> = HashMap::new();
        for s in st.sessions.values() {
            if !s.is_subagent && s.state != SessionState::Ended {
                *counts.entry(s.effective_host_id().to_string()).or_insert(0) += 1;
            }
        }
        let mut out: Vec<HostRecord> = st
            .hosts
            .values()
            .cloned()
            .map(|mut h| {
                h.session_count = counts.get(h.id.as_str()).copied().unwrap_or(0);
                h
            })
            .collect();
        // Ensure any host seen only on persisted/seeded sessions is also represented.
        let seen: HashSet<String> = out.iter().map(|h| h.id.0.clone()).collect();
        for (hid, count) in counts {
            if !seen.contains(&hid) {
                let display = st
                    .host_display_names
                    .get(&HostId::new(&hid))
                    .cloned()
                    .unwrap_or_else(|| hid.clone());
                out.push(HostRecord {
                    id: HostId::new(hid),
                    display_name: display,
                    kind: HostKind::Peer,
                    status: HostStatus::Online,
                    active_endpoint: None,
                    last_seen_ms: None,
                    error: None,
                    session_count: count,
                    rtt_ms: None,
                    next_retry_ms: None,
                });
            }
        }
        out.sort_by(|a, b| {
            let a_local = a.id.as_str() == LOCAL_HOST_ID;
            let b_local = b.id.as_str() == LOCAL_HOST_ID;
            b_local.cmp(&a_local).then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Rename a host's mutable human-friendly display name (immutable `HostId` stays stable).
    pub fn rename_host(
        &self,
        host_id: &HostId,
        display_name: &str,
    ) -> Result<HostRecord, ApiError> {
        let clean = display_name.trim();
        if clean.is_empty() || clean.chars().count() > 64 || clean.chars().any(char::is_control) {
            return Err(ApiError::bad_request(
                "display_name must be 1..=64 printable characters",
            ));
        }
        let (record, updated_sessions) = {
            let mut st = lock(&self.inner.state);
            let known = st.hosts.contains_key(host_id)
                || st
                    .sessions
                    .values()
                    .any(|s| s.effective_host_id() == host_id.as_str());
            if !known {
                return Err(ApiError::not_found(format!("unknown host {host_id}")));
            }
            st.host_display_names
                .insert(host_id.clone(), clean.to_string());
            if let Some(h) = st.hosts.get_mut(host_id) {
                h.display_name = clean.to_string();
            }
            let mut updated_sessions = Vec::new();
            for s in st.sessions.values_mut() {
                if s.effective_host_id() == host_id.as_str() {
                    s.host_display_name = Some(clean.to_string());
                    updated_sessions.push(s.clone());
                }
            }
            let count = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.state != SessionState::Ended
                        && s.effective_host_id() == host_id.as_str()
                })
                .count();
            let mut rec = st
                .hosts
                .get(host_id)
                .cloned()
                .unwrap_or_else(|| HostRecord {
                    id: host_id.clone(),
                    display_name: clean.to_string(),
                    kind: if host_id.as_str() == LOCAL_HOST_ID {
                        HostKind::Local
                    } else {
                        HostKind::Peer
                    },
                    status: HostStatus::Online,
                    active_endpoint: None,
                    last_seen_ms: Some(id::now_ms()),
                    error: None,
                    session_count: count,
                    rtt_ms: None,
                    next_retry_ms: None,
                });
            rec.session_count = count;
            (rec, updated_sessions)
        };
        self.mark_dirty();
        self.publish_host(record.clone());
        for s in updated_sessions {
            self.publish_session(s);
        }
        Ok(record)
    }

    /// True when `host_id` refers to a configured remote (`ssh` or `peer`) node provider.
    pub fn has_remote_host(&self, host_id: &str) -> bool {
        let trimmed = host_id.trim();
        !trimmed.is_empty()
            && trimmed != LOCAL_HOST_ID
            && self.inner.remote_nodes.contains_key(&HostId::new(trimmed))
    }

    /// Spawn one independent supervisor task per configured remote host so a hung or lossy WAN
    /// host has zero impact on local discovery or sibling remote hosts.
    pub fn spawn_remote_host_supervisors(&self) {
        for (hid, wake) in &self.inner.remote_wake {
            let core = self.clone();
            let hid = hid.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                core.run_remote_host_supervisor(hid, wake).await;
            });
        }
    }

    async fn run_remote_host_supervisor(self, host_id: HostId, wake: Arc<Notify>) {
        loop {
            self.sync_remote_host(&host_id).await;
            loop {
                let sleep_dur = self.compute_remote_host_sleep(&host_id);
                tokio::select! {
                    _ = self.inner.shutdown.notified() => return,
                    _ = wake.notified() => {
                        let should_sync_now = {
                            let now = id::now_ms();
                            let recently_synced = lock(&self.inner.state)
                                .hosts
                                .get(&host_id)
                                .and_then(|h| h.last_seen_ms)
                                .is_some_and(|t| now.saturating_sub(t) < 500);
                            let in_backoff = self
                                .inner
                                .remote_nodes
                                .get(&host_id)
                                .and_then(|p| p.next_retry_ms())
                                .is_some_and(|u| now < u);
                            !recently_synced && !in_backoff
                        };
                        if should_sync_now {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(sleep_dur) => break,
                }
            }
        }
    }

    /// Compute the adaptive sleep interval for a remote host's independent supervisor task:
    /// - `Degraded` / `Offline`: sleep until `next_retry_ms` (plus small slack so backoff has elapsed)
    /// - `Online` with active `Working`/`Starting`/`Needs*` sessions: `3s` (or `5s` when RTT >= 250ms)
    /// - `Online` with all sessions `Idle` >60s (or none): `8s`
    /// - `Online` with recently idle sessions: `5s`
    pub(crate) fn compute_remote_host_sleep(&self, host_id: &HostId) -> Duration {
        let Some(provider) = self.inner.remote_nodes.get(host_id) else {
            return REMOTE_SYNC_IDLE_INTERVAL;
        };
        let status = provider.circuit_status();
        let now = id::now_ms();
        if status != HostStatus::Online {
            if let Some(retry_at) = provider.next_retry_ms() {
                let wait_ms = retry_at
                    .saturating_sub(now)
                    .saturating_add(REMOTE_BACKOFF_SLACK_MS)
                    .clamp(200, REMOTE_RETRY_MAX_SLEEP_MS);
                return Duration::from_millis(wait_ms);
            }
            return REMOTE_SYNC_BUSY_SLOW_INTERVAL;
        }

        let st = lock(&self.inner.state);
        let mut any_live = false;
        let mut any_busy = false;
        let mut all_long_idle = true;
        for s in st.sessions.values() {
            if s.is_subagent
                || s.state == SessionState::Ended
                || s.effective_host_id() != host_id.as_str()
            {
                continue;
            }
            any_live = true;
            if matches!(
                s.state,
                SessionState::Working
                    | SessionState::Starting
                    | SessionState::NeedsPermission
                    | SessionState::NeedsInput
            ) {
                any_busy = true;
                break;
            }
            let last_activity = s.last_event_ms.max(s.state_since_ms);
            if now.saturating_sub(last_activity) <= REMOTE_IDLE_LONG_MS {
                all_long_idle = false;
            }
        }
        if any_busy {
            if provider.rtt_ms().is_some_and(|r| r >= 250) {
                REMOTE_SYNC_BUSY_SLOW_INTERVAL
            } else {
                REMOTE_SYNC_BUSY_FAST_INTERVAL
            }
        } else if !any_live || all_long_idle {
            REMOTE_SYNC_IDLE_INTERVAL
        } else {
            REMOTE_SYNC_RECENT_IDLE_INTERVAL
        }
    }

    /// Sync a single configured remote host (`SshMuxNodeProvider` or `PeerDaemonNodeProvider`),
    /// passing per-host `claimed_external_ids` (all active AND `Ended` sessions on that host)
    /// and routing any resulting heuristic events through `Core`'s event & notification pipeline.
    pub async fn sync_remote_host(&self, host_id: &HostId) {
        let Some(provider) = self.inner.remote_nodes.get(host_id).cloned() else {
            return;
        };
        let (display, claimed) = {
            let st = lock(&self.inner.state);
            let display = st
                .host_display_names
                .get(host_id)
                .cloned()
                .unwrap_or_else(|| provider.default_display_name().to_string());
            let claimed = collect_claimed_external_ids_for_host(&st, host_id);
            (display, claimed)
        };
        match provider.sync_with_claimed(&display, &claimed).await {
            Ok(snap) => {
                self.apply_remote_snapshot(host_id, provider.kind(), snap, false)
                    .await;
            }
            Err(err) => {
                self.apply_remote_sync_error(host_id, display, provider.as_ref(), err, false);
            }
        }
    }

    /// Sync all configured remote hosts concurrently (one-shot barrier used by tests/CLI).
    /// Offline or timed-out hosts degrade gracefully (`Degraded` -> `Offline`) without marking sessions `Ended`.
    pub async fn sync_remote_nodes(&self) {
        if self.inner.remote_nodes.is_empty() {
            return;
        }
        let host_ids: Vec<HostId> = self.inner.remote_nodes.keys().cloned().collect();
        let mut handles = Vec::with_capacity(host_ids.len());
        for hid in host_ids {
            let core = self.clone();
            handles.push(tokio::spawn(async move {
                core.sync_remote_host(&hid).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    async fn apply_remote_snapshot(
        &self,
        hid: &HostId,
        kind: HostKind,
        mut snap: NodeSnapshot,
        force_publish_host: bool,
    ) {
        let now_ms = id::now_ms();
        let mut updated_sessions = Vec::new();
        let mut changed = false;
        let updated_host;
        {
            let mut st = lock(&self.inner.state);
            let display = st
                .host_display_names
                .get(hid)
                .cloned()
                .unwrap_or_else(|| snap.host.display_name.clone());
            snap.host.display_name = display.clone();

            let live_ids: HashSet<SessionId> =
                snap.sessions.iter().map(|s| s.session_id.clone()).collect();
            for mut s in snap.sessions {
                s.host_display_name = Some(display.clone());
                // Zero-install SSH discovery may stamp fresh poll timestamps on subagents or
                // re-parsed metrics; preserve stable timestamps when meaningful fields did not
                // change so we never trigger endless fsync/SSE floods or reset idle timers.
                if kind == HostKind::Ssh {
                    if let Some(prev) = st.sessions.get(&s.session_id) {
                        let same_metrics =
                            metrics::metrics_meaningfully_equal(&prev.metrics, &s.metrics);
                        if same_metrics {
                            s.metrics = prev.metrics.clone();
                        }
                        if prev.state == s.state {
                            s.state_since_ms = prev.state_since_ms;
                            let same_meta = prev.agent == s.agent
                                && prev.pane == s.pane
                                && prev.cwd == s.cwd
                                && prev.git_root == s.git_root
                                && prev.model == s.model
                                && prev.effort == s.effort
                                && prev.tmux_target == s.tmux_target
                                && prev.tmux_label == s.tmux_label
                                && prev.host_display_name == s.host_display_name
                                && prev.mux_kind == s.mux_kind
                                && prev.transcript_path == s.transcript_path
                                && prev.last_error == s.last_error
                                && prev.subagent_count == s.subagent_count
                                && prev.active_subagents == s.active_subagents
                                && same_metrics;
                            if same_meta {
                                s.last_event_ms = prev.last_event_ms;
                            }
                        }
                    }
                }
                if st.sessions.get(&s.session_id) != Some(&s) {
                    st.sessions.insert(s.session_id.clone(), s.clone());
                    updated_sessions.push(s);
                    changed = true;
                }
            }

            // Transition previously live remote sessions that disappeared from a HEALTHY host.
            // Already-`Ended` sessions are left untouched so closed transcripts remain in
            // `claimed_external_ids` and SSE does not emit duplicate `Ended` tombstones.
            let gone: Vec<SessionId> = st
                .sessions
                .iter()
                .filter(|(sid, s)| {
                    s.effective_host_id() == hid.as_str()
                        && s.state != SessionState::Ended
                        && !live_ids.contains(*sid)
                })
                .map(|(sid, _)| sid.clone())
                .collect();
            for sid in gone {
                let keep_for_claim = st.sessions.get(&sid).is_some_and(|s| {
                    !s.is_subagent && (!s.discovered_only || s.transcript_path.is_some())
                });
                if keep_for_claim {
                    if let Some(old) = st.sessions.get_mut(&sid) {
                        old.state = SessionState::Ended;
                        old.state_since_ms = now_ms;
                        old.last_event_ms = now_ms;
                        old.last_error = None;
                        old.active_subagents = 0;
                        updated_sessions.push(old.clone());
                        changed = true;
                    }
                } else if let Some(mut old) = st.sessions.remove(&sid) {
                    old.state = SessionState::Ended;
                    old.state_since_ms = now_ms;
                    old.last_event_ms = now_ms;
                    old.last_error = None;
                    old.active_subagents = 0;
                    updated_sessions.push(old);
                    changed = true;
                }
            }

            snap.host.session_count = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.state != SessionState::Ended
                        && s.effective_host_id() == hid.as_str()
                })
                .count();
            let prev_host = st.hosts.get(hid);
            let host_diff = force_publish_host
                || prev_host.is_none_or(|p| {
                    p.status != snap.host.status
                        || p.error != snap.host.error
                        || p.active_endpoint != snap.host.active_endpoint
                        || p.display_name != snap.host.display_name
                        || p.session_count != snap.host.session_count
                        || p.rtt_ms != snap.host.rtt_ms
                        || p.next_retry_ms != snap.host.next_retry_ms
                });
            updated_host = host_diff.then(|| snap.host.clone());
            st.hosts.insert(hid.clone(), snap.host);
        }

        if changed {
            self.mark_dirty();
            for s in updated_sessions {
                self.publish_session(s);
            }
            self.refresh_attention();
        }
        if let Some(host) = updated_host {
            self.publish_host(host);
        }

        if !snap.events.is_empty() {
            for ev in snap.events {
                self.publish(|seq| DaemonEvent::Event {
                    seq,
                    event: ev.clone(),
                });
                let sess_opt = lock(&self.inner.state)
                    .sessions
                    .get(&ev.session_id)
                    .cloned();
                if let Some(sess) = sess_opt {
                    self.on_session_event(&sess, &ev, None, sess.is_subagent)
                        .await;
                }
            }
            self.refresh_attention();
        }
    }

    fn apply_remote_sync_error(
        &self,
        hid: &HostId,
        display: String,
        provider: &dyn NodeProvider,
        err: String,
        force_publish_host: bool,
    ) {
        // Graceful degradation: update circuit status (`Degraded` -> `Offline`) & backoff,
        // NEVER mark existing remote sessions `Ended`!
        log::debug!("remote host {hid} sync degraded: {err}");
        let cb_status = provider.circuit_status();
        let status = if cb_status == HostStatus::Online {
            HostStatus::Degraded
        } else {
            cb_status
        };
        let next_retry_ms = provider.next_retry_ms();
        let rtt_ms = provider.rtt_ms();
        let kind = provider.kind();

        let publish_rec = {
            let mut st = lock(&self.inner.state);
            let prev = st.hosts.get(hid).cloned();
            let display_name = st.host_display_names.get(hid).cloned().unwrap_or(display);
            let count = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.state != SessionState::Ended
                        && s.effective_host_id() == hid.as_str()
                })
                .count();
            let degraded = HostRecord {
                id: hid.clone(),
                display_name,
                kind,
                status,
                active_endpoint: prev.as_ref().and_then(|p| p.active_endpoint.clone()),
                last_seen_ms: prev.as_ref().and_then(|p| p.last_seen_ms),
                error: Some(err),
                session_count: count,
                rtt_ms: rtt_ms.or_else(|| prev.as_ref().and_then(|p| p.rtt_ms)),
                next_retry_ms,
            };
            let diff = force_publish_host
                || prev.as_ref().is_none_or(|p| {
                    p.status != degraded.status
                        || p.error != degraded.error
                        || p.session_count != degraded.session_count
                        || p.rtt_ms != degraded.rtt_ms
                        || p.next_retry_ms != degraded.next_retry_ms
                        || p.display_name != degraded.display_name
                });
            st.hosts.insert(hid.clone(), degraded.clone());
            diff.then_some(degraded)
        };
        if let Some(host) = publish_rec {
            self.publish_host(host);
        }
    }

    /// Reset a remote host's circuit breaker and execute an immediate probe (`POST /api/hosts/{id}/probe`).
    pub async fn probe_host(&self, host_id: &str) -> Result<HostProbeOutcome, ApiError> {
        let trimmed = host_id.trim();
        if trimmed.is_empty() || trimmed == LOCAL_HOST_ID {
            return Err(ApiError::not_found(format!(
                "unknown remote host {trimmed:?}"
            )));
        }
        let hid = HostId::new(trimmed);
        let provider = self
            .inner
            .remote_nodes
            .get(&hid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown remote host {trimmed}")))?;

        let mut outcome = provider.probe_now().await?;
        if let Some(snap) = outcome.snapshot.take() {
            self.apply_remote_snapshot(&hid, provider.kind(), snap, true)
                .await;
        } else {
            let display = {
                let st = lock(&self.inner.state);
                st.host_display_names
                    .get(&hid)
                    .cloned()
                    .unwrap_or_else(|| provider.default_display_name().to_string())
            };
            let err = outcome
                .error
                .clone()
                .unwrap_or_else(|| format!("host {hid} probe failed"));
            self.apply_remote_sync_error(&hid, display, provider.as_ref(), err, true);
        }

        if let Some(wake) = self.inner.remote_wake.get(&hid) {
            wake.notify_one();
        }
        Ok(outcome)
    }

    /// Best-effort non-blocking upload pre-staging onto a configured remote host.
    /// Logs warnings explicitly via `log::warn!`; never panics or swallows silently.
    pub async fn stage_upload_for_host(&self, host_id: &str, name: String, local_path: PathBuf) {
        let trimmed = host_id.trim();
        if trimmed.is_empty() || trimmed == LOCAL_HOST_ID {
            return;
        }
        let hid = HostId::new(trimmed);
        let Some(provider) = self.inner.remote_nodes.get(&hid).cloned() else {
            log::warn!("stage_upload_for_host: unknown remote host {trimmed:?}");
            return;
        };
        let files = vec![(name.clone(), local_path.clone())];
        if let Err(e) = provider.stage_input_files(&files).await {
            log::warn!(
                "background upload pre-stage to host {hid} failed for {name} ({}): {}",
                local_path.display(),
                e.message
            );
        }
    }

    /// State snapshot as JSON (includes `feishu_muted_until_ms`, `hosts`, and `terminal_font`).
    pub async fn state_json(&self) -> serde_json::Value {
        let snap = self.snapshot();
        let hosts = self.hosts();
        let has_user_font = crate::web::has_user_term_font().await;
        let mut v = serde_json::to_value(&snap).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            let now_ms = id::now_ms();
            let mutes = self.mute_snapshot();
            let feishu_eff =
                mutes.effective_mute_until(crate::notify::DEFAULT_FEISHU_NOTIFIER_ID, now_ms);
            let notifiers = self.inner.notify_router.list_statuses(&mutes, now_ms);
            obj.insert(
                "feishu_muted_until_ms".into(),
                feishu_eff
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "global_mute_until_ms".into(),
                mutes
                    .global_mute_until_ms
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "channel_mutes".into(),
                serde_json::to_value(&mutes.channel_mutes)
                    .unwrap_or_else(|_| serde_json::json!({})),
            );
            obj.insert(
                "any_muted".into(),
                serde_json::Value::Bool(mutes.any_muted(now_ms)),
            );
            obj.insert(
                "notifiers".into(),
                serde_json::to_value(&notifiers).unwrap_or(serde_json::Value::Array(Vec::new())),
            );
            obj.insert(
                "hosts".into(),
                serde_json::to_value(&hosts).unwrap_or(serde_json::Value::Array(Vec::new())),
            );
            if !self.inner.cfg.web.terminal_font.trim().is_empty() {
                obj.insert(
                    "terminal_font".into(),
                    serde_json::Value::String(self.inner.cfg.web.terminal_font.trim().to_string()),
                );
            }
            obj.insert(
                "has_user_term_font".into(),
                serde_json::Value::Bool(has_user_font),
            );
            obj.insert(
                "lang".into(),
                serde_json::Value::String(self.inner.cfg.web.effective_lang().to_string()),
            );
        }
        v
    }

    pub fn snapshot(&self) -> StateSnapshot {
        let st = lock(&self.inner.state);
        StateSnapshot {
            v: PROTO_VERSION,
            version: VERSION.to_string(),
            started_ms: self.inner.started_ms,
            sessions: st.sessions.values().cloned().collect(),
            pipelines: st.pipelines.values().cloned().collect(),
            attention: compute_attention(&st),
            event_seq: self.inner.seq.load(Ordering::SeqCst),
        }
    }

    pub fn request_shutdown(&self) {
        self.inner.shutdown.notify_waiters();
        self.inner.shutdown.notify_one();
    }

    // ------------------------------------------------------------------
    // Internals shared by submodules
    // ------------------------------------------------------------------

    pub(crate) fn mark_dirty(&self) {
        self.inner.dirty.store(true, Ordering::SeqCst);
    }

    /// Assign a seq, buffer for replay and broadcast.
    pub(crate) fn publish(&self, make: impl FnOnce(u64) -> DaemonEvent) {
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let ev = make(seq);
        {
            let mut buf = lock(&self.inner.replay);
            if buf.len() >= SSE_REPLAY_CAPACITY {
                buf.pop_front();
            }
            buf.push_back(ev.clone());
        }
        // Err only means "no subscribers right now", which is a normal state.
        let _no_subscribers_is_fine = self.inner.events.send(ev);
    }

    pub(crate) fn publish_session(&self, s: SessionRecord) {
        self.publish(|seq| DaemonEvent::Session { seq, session: s });
    }

    pub(crate) fn publish_host(&self, host: HostRecord) {
        self.publish(|seq| DaemonEvent::Host { seq, host });
    }

    pub(crate) fn publish_pipeline(&self, p: PipelineSpec) {
        self.persist_pipeline(&p);
        self.publish(|seq| DaemonEvent::Pipeline { seq, pipeline: p });
    }

    /// Recompute attention; broadcast when it changed.
    pub(crate) fn refresh_attention(&self) {
        let items = compute_attention(&lock(&self.inner.state));
        let changed = {
            let mut last = lock(&self.inner.last_attention);
            if *last != items {
                *last = items.clone();
                true
            } else {
                false
            }
        };
        if changed {
            self.publish(|seq| DaemonEvent::Attention { seq, items });
        }
    }

    /// Route & deliver an outbound notification through `NotifyRouter` (opt-in pluggable channels
    /// and legacy Card 2.0 `feishu-default`), enforcing Split Ownership, global/per-channel mutes,
    /// and `.starfane/team.toml` routes.
    pub(crate) fn notify(&self, notice: Notice) {
        let now_ms = id::now_ms();
        let mutes = self.mute_snapshot();
        if mutes.is_global_muted(now_ms) {
            return;
        }
        let (node, is_local_host, cwd, git_root, agent) = {
            let st = lock(&self.inner.state);
            match notice
                .session_id
                .as_ref()
                .and_then(|sid| st.sessions.get(sid))
            {
                Some(s) => (
                    s.effective_host_id().to_string(),
                    s.is_local_host(),
                    s.cwd.clone(),
                    s.git_root.clone(),
                    Some(s.agent),
                ),
                None => (LOCAL_HOST_ID.to_string(), true, None, None, None),
            }
        };
        let node_ownership = if is_local_host {
            crate::config::NotifyOwnership::NodeLocal
        } else {
            self.inner
                .remote_nodes
                .get(&HostId::new(&node))
                .map(|p| p.notify_ownership())
                .unwrap_or(crate::config::NotifyOwnership::NodeLocal)
        };
        let team_routes = if is_local_host {
            git_root
                .as_deref()
                .or(cwd.as_deref())
                .and_then(|root| TeamConfig::load(Path::new(root)).ok())
                .map(|t| t.notify_routes)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let ctx = NotifyRouteContext::from_notice(
            &notice,
            node,
            is_local_host,
            node_ownership,
            cwd,
            git_root,
            agent,
        );

        // 1. Fan out to opt-in pluggable notifiers (skipping any individually muted channels).
        if self.inner.notify_router.has_pluggable_notifiers() {
            let core = self.clone();
            let notice_clone = notice.clone();
            let ctx_clone = ctx.clone();
            let tr_clone = team_routes.clone();
            let mutes_clone = mutes.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = core
                        .inner
                        .notify_router
                        .dispatch_pluggable_with_mutes(
                            &notice_clone,
                            &ctx_clone,
                            &tr_clone,
                            &mutes_clone,
                            now_ms,
                        )
                        .await;
                });
            }
        }

        // 2. Deliver primary interactive Feishu Card 2.0 when unmuted, enabled, and matched by router.
        if mutes.is_channel_muted(crate::notify::DEFAULT_FEISHU_NOTIFIER_ID, now_ms)
            || !self
                .inner
                .notify_router
                .should_deliver_primary_feishu(&ctx, &team_routes)
        {
            return;
        }
        let decision = {
            let mut st = lock(&self.inner.state);
            check_feishu_rate_limit(&mut st, now_ms)
        };
        let to_send = match decision {
            FeishuRateDecision::Allow => notice,
            FeishuRateDecision::EmitThrottleSummary => {
                log::warn!(
                    "feishu: burst limit ({MAX_FEISHU_NOTICES_PER_WINDOW} / {}s) hit at notice {}; collapsing stream",
                    FEISHU_RATE_WINDOW_MS / 1000,
                    notice.key
                );
                Notice {
                    key: format!("rate-limit-{}", now_ms / 1000),
                    kind: NoticeKind::TurnComplete,
                    title: "⚠️ 飞书通知限流保护已触发".into(),
                    body_md: format!(
                        "1 分钟内连续触发超过 {MAX_FEISHU_NOTICES_PER_WINDOW} 条通知，后续同窗口通知已自动折叠。请打开 Starfane Web 控制台查看完整会话状态。\n"
                    ),
                    pipeline_id: None,
                    session_id: None,
                    review_id: None,
                    actions: Vec::new(),
                    binding_form: Vec::new(),
                    reply_session: None,
                }
            }
            FeishuRateDecision::Drop => {
                log::warn!("feishu: rate-limited notice {}", notice.key);
                return;
            }
        };
        if self.inner.notice_tx.send(to_send).is_err() {
            log::warn!("feishu notice dropped: adapter not running");
        }
    }

    pub(crate) fn register_ack(&self, session: &SessionId) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        lock(&self.inner.acks)
            .entry(session.clone())
            .or_default()
            .push(tx);
        rx
    }

    pub(crate) fn resolve_acks(&self, session: &SessionId) {
        if let Some(waiters) = lock(&self.inner.acks).remove(session) {
            for w in waiters {
                // Receiver may have timed out already; nothing to do then.
                let _receiver_gone_is_fine = w.send(());
            }
        }
    }

    fn persist_pipeline(&self, p: &PipelineSpec) {
        let path = paths::project_dir(Path::new(&p.project.root))
            .join("pipelines")
            .join(format!("{}.toml", p.id));
        match toml::to_string(p) {
            Ok(text) => {
                if let Err(e) = crate::store::atomic_write(&path, text.as_bytes()) {
                    log::warn!("write {}: {e}", path.display());
                }
            }
            Err(e) => log::error!("serialize pipeline {}: {e}", p.id),
        }
    }

    /// Clone persisted state in microseconds under `Mutex<State>`, then write + fsync on `spawn_blocking`.
    pub(crate) async fn flush_snapshot_async(&self) {
        if !self.inner.dirty.load(Ordering::SeqCst) {
            return;
        }
        if self
            .inner
            .snapshot_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.inner.dirty.store(false, Ordering::SeqCst);
        let snap = lock(&self.inner.state).persistent_snapshot();
        let store = self.inner.store.clone();
        let res = tokio::task::spawn_blocking(move || store.save_snapshot(&snap)).await;
        self.inner.snapshot_in_flight.store(false, Ordering::SeqCst);
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("snapshot write failed: {e}");
                self.inner.dirty.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                log::error!("snapshot task failed: {e}");
                self.inner.dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    fn flush_snapshot_sync(&self) {
        if !self.inner.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let snap = lock(&self.inner.state).persistent_snapshot();
        if let Err(e) = self.inner.store.save_snapshot(&snap) {
            log::error!("snapshot write failed: {e}");
            self.inner.dirty.store(true, Ordering::SeqCst);
        }
    }

    async fn drain_spool(&self) {
        let store = self.inner.store.clone();
        let res = tokio::task::spawn_blocking(move || store.drain_spool()).await;
        match res {
            Ok((reqs, errors)) => {
                for e in errors {
                    log::warn!("spool: {e}");
                }
                for r in reqs {
                    self.ingest(r).await;
                }
            }
            Err(e) => log::error!("spool drain task failed: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // Command dispatch
    // ------------------------------------------------------------------

    pub async fn handle(&self, cmd: DaemonCommand) -> Result<serde_json::Value, ApiError> {
        use DaemonCommand as C;
        let r = match cmd {
            C::Ping => Ok(serde_json::json!({"pong": true, "version": VERSION})),
            C::GetState => Ok(self.state_json().await),
            C::ListSessions {
                project,
                include_subagents,
                include_ended,
            } => to_json(self.list_sessions(project.as_deref(), include_subagents, include_ended)),
            C::AckSession { session_id } => self.ack_session(&session_id).and_then(to_json),
            C::ResolveIdentity {
                agent,
                cwd,
                pane,
                bind_token,
                session_hint,
                pid_chain,
            } => to_json(
                self.resolve_identity_with_pid_tree(
                    agent,
                    cwd,
                    pane,
                    bind_token,
                    session_hint,
                    pid_chain,
                )
                .await,
            ),
            C::ReadInbox {
                session_id,
                mark_delivered,
            } => self
                .read_inbox(&session_id, mark_delivered)
                .and_then(to_json),
            C::CreateHandoff { draft } => self.create_handoff(draft).await,
            C::AckHandoff {
                session_id,
                handoff_id,
                status,
                note,
            } => self.ack_handoff(&session_id, &handoff_id, status, note),
            C::SubmitArtifact {
                session_id,
                artifact,
            } => self.submit_artifact(&session_id, artifact).await,
            C::SubmitFalsifierVerdict {
                session_id,
                submission,
            } => self.submit_falsifier(&session_id, submission).await,
            C::ReportProgress {
                session_id,
                text,
                notify_human,
            } => self.report_progress(&session_id, &text, notify_human),
            C::ParseUtterance {
                text,
                playbook,
                project,
            } => self.parse_utterance(&text, playbook, project.as_deref()),
            C::CreatePipeline { req } => self.create_pipeline(req).await,
            C::ConfirmPipeline {
                pipeline_id,
                overrides,
            } => self.confirm_pipeline(&pipeline_id, overrides).await,
            C::PipelineAction {
                pipeline_id,
                action,
            } => self.pipeline_action(&pipeline_id, action).await,
            C::ListPipelines {
                project,
                include_done,
            } => to_json(self.list_pipelines(project.as_deref(), include_done)),
            C::GetPipeline { pipeline_id } => lock(&self.inner.state)
                .pipelines
                .get(&pipeline_id)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("pipeline {pipeline_id}")))
                .and_then(to_json),
            C::GetArtifact { id } => lock(&self.inner.state)
                .artifacts
                .get(&id)
                .map(|a| a.payload.clone())
                .ok_or_else(|| ApiError::not_found(format!("artifact {id}")))
                .and_then(to_json),
            C::ListHandoffs {
                session_id,
                pipeline_id,
            } => to_json(self.list_handoffs(session_id.as_ref(), pipeline_id.as_ref())),
            C::GetReview { review_id } => lock(&self.inner.state)
                .reviews
                .get(&review_id)
                .map(|r| r.record.clone())
                .ok_or_else(|| ApiError::not_found(format!("review {review_id}")))
                .and_then(to_json),
            C::ListReviews { pipeline_id } => to_json(self.list_reviews(pipeline_id.as_ref())),
            C::ReviewFeedback {
                review_id,
                finding_id,
                label,
                note,
                by,
            } => self.review_feedback(&review_id, &finding_id, label, note, by),
            C::HumanVerdict {
                review_id,
                finding_id,
                verdict,
                note,
                by,
            } => {
                self.human_verdict(&review_id, &finding_id, verdict, note, by)
                    .await
            }
            C::GetPrecision { agent, model } => to_json(self.precision(agent, model.as_deref())),
            C::Dispatch {
                target,
                text,
                attachments,
                allow,
                interactive,
            } => self
                .dispatch(target, text, attachments, allow, interactive)
                .await
                .and_then(to_json),
            C::SpawnSession {
                agent,
                host_id,
                model,
                effort,
                project_root,
                tmux_session,
                name,
                prompt,
                from_session: _,
            } => {
                self.spawn_direct_session(
                    agent,
                    host_id,
                    project_root,
                    tmux_session,
                    model,
                    effort,
                    prompt,
                    name,
                )
                .await
            }
            C::FeishuTest { text } => self.feishu_test(&text).await,
            C::TmuxBackup { force } => self.backup_tmux(force).await.and_then(to_json),
            C::TmuxRestore {
                file,
                restore_agents,
                dry_run,
            } => self
                .restore_tmux(file, restore_agents, dry_run)
                .await
                .and_then(to_json),
            C::ListTmuxBackups => self.list_tmux_backups().await.and_then(to_json),
            C::Shutdown => {
                self.request_shutdown();
                Ok(serde_json::json!({}))
            }
        };
        self.refresh_attention();
        r
    }

    async fn feishu_test(&self, text: &str) -> Result<serde_json::Value, ApiError> {
        let sender = match self.inner.feishu.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let sender = sender.ok_or_else(|| {
            ApiError::new(
                ErrorCode::Unavailable,
                "feishu adapter is not running (set [feishu] enabled = true)",
            )
        })?;
        let id = sender.send_test(text).await?;
        Ok(serde_json::json!({"message_id": id}))
    }
}

pub(crate) fn collect_claimed_external_ids_for_host(
    st: &State,
    host_id: &HostId,
) -> HashSet<String> {
    let prefix = format!("{}:", host_id.as_str());
    let mut claimed = HashSet::new();
    for s in st.sessions.values() {
        if s.is_subagent || s.effective_host_id() != host_id.as_str() {
            continue;
        }
        if let Some(tp) = s.transcript_path.as_deref() {
            if let Some(eid) =
                metrics::extract_session_id_from_transcript_path(s.agent, Path::new(tp))
            {
                claimed.insert(eid);
            }
        }
        let local_sid = s
            .session_id
            .as_str()
            .strip_prefix(&prefix)
            .unwrap_or(s.session_id.as_str());
        if !s.discovered_only && !local_sid.starts_with("tmux-") && !local_sid.is_empty() {
            claimed.insert(local_sid.to_string());
        }
    }
    claimed
}

pub(crate) fn to_json<T: serde::Serialize>(v: T) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::internal(format!("serialize: {e}")))
}

/// Derive "needs you" items from state.
pub(crate) fn compute_attention(st: &State) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for p in st.pipelines.values() {
        match p.status {
            PipelineStatus::AwaitingBindingConfirm => items.push(AttentionItem {
                kind: attention::BINDING_CONFIRM.into(),
                title: format!("确认绑定：{}", p.goal),
                pipeline_id: Some(p.id.clone()),
                session_id: None,
                review_id: None,
                since_ms: p.updated_ms,
            }),
            PipelineStatus::Cancelled | PipelineStatus::Done | PipelineStatus::Failed => continue,
            _ => {}
        }
        for s in &p.stages {
            if s.status != StageStatus::AwaitingHuman {
                continue;
            }
            let (kind, title) = match s.id {
                StageKind::Plan => (attention::PLAN_APPROVAL, format!("批准计划：{}", p.goal)),
                StageKind::Deep | StageKind::Triage => (
                    attention::TRIAGE_READY,
                    format!("快筛结论待处理：{}", p.goal),
                ),
                _ => (
                    attention::BLOCKED,
                    format!("{} 需要人工处理：{}", s.id, p.goal),
                ),
            };
            items.push(AttentionItem {
                kind: kind.into(),
                title,
                pipeline_id: Some(p.id.clone()),
                session_id: s.session_id.clone(),
                review_id: None,
                since_ms: s.started_ms.unwrap_or(p.updated_ms),
            });
        }
    }
    for s in st.sessions.values() {
        if s.is_subagent {
            continue;
        }
        let label = match s.state {
            SessionState::NeedsPermission => "等待审批",
            SessionState::NeedsInput => "等待输入",
            SessionState::Error => "出错",
            _ => continue,
        };
        items.push(AttentionItem {
            kind: attention::BLOCKED.into(),
            title: format!(
                "{}@{} {}",
                s.agent,
                s.pane.as_ref().map(|p| p.as_str()).unwrap_or("?"),
                label
            ),
            pipeline_id: s.stage.as_ref().map(|r| r.pipeline_id.clone()),
            session_id: Some(s.session_id.clone()),
            review_id: None,
            since_ms: s.state_since_ms,
        });
    }
    for r in st.reviews.values() {
        let pending = r.record.findings.iter().any(|f| {
            f.disposition == Disposition::Delivered
                && matches!(
                    f.falsifier.as_ref().map(|s| s.verdict),
                    Some(FalsifierVerdict::NeedsHuman)
                )
                && f.feedback.is_none()
        });
        if pending {
            items.push(AttentionItem {
                kind: attention::NEEDS_HUMAN_FINDING.into(),
                title: format!(
                    "Review {} 有待人工判断的发现",
                    util::short_id(r.record.id.as_str())
                ),
                pipeline_id: r.pipeline_id.clone(),
                session_id: None,
                review_id: Some(r.record.id.clone()),
                since_ms: r.record.report.header.created_ms,
            });
        }
    }
    items.extend(st.unconfirmed.iter().cloned());
    items.sort_by(|a, b| a.since_ms.cmp(&b.since_ms).then(a.title.cmp(&b.title)));
    items
}

// ----------------------------------------------------------------------
// Daemon lifecycle
// ----------------------------------------------------------------------

pub(crate) async fn run(cfg: DaemonConfig) -> Result<(), DaemonError> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    if std::os::unix::net::UnixStream::connect(&cfg.socket).is_ok() {
        return Err(DaemonError::AlreadyRunning(
            cfg.socket.display().to_string(),
        ));
    }
    if cfg.socket.exists() {
        std::fs::remove_file(&cfg.socket)?;
    }
    let core = Core::new(&cfg)?;
    let listener = tokio::net::UnixListener::bind(&cfg.socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let pid_path = cfg.state_dir.join("starfaned.pid");
    write_private(&pid_path, std::process::id().to_string().as_bytes())?;
    write_private(
        &cfg.state_dir.join("web.token"),
        core.web_token().as_bytes(),
    )?;
    log::info!(
        "starfaned {} listening on {} (web {}:{})",
        VERSION,
        cfg.socket.display(),
        core.config().web.bind,
        core.config().web.port
    );

    // Startup auto-restore MUST run before initial scan or periodic backup so an empty tmux
    // server after reboot never overwrites `latest.json`.
    core.maybe_auto_restore_on_start().await;

    // Immediate initial scan so already-running tmux panes are visible on the first request.
    core.sync_discovered_panes().await;

    tokio::spawn(crate::uds::serve(core.clone(), listener));
    {
        let c = core.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::web::serve(c).await {
                log::error!("web server stopped: {e}");
            }
        });
    }
    tokio::spawn(crate::lark::run(core.clone()));
    if core.config().tmux.backup.enabled {
        let c = core.clone();
        let secs = core.config().tmux.backup.interval_secs.max(10);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(secs));
            loop {
                tick.tick().await;
                match c.backup_tmux(false).await {
                    Ok(out) if out.saved => {
                        log::info!(
                            "tmux backup saved: sessions={} windows={} panes={} resumable={}",
                            out.session_count,
                            out.window_count,
                            out.pane_count,
                            out.resumable_agent_count
                        );
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("tmux backup failed: {}", e.message),
                }
            }
        });
    }
    {
        let c = core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(DISCOVER_INTERVAL);
            tick.tick().await; // skip immediate tick (already scanned above)
            loop {
                tick.tick().await;
                c.sync_discovered_panes().await;
            }
        });
    }
    if !core.inner.remote_nodes.is_empty() {
        core.spawn_remote_host_supervisors();
    }
    {
        let c = core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SNAPSHOT_INTERVAL);
            loop {
                tick.tick().await;
                c.flush_snapshot_async().await;
            }
        });
    }
    {
        let c = core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SPOOL_INTERVAL);
            loop {
                tick.tick().await;
                c.drain_spool().await;
            }
        });
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // A daemon auto-started from a hook shares the agent's process group; ignore
    // SIGHUP so closing that terminal does not take the daemon down.
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::spawn(async move {
        while hup.recv().await.is_some() {
            log::info!("SIGHUP ignored");
        }
    });
    tokio::select! {
        _ = core.inner.shutdown.notified() => log::info!("shutdown requested"),
        _ = tokio::signal::ctrl_c() => log::info!("SIGINT received"),
        _ = term.recv() => log::info!("SIGTERM received"),
    }
    core.mark_dirty();
    core.flush_snapshot_sync();
    if core.config().tmux.backup.enabled {
        if let Err(e) = core.backup_tmux(false).await {
            log::warn!("shutdown tmux backup failed: {}", e.message);
        }
    }
    for p in [&cfg.socket, &pid_path] {
        if let Err(e) = std::fs::remove_file(p) {
            log::warn!("cleanup {}: {e}", p.display());
        }
    }
    Ok(())
}
