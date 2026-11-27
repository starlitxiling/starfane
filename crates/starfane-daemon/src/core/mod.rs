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
    build_remote_providers_with_transcript_cache, HostProbeOutcome, HostPushSignal, LocalMuxPool,
    NodeProvider, NodeSnapshot,
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
const REMOTE_PEER_SSE_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const REMOTE_IDLE_LONG_MS: u64 = 60_000;
const REMOTE_BACKOFF_SLACK_MS: u64 = 25;
const REMOTE_RETRY_MAX_SLEEP_MS: u64 = 65_000;
const EVENT_CHANNEL: usize = 1024;
const FEISHU_RATE_WINDOW_MS: u64 = 60_000;
const MAX_FEISHU_NOTICES_PER_WINDOW: usize = 6;
/// Maximum serialized byte size of inline `appended_messages` in `DaemonEvent::Output` (32 KiB).
pub(crate) const MAX_INLINE_OUTPUT_BYTES: usize = 32 * 1024;
/// Maximum byte length of inline `live_tail` in `DaemonEvent::Output` (8 KiB).
pub(crate) const MAX_INLINE_LIVE_TAIL_BYTES: usize = 8 * 1024;
/// Maximum number of messages serialized inline in a single `DaemonEvent::Output` push.
pub(crate) const MAX_INLINE_OUTPUT_MESSAGES: usize = 64;

/// Off-state IO caches (SQLite, `lsof`, transcript telemetry) kept outside `Mutex<State>`
/// so blocking disk/subprocess queries never contend with control-plane reads/writes.
#[derive(Debug, Default)]
pub(crate) struct TelemetryCaches {
    pub agy_step_timings_cache: metrics::AgyStepTimingCache,
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
                sync_ms: None,
                next_retry_ms: None,
                retry_paused: false,
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
                    sync_ms: None,
                    next_retry_ms: None,
                    retry_paused: false,
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

    /// Send an explicit human-clicked allowlisted interactive key (e.g. `Enter`, `Up`, `Down`,
    /// `Escape`, `1`, `y`, `n`, `C-c`) to a live local or federated remote session pane.
    ///
    /// Serializes with text injection via the per-`(Core, Host, Pane)` lock and triggers a
    /// rapid post-key discovery/dirty-pane refresh (~120ms settle) so resolved approval/input
    /// prompts clear the pending attention queue promptly.
    pub async fn send_session_key(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<serde_json::Value, ApiError> {
        starfane_tmux::validate_interactive_key(key)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;

        let (pane, host_id) = {
            let st = lock(&self.inner.state);
            let s = st
                .sessions
                .get(session_id)
                .ok_or_else(|| ApiError::not_found(format!("session {session_id}")))?;
            if s.is_subagent {
                return Err(ApiError::bad_request(
                    "cannot send interactive key to subagent or session without terminal pane",
                ));
            }
            let Some(pane) = s.pane.clone() else {
                return Err(ApiError::bad_request(
                    "cannot send interactive key to subagent or session without terminal pane",
                ));
            };
            (pane, HostId::new(s.effective_host_id()))
        };

        let core_key = Arc::as_ptr(&self.inner) as usize;
        let lock_pane = if host_id.as_str() == LOCAL_HOST_ID {
            pane.clone()
        } else {
            qualify_pane_idempotent(&host_id, &pane)
        };
        let pane_lock = delivery::acquire_pane_inject_lock(core_key, &lock_pane);
        let pane_guard = pane_lock.lock().await;

        let refresh_local_pane = if host_id.as_str() != LOCAL_HOST_ID {
            let provider = self
                .inner
                .remote_nodes
                .get(&host_id)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {host_id}")))?;
            let (_, local_pane_str) = starfane_tmux::split_host_prefix(pane.as_str());
            let local_pane = PaneId::new(local_pane_str);
            let remote_sid = session_id
                .as_str()
                .strip_prefix(&format!("{}:", host_id.as_str()))
                .map(SessionId::new)
                .unwrap_or_else(|| session_id.clone());
            provider.send_key(&remote_sid, &local_pane, key).await?;
            Some(local_pane)
        } else {
            let core = self.clone();
            let pane_for_mux = pane.clone();
            let key_owned = key.to_string();
            tokio::task::spawn_blocking(move || core.inner.mux.send_key(&pane_for_mux, &key_owned))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .map_err(|e| match e {
                    starfane_tmux::TmuxError::InvalidRequest(m) => ApiError::bad_request(m),
                    other => ApiError::new(ErrorCode::Unavailable, format!("send key: {other}")),
                })?;
            None
        };
        drop(pane_guard);

        self.audit(
            "web",
            serde_json::json!({
                "action": "send_session_key",
                "session_id": session_id,
                "pane": pane,
                "host_id": host_id,
                "key": key,
            }),
        );

        let core_refresh = self.clone();
        let refresh_host = host_id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            if refresh_host.as_str() == LOCAL_HOST_ID {
                core_refresh.sync_discovered_panes().await;
            } else {
                if let Some(lp) = refresh_local_pane {
                    core_refresh
                        .sync_remote_host_dirty_panes(&refresh_host, &[lp])
                        .await;
                }
                if let Some(wake) = core_refresh.inner.remote_wake.get(&refresh_host) {
                    wake.notify_one();
                }
            }
        });

        Ok(serde_json::json!({
            "session_id": session_id,
            "key": key,
            "sent": true,
        }))
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
                    sync_ms: None,
                    next_retry_ms: None,
                    retry_paused: false,
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
                    sync_ms: None,
                    next_retry_ms: None,
                    retry_paused: false,
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
        // Initial full sync unless auto-retry is already paused.
        if !self
            .inner
            .remote_nodes
            .get(&host_id)
            .is_some_and(|p| p.is_retry_paused())
        {
            self.sync_remote_host(&host_id).await;
        }

        let (push_tx, mut push_rx) = mpsc::unbounded_channel::<HostPushSignal>();
        let mut watcher_handle: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            let paused = self
                .inner
                .remote_nodes
                .get(&host_id)
                .is_some_and(|p| p.is_retry_paused());
            if paused {
                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                while push_rx.try_recv().is_ok() {}
                tokio::select! {
                    _ = self.inner.shutdown.notified() => return,
                    _ = wake.notified() => continue,
                }
            }
            if watcher_handle.is_none() {
                watcher_handle = self
                    .inner
                    .remote_nodes
                    .get(&host_id)
                    .and_then(|p| p.spawn_push_watcher(push_tx.clone()));
            }
            let has_push_watcher = watcher_handle.is_some();

            let sleep_dur = self.compute_remote_host_sleep_with_push(&host_id, has_push_watcher);
            let sleep_fut = tokio::time::sleep(sleep_dur);
            tokio::pin!(sleep_fut);

