//! `Core`: shared daemon state + command dispatch (contracts §7).

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
pub(crate) mod uploads;
pub(crate) mod util;

use crate::config::BatonConfig;
use crate::notice::{FeishuSender, Notice, NoticeKind};
use crate::store::{write_private, Store};
use crate::{DaemonConfig, DaemonError};
use baton_proto::*;
use state::State;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);
const SPOOL_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVER_INTERVAL: Duration = Duration::from_secs(3);
const EVENT_CHANNEL: usize = 1024;
const FEISHU_RATE_WINDOW_MS: u64 = 60_000;
const MAX_FEISHU_NOTICES_PER_WINDOW: usize = 6;

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
    pub cfg: BatonConfig,
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
    pub tmux: baton_tmux::Tmux,
    pub store: Store,
    pub acks: Mutex<HashMap<SessionId, Vec<oneshot::Sender<()>>>>,
    pub dirty: AtomicBool,
    pub shutdown: Notify,
    pub last_attention: Mutex<Vec<AttentionItem>>,
    pub model_catalog: Mutex<Option<models::ModelCatalog>>,
    pub slash_cache: Mutex<slash::SlashCache>,
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
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let tmux = baton_tmux::Tmux::new(
            PathBuf::from(cfg.config.tmux_bin()),
            cfg.config.tmux_socket(),
        );
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
                store,
                acks: Mutex::new(HashMap::new()),
                dirty: AtomicBool::new(purged_any),
                shutdown: Notify::new(),
                last_attention: Mutex::new(Vec::new()),
                model_catalog: Mutex::new(None),
                slash_cache: Mutex::new(slash::SlashCache::default()),
            }),
        })
    }

    // ------------------------------------------------------------------
    // Public surface (web / lark / uds)
    // ------------------------------------------------------------------

    pub fn config(&self) -> &BatonConfig {
        &self.inner.cfg
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

    /// Return curated native `/` commands + 60s-cached local skills + Baton shortcuts.
    pub async fn slash_commands(
        &self,
        agent: Option<AgentKind>,
    ) -> Result<slash::SlashCommandsResponse, ApiError> {
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            let home = slash::user_home_dir();
            let mut cache = lock(&core.inner.slash_cache);
            let scanned = cache.get_or_refresh(home.as_deref());
            slash::build_catalog(agent, scanned)
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

    pub fn set_feishu_mute(&self, until_ms: Option<u64>) {
        lock(&self.inner.state).feishu_muted_until = until_ms;
        self.mark_dirty();
    }

    pub fn feishu_muted_until(&self) -> Option<u64> {
        let st = lock(&self.inner.state);
        st.feishu_muted_until.filter(|t| *t > id::now_ms())
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

    /// Focus the pane's tmux window/pane and switch the appropriate client without hijacking other sessions.
    pub async fn focus_session(&self, id: &SessionId) -> Result<serde_json::Value, ApiError> {
        let pane = lock(&self.inner.state)
            .sessions
            .get(id)
            .and_then(|s| s.pane.clone())
            .ok_or_else(|| ApiError::not_found(format!("session {id} has no known pane")))?;
        let core = self.clone();
        let outcome = tokio::task::spawn_blocking(move || core.inner.tmux.focus_pane(&pane))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, e.to_string()))?;
        let mut payload = serde_json::to_value(&outcome)
            .map_err(|e| ApiError::internal(format!("serialize focus outcome: {e}")))?;
        if outcome.switched_client.is_none() {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "attach_command".into(),
                    serde_json::Value::String(format!("tmux attach -t {}", outcome.session_name)),
                );
            }
        }
        Ok(payload)
    }

    /// State snapshot as JSON (always includes `feishu_muted_until_ms` for the Web UI).
    pub fn state_json(&self) -> serde_json::Value {
        let snap = self.snapshot();
        let mut v = serde_json::to_value(&snap).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            let val = self
                .feishu_muted_until()
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null);
            obj.insert("feishu_muted_until_ms".into(), val);
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

    /// Queue a Feishu notice (dropped while muted; burst-limited globally; Web is unaffected).
    pub(crate) fn notify(&self, notice: Notice) {
        if !self.inner.cfg.feishu.enabled || self.feishu_muted_until().is_some() {
            return;
        }
        let now_ms = id::now_ms();
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
                        "1 分钟内连续触发超过 {MAX_FEISHU_NOTICES_PER_WINDOW} 条通知，后续同窗口通知已自动折叠。请打开 Baton Web 控制台查看完整会话状态。\n"
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

    fn flush_snapshot(&self) {
        if !self.inner.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let st = lock(&self.inner.state);
        if let Err(e) = self.inner.store.save_snapshot(&*st) {
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
            C::GetState => Ok(self.state_json()),
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
            } => to_json(self.resolve_identity(agent, cwd, pane, bind_token, session_hint)),
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
            C::FeishuTest { text } => self.feishu_test(&text).await,
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
    let pid_path = cfg.state_dir.join("batond.pid");
    write_private(&pid_path, std::process::id().to_string().as_bytes())?;
    write_private(
        &cfg.state_dir.join("web.token"),
        core.web_token().as_bytes(),
    )?;
    log::info!(
        "batond {} listening on {} (web {}:{})",
        VERSION,
        cfg.socket.display(),
        core.config().web.bind,
        core.config().web.port
    );

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
    {
        let c = core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SNAPSHOT_INTERVAL);
            loop {
                tick.tick().await;
                c.flush_snapshot();
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
    core.flush_snapshot();
    for p in [&cfg.socket, &pid_path] {
        if let Err(e) = std::fs::remove_file(p) {
            log::warn!("cleanup {}: {e}", p.display());
        }
    }
    Ok(())
}