            loop {
                tokio::select! {
                    _ = self.inner.shutdown.notified() => {
                        if let Some(h) = watcher_handle.take() {
                            h.abort();
                        }
                        return;
                    }
                    Some(sig) = push_rx.recv() => {
                        if self
                            .inner
                            .remote_nodes
                            .get(&host_id)
                            .is_some_and(|p| p.is_retry_paused())
                        {
                            break;
                        }
                        if self.handle_remote_push_signal(&host_id, sig, &mut push_rx, &push_tx).await {
                            break;
                        }
                    }
                    _ = wake.notified() => {
                        if self
                            .inner
                            .remote_nodes
                            .get(&host_id)
                            .is_some_and(|p| p.is_retry_paused())
                        {
                            break;
                        }
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
                            self.sync_remote_host(&host_id).await;
                            break;
                        }
                    }
                    _ = &mut sleep_fut => {
                        if self
                            .inner
                            .remote_nodes
                            .get(&host_id)
                            .is_some_and(|p| p.is_retry_paused())
                        {
                            break;
                        }
                        self.sync_remote_host(&host_id).await;
                        break;
                    }
                }
            }
        }
    }

    /// Process a push signal from a host's `tmux -C` or Peer `/api/events` watcher.
    /// Returns `true` when a full sync was performed and the supervisor's reconciliation timer
    /// should reset.
    async fn handle_remote_push_signal(
        &self,
        host_id: &HostId,
        first: HostPushSignal,
        push_rx: &mut mpsc::UnboundedReceiver<HostPushSignal>,
        push_tx: &mpsc::UnboundedSender<HostPushSignal>,
    ) -> bool {
        match first {
            HostPushSignal::DirtyPanes(mut panes) => {
                // Drain any already-queued signals so bursts coalesce into 1 targeted sync.
                let mut need_full = false;
                while let Ok(next) = push_rx.try_recv() {
                    match next {
                        HostPushSignal::DirtyPanes(more) => panes.extend(more),
                        HostPushSignal::FullSync | HostPushSignal::Reconnect => {
                            need_full = true;
                        }
                        HostPushSignal::PeerSnapshot(snap) => {
                            if let Some(provider) = self.inner.remote_nodes.get(host_id).cloned() {
                                let _ = self
                                    .apply_remote_snapshot(
                                        host_id,
                                        provider.as_ref(),
                                        *snap,
                                        false,
                                        false,
                                    )
                                    .await;
                            }
                        }
                        HostPushSignal::PeerEvent(ev) => {
                            self.apply_remote_peer_event(host_id, ev).await;
                        }
                    }
                }
                if need_full {
                    self.sync_remote_host(host_id).await;
                    return true;
                }
                panes.sort();
                panes.dedup();
                if !panes.is_empty() {
                    let transitioned = self
                        .sync_remote_host_dirty_panes_with_transitions(host_id, &panes)
                        .await;
                    if !transitioned.is_empty() {
                        // Catch late transcript JSONL flushes ~900ms after a pane settles out of
                        // Working/Starting with zero loop risk (the follow-up won't re-transition).
                        let delayed_tx = push_tx.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_millis(900)) => {
                                    let _ = delayed_tx.send(HostPushSignal::DirtyPanes(transitioned));
                                }
                                _ = delayed_tx.closed() => {}
                            }
                        });
                    }
                }
                false
            }
            HostPushSignal::PeerEvent(ev) => {
                self.apply_remote_peer_event(host_id, ev).await;
                false
            }
            HostPushSignal::PeerSnapshot(snap) => {
                if let Some(provider) = self.inner.remote_nodes.get(host_id).cloned() {
                    let _ = self
                        .apply_remote_snapshot(host_id, provider.as_ref(), *snap, false, false)
                        .await;
                }
                true
            }
            HostPushSignal::FullSync | HostPushSignal::Reconnect => {
                self.sync_remote_host(host_id).await;
                true
            }
        }
    }

    fn compute_remote_host_sleep_with_push(
        &self,
        host_id: &HostId,
        has_push_watcher: bool,
    ) -> Duration {
        if has_push_watcher {
            if let Some(provider) = self.inner.remote_nodes.get(host_id) {
                if provider.kind() == HostKind::Peer
                    && provider.circuit_status() == HostStatus::Online
                {
                    return REMOTE_PEER_SSE_RECONCILE_INTERVAL;
                }
            }
        }
        self.compute_remote_host_sleep(host_id)
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
        if provider.is_retry_paused() {
            return;
        }
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
                let _ = self
                    .apply_remote_snapshot(host_id, provider.as_ref(), snap, false, false)
                    .await;
            }
            Err(err) => {
                self.apply_remote_sync_error(host_id, display, provider.as_ref(), err, false);
            }
        }
    }

    /// Execute a targeted fast sync for specific dirty panes signaled by `tmux -C` push watch,
    /// applying returned partial updates and broadcasting all `output_deltas` without reaping
    /// non-dirty sibling sessions.
    pub async fn sync_remote_host_dirty_panes(&self, host_id: &HostId, dirty_panes: &[PaneId]) {
        let _ = self
            .sync_remote_host_dirty_panes_with_transitions(host_id, dirty_panes)
            .await;
    }

    async fn sync_remote_host_dirty_panes_with_transitions(
        &self,
        host_id: &HostId,
        dirty_panes: &[PaneId],
    ) -> Vec<PaneId> {
        if dirty_panes.is_empty() {
            return Vec::new();
        }
        let Some(provider) = self.inner.remote_nodes.get(host_id).cloned() else {
            return Vec::new();
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
        match provider
            .sync_dirty_panes(&display, dirty_panes, &claimed)
            .await
        {
            Ok(snap) => {
                self.apply_remote_snapshot(host_id, provider.as_ref(), snap, false, true)
                    .await
            }
            Err(err) => {
                self.apply_remote_sync_error(host_id, display, provider.as_ref(), err, false);
                Vec::new()
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

    /// Qualify and apply a real-time SSE event pushed by a remote peer `starfaned` daemon.
    pub(crate) async fn apply_remote_peer_event(&self, hid: &HostId, ev: DaemonEvent) {
        match ev {
            DaemonEvent::Session { mut session, .. } => {
                // Import only sessions local to that peer (or already qualified to `hid`).
                if !session.is_local_host() && session.effective_host_id() != hid.as_str() {
                    return;
                }
                let (changed_session, updated_host) = {
                    let mut st = lock(&self.inner.state);
                    let display = st
                        .host_display_names
                        .get(hid)
                        .cloned()
                        .or_else(|| {
                            self.inner
                                .remote_nodes
                                .get(hid)
                                .map(|p| p.default_display_name().to_string())
                        })
                        .unwrap_or_else(|| hid.0.clone());
                    session.session_id = qualify_session_idempotent(hid, &session.session_id);
                    session.pane = session
                        .pane
                        .as_ref()
                        .map(|p| qualify_pane_idempotent(hid, p));
                    session.parent_session_id = session
                        .parent_session_id
                        .as_ref()
                        .map(|p| qualify_session_idempotent(hid, p));
                    session.host_id = Some(hid.clone());
                    session.host_display_name = Some(display);

                    let changed_session = if st.sessions.get(&session.session_id) != Some(&session)
                    {
                        st.sessions
                            .insert(session.session_id.clone(), session.clone());
                        Some(session)
                    } else {
                        None
                    };
                    let now_ms = id::now_ms();
                    let count = st
                        .sessions
                        .values()
                        .filter(|s| {
                            !s.is_subagent
                                && s.state != SessionState::Ended
                                && s.effective_host_id() == hid.as_str()
                        })
                        .count();
                    let provider_opt = self.inner.remote_nodes.get(hid);
                    let updated_host = if let Some(host) = st.hosts.get_mut(hid) {
                        let prev = host.clone();
                        host.status = HostStatus::Online;
                        host.error = None;
                        host.next_retry_ms = None;
                        host.retry_paused = false;
                        host.last_seen_ms = Some(now_ms);
                        host.session_count = count;
                        if let Some(rtt) = provider_opt.and_then(|p| p.rtt_ms()) {
                            host.rtt_ms = Some(rtt);
                        }
                        if let Some(sm) = provider_opt.and_then(|p| p.sync_ms()) {
                            host.sync_ms = Some(sm);
                        }
                        let diff = prev.status != host.status
                            || prev.error != host.error
                            || prev.session_count != host.session_count
                            || prev.rtt_ms != host.rtt_ms
                            || prev.sync_ms != host.sync_ms
                            || prev.next_retry_ms != host.next_retry_ms
                            || prev.retry_paused != host.retry_paused;
                        diff.then(|| host.clone())
                    } else {
                        None
                    };
                    (changed_session, updated_host)
                };
                if let Some(s) = changed_session {
                    self.mark_dirty();
                    self.publish_session(s);
                    self.refresh_attention();
                }
                if let Some(h) = updated_host {
                    self.publish_host(h);
                }
            }
            DaemonEvent::Output {
                session_id,
                start_seq,
                total_messages,
                appended_messages,
                live_tail,
                metrics,
                dirty,
                ..
            } => {
                if is_foreign_federated_session_id(hid, &session_id) {
                    return;
                }
                let qualified_sid = qualify_session_idempotent(hid, &session_id);
                if let Some(ref m) = metrics {
                    let mut st = lock(&self.inner.state);
                    if let Some(rec) = st.sessions.get_mut(&qualified_sid) {
                        if !metrics::metrics_meaningfully_equal(&rec.metrics, &Some(m.clone())) {
                            rec.metrics = Some(m.clone());
                        }
                    }
                }
                self.publish_output(
                    qualified_sid,
                    start_seq,
                    total_messages,
                    appended_messages,
                    live_tail,
                    metrics,
                    dirty,
                );
            }
            DaemonEvent::Event { mut event, .. } => {
                if is_foreign_federated_session_id(hid, &event.session_id) {
                    return;
                }
                event.session_id = qualify_session_idempotent(hid, &event.session_id);
                event.pane = event.pane.as_ref().map(|p| qualify_pane_idempotent(hid, p));
                self.publish(|seq| DaemonEvent::Event {
                    seq,
                    event: event.clone(),
                });
                let sess_opt = lock(&self.inner.state)
                    .sessions
                    .get(&event.session_id)
                    .cloned();
                if let Some(sess) = sess_opt {
                    self.on_session_event(&sess, &event, None, sess.is_subagent)
                        .await;
                }
                self.refresh_attention();
            }
            DaemonEvent::Attention { .. } => {
                self.refresh_attention();
            }
            DaemonEvent::Delivery {
                mut report,
                mut target,
                ..
            } => {
                report.pane = report
                    .pane
                    .as_ref()
                    .map(|p| qualify_pane_idempotent(hid, p));
                match &mut target {
                    DispatchTarget::Session { id } => {
                        *id = qualify_session_idempotent(hid, id);
                    }
                    DispatchTarget::Pane { id } => {
                        *id = qualify_pane_idempotent(hid, id);
                    }
                    DispatchTarget::Stage { .. } => {}
                }
                self.publish(|seq| DaemonEvent::Delivery {
                    seq,
                    report,
                    target,
                });
            }
            DaemonEvent::Host { .. }
            | DaemonEvent::Pipeline { .. }
            | DaemonEvent::Artifact { .. }
            | DaemonEvent::Review { .. }
            | DaemonEvent::Notifiers { .. } => {}
        }
    }

    fn broadcast_remote_output_event(&self, hid: &HostId, ev: DaemonEvent) {
        if let DaemonEvent::Output {
            session_id,
            start_seq,
            total_messages,
            appended_messages,
            live_tail,
            metrics,
            dirty,
            ..
        } = ev
        {
            if is_foreign_federated_session_id(hid, &session_id) {
                return;
            }
            let qualified_sid = qualify_session_idempotent(hid, &session_id);
            self.publish_output(
                qualified_sid,
                start_seq,
                total_messages,
                appended_messages,
                live_tail,
                metrics,
                dirty,
            );
        }
    }

    async fn apply_remote_snapshot(
        &self,
        hid: &HostId,
        provider: &dyn NodeProvider,
        mut snap: NodeSnapshot,
        force_publish_host: bool,
        is_partial: bool,
    ) -> Vec<PaneId> {
        let kind = provider.kind();
        let now_ms = id::now_ms();
        let mut updated_sessions = Vec::new();
        let mut transitioned_local_panes = Vec::new();
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
                if let Some(prev) = st.sessions.get(&s.session_id) {
                    if matches!(prev.state, SessionState::Working | SessionState::Starting)
                        && matches!(
                            s.state,
                            SessionState::Idle
                                | SessionState::NeedsPermission
                                | SessionState::NeedsInput
                                | SessionState::Error
                        )
                    {
                        if let Some(ref p) = s.pane {
                            let (_, raw_pane) = starfane_tmux::split_host_prefix(p.as_str());
                            let local_pane = PaneId::new(raw_pane);
                            if !transitioned_local_panes.contains(&local_pane) {
                                transitioned_local_panes.push(local_pane);
                            }
                        }
                    }
                }
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
            // Skipped for partial dirty-pane push snapshots so non-dirty sibling sessions remain live.
            // Already-`Ended` sessions are left untouched so closed transcripts remain in
            // `claimed_external_ids` and SSE does not emit duplicate `Ended` tombstones.
            if !is_partial {
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
            if snap.host.rtt_ms.is_none() {
                snap.host.rtt_ms = provider
                    .rtt_ms()
                    .or_else(|| prev_host.and_then(|p| p.rtt_ms));
            }
            if snap.host.sync_ms.is_none() {
                snap.host.sync_ms = provider
                    .sync_ms()
                    .or_else(|| prev_host.and_then(|p| p.sync_ms));
            }
            snap.host.retry_paused = provider.is_retry_paused();
            let host_diff = force_publish_host
                || prev_host.is_none_or(|p| {
                    p.status != snap.host.status
                        || p.error != snap.host.error
                        || p.active_endpoint != snap.host.active_endpoint
                        || p.display_name != snap.host.display_name
                        || p.session_count != snap.host.session_count
                        || p.rtt_ms != snap.host.rtt_ms
                        || p.sync_ms != snap.host.sync_ms
                        || p.next_retry_ms != snap.host.next_retry_ms
                        || p.retry_paused != snap.host.retry_paused
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

        for delta in snap.output_deltas {
            self.broadcast_remote_output_event(hid, delta);
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
        transitioned_local_panes
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
        let retry_paused = provider.is_retry_paused();
        let rtt_ms = provider.rtt_ms();
        let sync_ms = provider.sync_ms();
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
                sync_ms: sync_ms.or_else(|| prev.as_ref().and_then(|p| p.sync_ms)),
                next_retry_ms,
                retry_paused,
            };
            let diff = force_publish_host
                || prev.as_ref().is_none_or(|p| {
                    p.status != degraded.status
                        || p.error != degraded.error
                        || p.session_count != degraded.session_count
                        || p.rtt_ms != degraded.rtt_ms
                        || p.sync_ms != degraded.sync_ms
                        || p.next_retry_ms != degraded.next_retry_ms
                        || p.retry_paused != degraded.retry_paused
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
            self.apply_remote_snapshot(&hid, provider.as_ref(), snap, true, false)
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

    /// Pause automatic background reconnect retries for a remote host (`POST /api/hosts/{id}/pause`).
    pub async fn pause_host_retry(&self, host_id: &str) -> Result<HostRecord, ApiError> {
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

        provider.set_retry_paused(true);

        let rec = {
            let mut st = lock(&self.inner.state);
            let display = st
                .host_display_names
                .get(&hid)
                .cloned()
                .unwrap_or_else(|| provider.default_display_name().to_string());
            let count = st
                .sessions
                .values()
                .filter(|s| {
                    !s.is_subagent
                        && s.state != SessionState::Ended
                        && s.effective_host_id() == hid.as_str()
                })
                .count();
            let prev = st.hosts.get(&hid).cloned();
            let cb_status = provider.circuit_status();
            let status =
                prev.as_ref()
                    .map(|p| p.status)
                    .unwrap_or(if cb_status == HostStatus::Online {
                        HostStatus::Offline
                    } else {
                        cb_status
                    });
            let rec = HostRecord {
                id: hid.clone(),
                display_name: display,
                kind: provider.kind(),
                status,
                active_endpoint: prev.as_ref().and_then(|p| p.active_endpoint.clone()),
                last_seen_ms: prev.as_ref().and_then(|p| p.last_seen_ms),
                error: prev.and_then(|p| p.error),
                session_count: count,
                rtt_ms: provider.rtt_ms(),
                sync_ms: provider.sync_ms(),
                next_retry_ms: None,
                retry_paused: true,
            };
            st.hosts.insert(hid.clone(), rec.clone());
            rec
        };
        self.publish_host(rec.clone());
        if let Some(wake) = self.inner.remote_wake.get(&hid) {
            wake.notify_one();
        }
        Ok(rec)
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_output(
        &self,
        session_id: SessionId,
        start_seq: Option<usize>,
        total_messages: Option<usize>,
        appended_messages: Vec<serde_json::Value>,
        live_tail: Option<String>,
        metrics: Option<SessionMetrics>,
        dirty: bool,
    ) {
        self.publish(|seq| {
            make_output_event(
                seq,
                session_id,
                start_seq,
                total_messages,
                appended_messages,
                live_tail,
                metrics,
                dirty,
            )
        });
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

fn qualify_session_idempotent(hid: &HostId, sid: &SessionId) -> SessionId {
    let prefix = format!("{}:", hid.as_str());
    if sid.as_str().starts_with(&prefix) {
        sid.clone()
    } else {
        starfane_tmux::qualify_host_session(hid, sid)
    }
}

fn qualify_pane_idempotent(hid: &HostId, pane: &PaneId) -> PaneId {
    let prefix = format!("{}:", hid.as_str());
    if pane.as_str().starts_with(&prefix) {
        pane.clone()
    } else {
        starfane_tmux::qualify_host_pane(hid, pane)
    }
}

fn is_foreign_federated_session_id(hid: &HostId, sid: &SessionId) -> bool {
    let raw = sid.as_str();
    let own_prefix = format!("{}:", hid.as_str());
    if raw.starts_with(&own_prefix) {
        return false;
    }
    if starfane_tmux::parse_zellij_pane_id(raw).is_some() {
        return false;
    }
    if let Some((prefix, _)) = raw.split_once(':') {
        return !prefix.is_empty() && prefix != "zj" && prefix != "bind";
    }
    false
}

pub(crate) fn to_json<T: serde::Serialize>(v: T) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::internal(format!("serialize: {e}")))
}

/// Build a [`DaemonEvent::Output`] frame enforcing the 32 KiB inline payload budget for
/// `appended_messages`: if serialized messages exceed [`MAX_INLINE_OUTPUT_BYTES`], omits
/// `appended_messages` and sets `dirty = true` so the browser triggers an immediate
/// incremental `GET /api/sessions/{id}/transcript`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_output_event(
    seq: u64,
    session_id: SessionId,
    start_seq: Option<usize>,
    total_messages: Option<usize>,
    appended_messages: Vec<serde_json::Value>,
    live_tail: Option<String>,
    metrics: Option<SessionMetrics>,
    dirty: bool,
) -> DaemonEvent {
    let mut final_dirty = dirty;
    let final_appended = if appended_messages.is_empty() {
        Vec::new()
    } else {
        match serde_json::to_vec(&appended_messages) {
            Ok(bytes) if bytes.len() <= MAX_INLINE_OUTPUT_BYTES => appended_messages,
            _ => {
                final_dirty = true;
                Vec::new()
            }
        }
    };
    let final_tail =
        live_tail.map(|t| starfane_tmux::trim_and_cap_tail(&t, 20, MAX_INLINE_LIVE_TAIL_BYTES));
    DaemonEvent::Output {
        seq,
        session_id,
        start_seq,
        total_messages,
        appended_messages: final_appended,
        live_tail: final_tail,
        metrics,
        dirty: final_dirty,
    }
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

#[cfg(test)]
mod push_tests {
    use super::*;
    use crate::config::NotifyOwnership;
    use crate::core::transcript::TranscriptResponse;
    use crate::nodes::{RemoteSpawnOutcome, RemoteSpawnReq};
    use crate::notice::BoxFuture;
    use starfane_tmux::{CaptureRequest, PanePreview};
    use std::sync::atomic::AtomicUsize;

    struct MockPushProvider {
        host_id: HostId,
        kind: HostKind,
        push_tx_slot: Mutex<Option<mpsc::UnboundedSender<HostPushSignal>>>,
        full_syncs: AtomicUsize,
        dirty_syncs: Mutex<Vec<Vec<PaneId>>>,
    }

    impl MockPushProvider {
        fn new(id: &str, kind: HostKind) -> Self {
            Self {
                host_id: HostId::new(id),
                kind,
                push_tx_slot: Mutex::new(None),
                full_syncs: AtomicUsize::new(0),
                dirty_syncs: Mutex::new(Vec::new()),
            }
        }

        fn mk_session(&self, raw_sid: &str, raw_pane: &str, state: SessionState) -> SessionRecord {
            SessionRecord {
                session_id: starfane_tmux::qualify_host_session(
                    &self.host_id,
                    &SessionId::new(raw_sid),
                ),
                agent: AgentKind::Claude,
                pane: Some(starfane_tmux::qualify_host_pane(
                    &self.host_id,
                    &PaneId::new(raw_pane),
                )),
                pane_resolution: PaneResolution::Env,
                cwd: Some("/srv/app".into()),
                git_root: Some("/srv/app".into()),
                model: Some("opus".into()),
                effort: None,
                state,
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
                tmux_target: Some("dev:0.0".into()),
                tmux_label: Some("dev-main".into()),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: Some(self.host_id.clone()),
                host_display_name: Some("Mock Host".into()),
                mux_kind: Some("tmux".into()),
            }
        }
    }

    impl NodeProvider for MockPushProvider {
        fn host_id(&self) -> &HostId {
            &self.host_id
        }
        fn kind(&self) -> HostKind {
            self.kind
        }
        fn default_display_name(&self) -> &str {
            "Mock Host"
        }
        fn notify_ownership(&self) -> NotifyOwnership {
            NotifyOwnership::Hub
        }
        fn sync_with_claimed<'a>(
            &'a self,
            display_name: &'a str,
            _claimed_external_ids: &'a HashSet<String>,
        ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
            let n = self.full_syncs.fetch_add(1, Ordering::SeqCst) + 1;
            let disp = display_name.to_string();
            Box::pin(async move {
                Ok(NodeSnapshot {
                    host: HostRecord {
                        id: self.host_id.clone(),
                        display_name: disp,
                        kind: self.kind,
                        status: HostStatus::Online,
                        active_endpoint: Some("ssh://mock".into()),
                        last_seen_ms: Some(id::now_ms()),
                        error: None,
                        session_count: 2,
                        rtt_ms: Some(11),
                        sync_ms: Some(42 + n as u64),
                        next_retry_ms: None,
                        retry_paused: false,
                    },
                    sessions: vec![
                        self.mk_session("tmux-%1", "%1", SessionState::Idle),
                        self.mk_session("tmux-%2", "%2", SessionState::Idle),
                    ],
                    events: Vec::new(),
                    output_deltas: Vec::new(),
                })
            })
        }
        fn sync_dirty_panes<'a>(
            &'a self,
            display_name: &'a str,
            dirty_panes: &'a [PaneId],
            _claimed_external_ids: &'a HashSet<String>,
        ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
            self.dirty_syncs.lock().unwrap().push(dirty_panes.to_vec());
            let disp = display_name.to_string();
            Box::pin(async move {
                // Return ONLY the dirty session %1 plus an inline Output delta; sibling %2 is omitted!
                Ok(NodeSnapshot {
                    host: HostRecord {
                        id: self.host_id.clone(),
                        display_name: disp,
                        kind: self.kind,
                        status: HostStatus::Online,
                        active_endpoint: Some("ssh://mock".into()),
                        last_seen_ms: Some(id::now_ms()),
                        error: None,
                        session_count: 1,
                        rtt_ms: Some(10),
                        sync_ms: Some(18),
                        next_retry_ms: None,
                        retry_paused: false,
                    },
                    sessions: vec![self.mk_session("tmux-%1", "%1", SessionState::Working)],
                    events: Vec::new(),
                    output_deltas: vec![DaemonEvent::Output {
                        seq: 0,
                        session_id: SessionId::new("tmux-%1"),
                        start_seq: Some(3),
                        total_messages: Some(4),
                        appended_messages: vec![
                            serde_json::json!({"role":"assistant","text":"fast push"}),
                        ],
                        live_tail: Some("streaming...".into()),
                        metrics: None,
                        dirty: false,
                    }],
                })
            })
        }
        fn spawn_push_watcher(
            &self,
            tx: mpsc::UnboundedSender<HostPushSignal>,
        ) -> Option<tokio::task::JoinHandle<()>> {
            *self.push_tx_slot.lock().unwrap() = Some(tx);
            Some(tokio::spawn(async {
                std::future::pending::<()>().await;
            }))
        }
        fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn spawn_session<'a>(
            &'a self,
            _display_name: &'a str,
            _req: &'a RemoteSpawnReq,
        ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn stage_input_files<'a>(
            &'a self,
            _files: &'a [(String, PathBuf)],
        ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>> {
            Box::pin(async { Ok(HashMap::new()) })
        }
        fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn rtt_ms(&self) -> Option<u64> {
            Some(10)
        }
        fn sync_ms(&self) -> Option<u64> {
            Some(18)
        }
        fn next_retry_ms(&self) -> Option<u64> {
            None
        }
        fn circuit_status(&self) -> HostStatus {
            HostStatus::Online
        }
        fn capture_pane<'a>(
            &'a self,
            _local_pane: &'a PaneId,
            _req: &'a CaptureRequest,
        ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn fetch_transcript<'a>(
            &'a self,
            _remote_session: &'a SessionId,
            _query: &'a TranscriptQuery,
        ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn dispatch_interactive<'a>(
            &'a self,
            _remote_session: Option<&'a SessionId>,
            _local_pane: &'a PaneId,
            _text: &'a str,
            _settle_ms: u64,
        ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn focus_pane<'a>(
            &'a self,
            _remote_session: &'a SessionId,
            _local_pane: &'a PaneId,
        ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
        fn close_pane<'a>(
            &'a self,
            _remote_session: &'a SessionId,
            _local_pane: &'a PaneId,
        ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
            Box::pin(async { Err(ApiError::internal("unused")) })
        }
    }

    fn build_core_with_mock_provider(
        root: &Path,
        provider: Arc<MockPushProvider>,
    ) -> (Core, HostId, Arc<Notify>) {
        let mut cfg = StarfaneConfig::default();
        cfg.paths.tmux = "/usr/bin/false".into();
        cfg.zellij.enabled = false;
        let base = Core::new(&DaemonConfig {
            state_dir: root.join("state"),
            socket: root.join("state/starfaned.sock"),
            config: cfg.clone(),
            web_dir: None,
            foreground: true,
        })
        .unwrap();
        let hid = provider.host_id().clone();
        let wake = Arc::new(Notify::new());
        let mut remote_nodes: HashMap<HostId, Arc<dyn NodeProvider>> = HashMap::new();
        remote_nodes.insert(hid.clone(), provider);
        let mut remote_wake = HashMap::new();
        remote_wake.insert(hid.clone(), wake.clone());
        let mut state = State::default();
        state.hosts.insert(
            hid.clone(),
            HostRecord {
                id: hid.clone(),
                display_name: "Mock Host".into(),
                kind: HostKind::Ssh,
                status: HostStatus::Offline,
                active_endpoint: None,
                last_seen_ms: None,
                error: None,
                session_count: 0,
                rtt_ms: None,
                sync_ms: None,
                next_retry_ms: None,
                retry_paused: false,
            },
        );
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let core = Core {
            inner: Arc::new(Inner {
                cfg: base.inner.cfg.clone(),
                state_dir: base.inner.state_dir.clone(),
                web_dir: None,
                started_ms: base.inner.started_ms,
                web_token: base.inner.web_token.clone(),
                state: Mutex::new(state),
                events,
                replay: Mutex::new(VecDeque::with_capacity(SSE_REPLAY_CAPACITY)),
                seq: AtomicU64::new(0),
                notice_tx,
                notice_rx: Mutex::new(Some(notice_rx)),
                feishu: RwLock::new(None),
                tmux: base.inner.tmux.clone(),
                mux: base.inner.mux.clone(),
                access_gate: base.inner.access_gate.clone(),
                notify_router: NotifyRouter::from_config(&cfg),
                remote_nodes,
                remote_wake,
                store: Store::new(root.join("state")),
                acks: Mutex::new(HashMap::new()),
                dirty: AtomicBool::new(false),
                snapshot_in_flight: AtomicBool::new(false),
                shutdown: Notify::new(),
                last_attention: Mutex::new(Vec::new()),
                model_catalog: Mutex::new(None),
                slash_cache: Mutex::new(slash::SlashCache::default()),
                telemetry: Mutex::new(TelemetryCaches::default()),
                transcript_index: Arc::new(Mutex::new(transcript::TranscriptIndexCache::default())),
                discovery_cache: Mutex::new(discover::DiscoveryScanCache::default()),
            }),
        };
        (core, hid, wake)
    }

    #[test]
    fn make_output_event_enforces_32kib_inline_budget() {
        let small = make_output_event(
            1,
            SessionId::new("s1"),
            Some(0),
            Some(1),
            vec![serde_json::json!({"role": "assistant", "text": "ok"})],
            Some("tail".into()),
            None,
            false,
        );
        match small {
            DaemonEvent::Output {
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(appended_messages.len(), 1);
                assert!(!dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        let big = make_output_event(
            2,
            SessionId::new("s1"),
            Some(1),
            Some(2),
            vec![serde_json::json!({"role": "assistant", "text": "a".repeat(33 * 1024)})],
            None,
            None,
            false,
        );
        match big {
            DaemonEvent::Output {
                appended_messages,
                dirty,
                ..
            } => {
                assert!(appended_messages.is_empty());
                assert!(dirty);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_push_supervisor_handles_dirty_panes_peer_events_and_full_sync() {
        let root = std::env::temp_dir().join(format!("starfane-push-sup-{}", id::new_id("T")));
        let provider = Arc::new(MockPushProvider::new("mac-xiling", HostKind::Ssh));
        let (core, hid, wake) = build_core_with_mock_provider(&root, provider.clone());
        let mut sse = core.subscribe();

        let sup_core = core.clone();
        let sup_hid = hid.clone();
        let sup = tokio::spawn(async move {
            sup_core.run_remote_host_supervisor(sup_hid, wake).await;
        });

        // Wait until initial sync finishes and push watcher registers its tx.
        let push_tx = loop {
            if let Some(tx) = provider.push_tx_slot.lock().unwrap().clone() {
                break tx;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(provider.full_syncs.load(Ordering::SeqCst), 1);
        {
            let st = lock(&core.inner.state);
            assert!(st
                .sessions
                .contains_key(&SessionId::new("mac-xiling:tmux-%1")));
            assert!(st
                .sessions
                .contains_key(&SessionId::new("mac-xiling:tmux-%2")));
            assert_eq!(st.hosts[&hid].sync_ms, Some(43));
        }

        // 1. Send DirtyPanes([%1]) -> targeted sync_dirty_panes updates %1, keeps %2 alive, updates sync_ms=18, broadcasts Output!
        push_tx
            .send(HostPushSignal::DirtyPanes(vec![PaneId::new("%1")]))
            .unwrap();

        let mut saw_dirty_output = false;
        let mut saw_sync_ms_18 = false;
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        while !saw_dirty_output || !saw_sync_ms_18 {
            tokio::select! {
                _ = &mut deadline => panic!("timed out waiting for DirtyPanes output/host SSE"),
                Ok(ev) = sse.recv() => {
                    match ev {
                        DaemonEvent::Output { session_id, start_seq, total_messages, appended_messages, dirty, .. }
                            if session_id.as_str() == "mac-xiling:tmux-%1" =>
                        {
                            assert_eq!(start_seq, Some(3));
                            assert_eq!(total_messages, Some(4));
                            assert_eq!(appended_messages.len(), 1);
                            assert!(!dirty);
                            saw_dirty_output = true;
                        }
                        DaemonEvent::Host { host, .. } if host.id == hid && host.sync_ms == Some(18) => {
                            assert_eq!(host.session_count, 2, "partial dirty sync must NOT reap non-dirty sibling %2");
                            saw_sync_ms_18 = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(provider.dirty_syncs.lock().unwrap().len(), 1);
        assert!(lock(&core.inner.state)
            .sessions
            .get(&SessionId::new("mac-xiling:tmux-%2"))
            .is_some_and(|s| s.state != SessionState::Ended));

        // 2. Send PeerEvent(Output) for unqualified session -> qualified & forwarded to Hub SSE!
        push_tx
            .send(HostPushSignal::PeerEvent(DaemonEvent::Output {
                seq: 99,
                session_id: SessionId::new("tmux-%2"),
                start_seq: Some(10),
                total_messages: Some(11),
                appended_messages: vec![serde_json::json!({"role":"assistant","text":"peer push"})],
                live_tail: None,
                metrics: None,
                dirty: false,
            }))
            .unwrap();
        // Foreign-federated session must be ignored (no loop).
        push_tx
            .send(HostPushSignal::PeerEvent(DaemonEvent::Output {
                seq: 100,
                session_id: SessionId::new("other-host:tmux-%9"),
                start_seq: Some(0),
                total_messages: Some(1),
                appended_messages: Vec::new(),
                live_tail: None,
                metrics: None,
                dirty: true,
            }))
            .unwrap();

        let mut saw_peer_output = false;
        let deadline2 = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline2);
        while !saw_peer_output {
            tokio::select! {
                _ = &mut deadline2 => panic!("timed out waiting for PeerEvent output SSE"),
                Ok(ev) = sse.recv() => {
                    if let DaemonEvent::Output { session_id, appended_messages, .. } = ev {
                        assert_ne!(session_id.as_str(), "other-host:tmux-%9");
                        if session_id.as_str() == "mac-xiling:tmux-%2" {
                            assert_eq!(appended_messages[0]["text"], "peer push");
                            saw_peer_output = true;
                        }
                    }
                }
            }
        }

        // 3. Send Reconnect -> triggers immediate full sync (full_syncs == 2).
        push_tx.send(HostPushSignal::Reconnect).unwrap();
        let deadline3 = tokio::time::Instant::now() + Duration::from_secs(2);
        while provider.full_syncs.load(Ordering::SeqCst) < 2 {
            assert!(
                tokio::time::Instant::now() < deadline3,
                "timed out waiting for Reconnect full sync"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        core.request_shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(1), sup).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dirty_panes_transition_out_of_working_schedules_single_followup() {
        struct TransitionMockProvider {
            host_id: HostId,
            push_tx_slot: Mutex<Option<mpsc::UnboundedSender<HostPushSignal>>>,
            next_dirty_state: Mutex<SessionState>,
            dirty_syncs: Mutex<Vec<Vec<PaneId>>>,
        }

        impl TransitionMockProvider {
            fn mk_session(
                &self,
                raw_sid: &str,
                raw_pane: &str,
                state: SessionState,
            ) -> SessionRecord {
                SessionRecord {
                    session_id: starfane_tmux::qualify_host_session(
                        &self.host_id,
                        &SessionId::new(raw_sid),
                    ),
                    agent: AgentKind::Claude,
                    pane: Some(starfane_tmux::qualify_host_pane(
                        &self.host_id,
                        &PaneId::new(raw_pane),
                    )),
                    pane_resolution: PaneResolution::Env,
                    cwd: Some("/srv/app".into()),
                    git_root: None,
                    model: None,
                    effort: None,
                    state,
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
                    tmux_target: Some("dev:0.0".into()),
                    tmux_label: Some("dev-main".into()),
                    discovered_only: true,
                    transcript_path: None,
                    metrics: None,
                    host_id: Some(self.host_id.clone()),
                    host_display_name: Some("Mock Host".into()),
                    mux_kind: Some("tmux".into()),
                }
            }
        }

        impl NodeProvider for TransitionMockProvider {
            fn host_id(&self) -> &HostId {
                &self.host_id
            }
            fn kind(&self) -> HostKind {
                HostKind::Ssh
            }
            fn default_display_name(&self) -> &str {
                "Mock Host"
            }
            fn notify_ownership(&self) -> NotifyOwnership {
                NotifyOwnership::Hub
            }
            fn sync_with_claimed<'a>(
                &'a self,
                display_name: &'a str,
                _claimed: &'a HashSet<String>,
            ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
                let disp = display_name.to_string();
                Box::pin(async move {
                    Ok(NodeSnapshot {
                        host: HostRecord {
                            id: self.host_id.clone(),
                            display_name: disp,
                            kind: HostKind::Ssh,
                            status: HostStatus::Online,
                            active_endpoint: Some("ssh://mock".into()),
                            last_seen_ms: Some(id::now_ms()),
                            error: None,
                            session_count: 1,
                            rtt_ms: Some(5),
                            sync_ms: Some(5),
                            next_retry_ms: None,
                            retry_paused: false,
                        },
                        sessions: vec![self.mk_session("tmux-%1", "%1", SessionState::Working)],
                        events: Vec::new(),
                        output_deltas: Vec::new(),
                    })
                })
            }
            fn sync_dirty_panes<'a>(
                &'a self,
                display_name: &'a str,
                dirty_panes: &'a [PaneId],
                _claimed: &'a HashSet<String>,
            ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
                self.dirty_syncs.lock().unwrap().push(dirty_panes.to_vec());
                let call_idx = self.dirty_syncs.lock().unwrap().len();
                let st = *self.next_dirty_state.lock().unwrap();
                let disp = display_name.to_string();
                Box::pin(async move {
                    let deltas = if call_idx >= 2 {
                        vec![DaemonEvent::Output {
                            seq: 0,
                            session_id: SessionId::new("tmux-%1"),
                            start_seq: Some(1),
                            total_messages: Some(2),
                            appended_messages: vec![
                                serde_json::json!({"role":"assistant","text":"late flush"}),
                            ],
                            live_tail: None,
                            metrics: None,
                            dirty: false,
                        }]
                    } else {
                        Vec::new()
                    };
                    Ok(NodeSnapshot {
                        host: HostRecord {
                            id: self.host_id.clone(),
                            display_name: disp,
                            kind: HostKind::Ssh,
                            status: HostStatus::Online,
                            active_endpoint: Some("ssh://mock".into()),
                            last_seen_ms: Some(id::now_ms()),
                            error: None,
                            session_count: 1,
                            rtt_ms: Some(5),
                            sync_ms: Some(5),
                            next_retry_ms: None,
                            retry_paused: false,
                        },
                        sessions: vec![self.mk_session("tmux-%1", "%1", st)],
                        events: Vec::new(),
                        output_deltas: deltas,
                    })
                })
            }
            fn spawn_push_watcher(
                &self,
                tx: mpsc::UnboundedSender<HostPushSignal>,
            ) -> Option<tokio::task::JoinHandle<()>> {
                *self.push_tx_slot.lock().unwrap() = Some(tx);
                Some(tokio::spawn(async {
                    std::future::pending::<()>().await;
                }))
            }
            fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn spawn_session<'a>(
                &'a self,
                _: &'a str,
                _: &'a RemoteSpawnReq,
            ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn stage_input_files<'a>(
                &'a self,
                _: &'a [(String, PathBuf)],
            ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>> {
                Box::pin(async { Ok(HashMap::new()) })
            }
            fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn rtt_ms(&self) -> Option<u64> {
                Some(5)
            }
            fn sync_ms(&self) -> Option<u64> {
                Some(5)
            }
            fn next_retry_ms(&self) -> Option<u64> {
                None
            }
            fn circuit_status(&self) -> HostStatus {
                HostStatus::Online
            }
            fn capture_pane<'a>(
                &'a self,
                _: &'a PaneId,
                _: &'a CaptureRequest,
            ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn fetch_transcript<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a TranscriptQuery,
            ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn dispatch_interactive<'a>(
                &'a self,
                _: Option<&'a SessionId>,
                _: &'a PaneId,
                _: &'a str,
                _: u64,
            ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn focus_pane<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a PaneId,
            ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn close_pane<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a PaneId,
            ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
        }

        let root = std::env::temp_dir().join(format!("starfane-followup-{}", id::new_id("T")));
        let provider = Arc::new(TransitionMockProvider {
            host_id: HostId::new("ssh-follow"),
            push_tx_slot: Mutex::new(None),
            next_dirty_state: Mutex::new(SessionState::Idle),
            dirty_syncs: Mutex::new(Vec::new()),
        });

        let mut cfg = StarfaneConfig::default();
        cfg.paths.tmux = "/usr/bin/false".into();
        cfg.zellij.enabled = false;
        let base = Core::new(&DaemonConfig {
            state_dir: root.join("state"),
            socket: root.join("state/starfaned.sock"),
            config: cfg.clone(),
            web_dir: None,
            foreground: true,
        })
        .unwrap();
        let hid = provider.host_id().clone();
        let wake = Arc::new(Notify::new());
        let mut remote_nodes: HashMap<HostId, Arc<dyn NodeProvider>> = HashMap::new();
        remote_nodes.insert(hid.clone(), provider.clone());
        let mut remote_wake = HashMap::new();
        remote_wake.insert(hid.clone(), wake.clone());
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let core = Core {
            inner: Arc::new(Inner {
                cfg: base.inner.cfg.clone(),
                state_dir: base.inner.state_dir.clone(),
                web_dir: None,
                started_ms: base.inner.started_ms,
                web_token: base.inner.web_token.clone(),
                state: Mutex::new(State::default()),
                events,
                replay: Mutex::new(VecDeque::with_capacity(SSE_REPLAY_CAPACITY)),
                seq: AtomicU64::new(0),
                notice_tx,
                notice_rx: Mutex::new(Some(notice_rx)),
                feishu: RwLock::new(None),
                tmux: base.inner.tmux.clone(),
                mux: base.inner.mux.clone(),
                access_gate: base.inner.access_gate.clone(),
                notify_router: NotifyRouter::from_config(&cfg),
                remote_nodes,
                remote_wake,
                store: Store::new(root.join("state")),
                acks: Mutex::new(HashMap::new()),
                dirty: AtomicBool::new(false),
                snapshot_in_flight: AtomicBool::new(false),
                shutdown: Notify::new(),
                last_attention: Mutex::new(Vec::new()),
                model_catalog: Mutex::new(None),
                slash_cache: Mutex::new(slash::SlashCache::default()),
                telemetry: Mutex::new(TelemetryCaches::default()),
                transcript_index: Arc::new(Mutex::new(transcript::TranscriptIndexCache::default())),
                discovery_cache: Mutex::new(discover::DiscoveryScanCache::default()),
            }),
        };

        let mut sse = core.subscribe();
        let sup_core = core.clone();
        let sup_hid = hid.clone();
        let sup = tokio::spawn(async move {
            sup_core.run_remote_host_supervisor(sup_hid, wake).await;
        });

        let push_tx = loop {
            if let Some(tx) = provider.push_tx_slot.lock().unwrap().clone() {
                break tx;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // Trigger DirtyPanes([%1]) which transitions Working -> Idle.
        push_tx
            .send(HostPushSignal::DirtyPanes(vec![PaneId::new("%1")]))
            .unwrap();

        let mut saw_late_flush = false;
        let deadline = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(deadline);
        while !saw_late_flush {
            tokio::select! {
                _ = &mut deadline => panic!("timed out waiting for 900ms follow-up output"),
                Ok(ev) = sse.recv() => {
                    if let DaemonEvent::Output { session_id, appended_messages, .. } = ev {
                        if session_id.as_str() == "ssh-follow:tmux-%1"
                            && appended_messages.first().and_then(|m| m["text"].as_str()) == Some("late flush")
                        {
                            saw_late_flush = true;
                        }
                    }
                }
            }
        }
        assert!(saw_late_flush);

        // Wait 1.1s to verify the follow-up does NOT re-trigger/loop!
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(provider.dirty_syncs.lock().unwrap().len(), 2);

        core.request_shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(1), sup).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pause_host_retry_parks_supervisor_and_probe_resumes() {
        use std::sync::atomic::AtomicUsize;

        struct PausableMockProvider {
            host_id: HostId,
            paused: AtomicBool,
            healthy: AtomicBool,
            sync_calls: AtomicUsize,
            watcher_spawns: AtomicUsize,
            last_error: Mutex<Option<String>>,
            failures: AtomicUsize,
            next_retry: Mutex<Option<u64>>,
        }

        impl NodeProvider for PausableMockProvider {
            fn host_id(&self) -> &HostId {
                &self.host_id
            }
            fn kind(&self) -> HostKind {
                HostKind::Ssh
            }
            fn default_display_name(&self) -> &str {
                "Pause Test Host"
            }
            fn notify_ownership(&self) -> crate::config::NotifyOwnership {
                crate::config::NotifyOwnership::Hub
            }
            fn sync_with_claimed<'a>(
                &'a self,
                display_name: &'a str,
                _claimed: &'a HashSet<String>,
            ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
                self.sync_calls.fetch_add(1, Ordering::SeqCst);
                let disp = display_name.to_string();
                Box::pin(async move {
                    if self.healthy.load(Ordering::Acquire) {
                        self.failures.store(0, Ordering::SeqCst);
                        *self.next_retry.lock().unwrap() = None;
                        *self.last_error.lock().unwrap() = None;
                        Ok(NodeSnapshot {
                            host: HostRecord {
                                id: self.host_id.clone(),
                                display_name: disp,
                                kind: HostKind::Ssh,
                                status: HostStatus::Online,
                                active_endpoint: Some("ssh://mock".into()),
                                last_seen_ms: Some(id::now_ms()),
                                error: None,
                                session_count: 0,
                                rtt_ms: Some(12),
                                sync_ms: Some(15),
                                next_retry_ms: None,
                                retry_paused: self.is_retry_paused(),
                            },
                            sessions: Vec::new(),
                            events: Vec::new(),
                            output_deltas: Vec::new(),
                        })
                    } else {
                        self.failures.fetch_add(1, Ordering::SeqCst);
                        let err = "ssh: connect timed out".to_string();
                        *self.last_error.lock().unwrap() = Some(err.clone());
                        if !self.is_retry_paused() {
                            *self.next_retry.lock().unwrap() = Some(id::now_ms() + 15_000);
                        }
                        Err(err)
                    }
                })
            }
            fn spawn_push_watcher(
                &self,
                _tx: mpsc::UnboundedSender<HostPushSignal>,
            ) -> Option<tokio::task::JoinHandle<()>> {
                self.watcher_spawns.fetch_add(1, Ordering::SeqCst);
                Some(tokio::spawn(async {
                    std::future::pending::<()>().await;
                }))
            }
            fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn spawn_session<'a>(
                &'a self,
                _: &'a str,
                _: &'a RemoteSpawnReq,
            ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn stage_input_files<'a>(
                &'a self,
                _: &'a [(String, PathBuf)],
            ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>> {
                Box::pin(async { Ok(HashMap::new()) })
            }
            fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>> {
                self.set_retry_paused(false);
                Box::pin(async move {
                    let empty = HashSet::new();
                    match self
                        .sync_with_claimed(self.default_display_name(), &empty)
                        .await
                    {
                        Ok(snap) => Ok(HostProbeOutcome {
                            host_id: self.host_id.clone(),
                            status: snap.host.status,
                            rtt_ms: snap.host.rtt_ms,
                            next_retry_ms: snap.host.next_retry_ms,
                            retry_paused: self.is_retry_paused(),
                            error: None,
                            snapshot: Some(snap),
                        }),
                        Err(err) => Ok(HostProbeOutcome {
                            host_id: self.host_id.clone(),
                            status: self.circuit_status(),
                            rtt_ms: None,
                            next_retry_ms: self.next_retry_ms(),
                            retry_paused: self.is_retry_paused(),
                            error: Some(err),
                            snapshot: None,
                        }),
                    }
                })
            }
            fn rtt_ms(&self) -> Option<u64> {
                None
            }
            fn next_retry_ms(&self) -> Option<u64> {
                *self.next_retry.lock().unwrap()
            }
            fn circuit_status(&self) -> HostStatus {
                match self.failures.load(Ordering::Acquire) {
                    0 => HostStatus::Online,
                    1..=2 => HostStatus::Degraded,
                    _ => HostStatus::Offline,
                }
            }
            fn is_retry_paused(&self) -> bool {
                self.paused.load(Ordering::Acquire)
            }
            fn set_retry_paused(&self, paused: bool) {
                self.paused.store(paused, Ordering::Release);
                if paused {
                    *self.next_retry.lock().unwrap() = None;
                }
            }
            fn capture_pane<'a>(
                &'a self,
                _: &'a PaneId,
                _: &'a CaptureRequest,
            ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn fetch_transcript<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a TranscriptQuery,
            ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn dispatch_interactive<'a>(
                &'a self,
                _: Option<&'a SessionId>,
                _: &'a PaneId,
                _: &'a str,
                _: u64,
            ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn focus_pane<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a PaneId,
            ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
            fn close_pane<'a>(
                &'a self,
                _: &'a SessionId,
                _: &'a PaneId,
            ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
                Box::pin(async { Err(ApiError::internal("unused")) })
            }
        }

        let root = std::env::temp_dir().join(format!("starfane-pause-retry-{}", id::new_id("T")));
        let provider = Arc::new(PausableMockProvider {
            host_id: HostId::new("ssh-pause"),
            paused: AtomicBool::new(false),
            healthy: AtomicBool::new(false),
            sync_calls: AtomicUsize::new(0),
            watcher_spawns: AtomicUsize::new(0),
            last_error: Mutex::new(None),
            failures: AtomicUsize::new(0),
            next_retry: Mutex::new(None),
        });
        let mut cfg = StarfaneConfig::default();
        cfg.paths.tmux = "/usr/bin/false".into();
        cfg.zellij.enabled = false;
        let base = Core::new(&DaemonConfig {
            state_dir: root.join("state"),
            socket: root.join("state/starfaned.sock"),
            config: cfg.clone(),
            web_dir: None,
            foreground: true,
        })
        .unwrap();
        let hid = provider.host_id().clone();
        let wake = Arc::new(Notify::new());
        let mut remote_nodes: HashMap<HostId, Arc<dyn NodeProvider>> = HashMap::new();
        remote_nodes.insert(hid.clone(), provider.clone());
        let mut remote_wake = HashMap::new();
        remote_wake.insert(hid.clone(), wake.clone());
        let mut state = State::default();
        state.hosts.insert(
            hid.clone(),
            HostRecord {
                id: hid.clone(),
                display_name: "Pause Test Host".into(),
                kind: HostKind::Ssh,
                status: HostStatus::Offline,
                active_endpoint: None,
                last_seen_ms: None,
                error: None,
                session_count: 0,
                rtt_ms: None,
                sync_ms: None,
                next_retry_ms: None,
                retry_paused: false,
            },
        );
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let core = Core {
            inner: Arc::new(Inner {
                cfg: base.inner.cfg.clone(),
                state_dir: base.inner.state_dir.clone(),
                web_dir: None,
                started_ms: base.inner.started_ms,
                web_token: base.inner.web_token.clone(),
                state: Mutex::new(state),
                events,
                replay: Mutex::new(VecDeque::with_capacity(SSE_REPLAY_CAPACITY)),
                seq: AtomicU64::new(0),
                notice_tx,
                notice_rx: Mutex::new(Some(notice_rx)),
                feishu: RwLock::new(None),
                tmux: base.inner.tmux.clone(),
                mux: base.inner.mux.clone(),
                access_gate: base.inner.access_gate.clone(),
                notify_router: NotifyRouter::from_config(&cfg),
                remote_nodes,
                remote_wake,
                store: Store::new(root.join("state")),
                acks: Mutex::new(HashMap::new()),
                dirty: AtomicBool::new(false),
                snapshot_in_flight: AtomicBool::new(false),
                shutdown: Notify::new(),
                last_attention: Mutex::new(Vec::new()),
                model_catalog: Mutex::new(None),
                slash_cache: Mutex::new(slash::SlashCache::default()),
                telemetry: Mutex::new(TelemetryCaches::default()),
                transcript_index: Arc::new(Mutex::new(transcript::TranscriptIndexCache::default())),
                discovery_cache: Mutex::new(discover::DiscoveryScanCache::default()),
            }),
        };

        let mut sse = core.subscribe();
        let sup_core = core.clone();
        let sup_hid = hid.clone();
        let sup = tokio::spawn(async move {
            sup_core.run_remote_host_supervisor(sup_hid, wake).await;
        });

        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            if provider.sync_calls.load(Ordering::Acquire) >= 1
                && provider.watcher_spawns.load(Ordering::Acquire) >= 1
            {
                break;
            }
            tokio::select! {
                _ = &mut deadline => panic!("timed out waiting for initial supervisor sync"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }

        let paused_rec = core.pause_host_retry(hid.as_str()).await.unwrap();
        assert!(paused_rec.retry_paused);
        assert_eq!(paused_rec.next_retry_ms, None);

        let mut saw_paused_sse = false;
        let sse_deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(sse_deadline);
        while !saw_paused_sse {
            tokio::select! {
                _ = &mut sse_deadline => panic!("timed out waiting for paused SSE host event"),
                Ok(ev) = sse.recv() => {
                    if let DaemonEvent::Host { host, .. } = ev {
                        if host.id == hid && host.retry_paused && host.next_retry_ms.is_none() {
                            saw_paused_sse = true;
                        }
                    }
                }
            }
        }
        assert!(saw_paused_sse);

        let calls_after_pause = provider.sync_calls.load(Ordering::Acquire);
        core.sync_remote_host(&hid).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            provider.sync_calls.load(Ordering::Acquire),
            calls_after_pause
        );

        provider.healthy.store(true, Ordering::Release);
        let outcome = core.probe_host(hid.as_str()).await.unwrap();
        assert!(!outcome.retry_paused);
        assert_eq!(outcome.status, HostStatus::Online);
        let hosts = core.hosts();
        let h = hosts.iter().find(|h| h.id == hid).unwrap();
        assert!(!h.retry_paused);
        assert_eq!(h.status, HostStatus::Online);

        core.request_shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(1), sup).await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
