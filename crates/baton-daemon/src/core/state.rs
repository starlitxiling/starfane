//! Persistent daemon state (serialized as `<state>/state.json`).

use baton_proto::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Debug, Clone)]
pub(crate) struct PendingAwayItem {
    pub session: SessionRecord,
    pub notice: crate::notice::Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InboxEntry {
    pub item: InboxItem,
    #[serde(default)]
    pub delivered: bool,
    /// Pointer not yet confirmed; retry L2 on the next idle transition.
    #[serde(default)]
    pub poke_pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredArtifact {
    pub payload: ArtifactPayload,
    #[serde(default)]
    pub project_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredReview {
    pub record: ReviewRecord,
    pub project_root: String,
    /// Tree G2 verifies against (worktree or project root).
    pub repo_dir: String,
    pub reviewer: AuthorRef,
    #[serde(default)]
    pub pipeline_id: Option<PipelineId>,
    /// Session currently asked to falsify (if any).
    #[serde(default)]
    pub falsifier_session: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BindPurpose {
    Stage {
        pipeline_id: PipelineId,
        stage: StageKind,
    },
    Falsifier {
        review_id: ReviewId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingBind {
    pub purpose: BindPurpose,
    pub agent: AgentKind,
    #[serde(default)]
    pub pane: Option<PaneId>,
    /// Filled once the spawned session's first hook arrives.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub base: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PaneWatch {
    pub last_hash: u64,
    pub stable_ticks: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    #[serde(default)]
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    #[serde(default)]
    pub pipelines: BTreeMap<PipelineId, PipelineSpec>,
    #[serde(default)]
    pub handoffs: BTreeMap<HandoffId, HandoffPacket>,
    #[serde(default)]
    pub inbox: BTreeMap<SessionId, Vec<InboxEntry>>,
    #[serde(default)]
    pub artifacts: BTreeMap<ArtifactId, StoredArtifact>,
    #[serde(default)]
    pub reviews: BTreeMap<ReviewId, StoredReview>,
    #[serde(default)]
    pub binds: BTreeMap<BindToken, PendingBind>,
    #[serde(default)]
    pub worktrees: BTreeMap<PipelineId, WorktreeInfo>,
    #[serde(default)]
    pub feishu_muted_until: Option<u64>,
    /// Feishu message id → pipeline (thread routing).
    #[serde(default)]
    pub feishu_msgs: BTreeMap<String, PipelineId>,
    /// Feishu session-notification card message id → session reply target (bounded to 512 entries).
    #[serde(default)]
    pub feishu_session_msgs: BTreeMap<String, crate::notice::SessionReplyTarget>,
    /// Insertion order for bounded pruning of `feishu_session_msgs`.
    #[serde(default)]
    pub feishu_session_msg_order: Vec<String>,
    /// Session id → root Feishu message id for continuous per-session thread replies (bounded to 512 entries).
    #[serde(default)]
    pub feishu_session_threads: BTreeMap<SessionId, String>,
    /// Insertion order for bounded pruning of `feishu_session_threads`.
    #[serde(default)]
    pub feishu_session_thread_order: Vec<SessionId>,
    #[serde(default)]
    pub unconfirmed: Vec<AttentionItem>,
    /// (session, kind) → last notice ms.
    #[serde(skip)]
    pub cooldown: HashMap<(SessionId, EventKind), u64>,
    /// Cross-session 30s notice content dedup: `(agent, norm_cwd, pane_or_empty, kind, norm_body_prefix_120) -> last_sent_ms`.
    #[serde(skip)]
    pub notice_fingerprints:
        HashMap<(AgentKind, String, String, crate::notice::NoticeKind, String), u64>,
    /// cwd → git root cache.
    #[serde(skip)]
    pub git_roots: HashMap<String, Option<String>>,
    /// Per-pane output hash tracker for `discovered_only` legacy sessions.
    #[serde(skip)]
    pub pane_watch: HashMap<PaneId, PaneWatch>,
    /// File `(mtime_ns, len)` cache for transcript-derived `SessionMetrics` (zero disk re-read when unchanged).
    #[serde(skip)]
    pub metrics_cache: HashMap<std::path::PathBuf, crate::core::metrics::CachedFileMetrics>,
    /// Cached read-only snapshot of `~/.cc-switch/cc-switch.db` latest per-session metrics.
    #[serde(skip)]
    pub cc_switch_cache: crate::core::metrics::CcSwitchCache,
    /// Cached read-only snapshot of `~/.codex/state_5.sqlite` threads for exact pane-title matching.
    #[serde(skip)]
    pub codex_threads_cache: crate::core::metrics::CodexThreadCache,
    /// Cached read-only snapshot of `~/.gemini/antigravity-cli/conversation_summaries.db` root sessions.
    #[serde(skip)]
    pub agy_convs_cache: crate::core::metrics::AgyConvCache,
    /// Cached PID -> AGY `conversation_id` mapping derived from open `presence/<uuid>.lock` handles.
    /// Cached read-only snapshot of `~/.gemini/antigravity-cli/presence/<uuid>.lock` PIDs.
    #[serde(skip)]
    pub agy_presence_cache: crate::core::metrics::AgyPresenceCache,
    /// File `(mtime_ns, len)` cache for parsed session transcripts (avoids re-parsing huge JSONLs).
    #[serde(skip)]
    pub transcript_cache: HashMap<std::path::PathBuf, crate::core::transcript::CachedTranscript>,
    /// Deduplication cache for `mirror_tmux_status` (`@baton_state` / `@agent_state` etc.).
    #[serde(skip)]
    pub last_mirrored_tmux: HashMap<PaneId, (SessionState, Option<StageKind>, Option<PipelineId>)>,
    /// Coalesced turn-complete notices waiting for the shared away-digest timer.
    #[serde(skip)]
    pub pending_away_notices: BTreeMap<SessionId, PendingAwayItem>,
    /// Whether an away-digest flush task is currently armed.
    #[serde(skip)]
    pub away_digest_scheduled: bool,
    /// Sliding window timestamps (ms) of outbound Feishu notices for burst rate limiting.
    #[serde(skip)]
    pub feishu_sent_ms: VecDeque<u64>,
    /// Timestamp (ms) when a rate-limit summary notice was last emitted for the current window.
    #[serde(skip)]
    pub feishu_rate_warned_window_ms: Option<u64>,
}

/// True when a subagent session is actively running or awaiting interaction (`Ended`/`Idle`/`Error` are inactive).
pub(crate) fn is_subagent_active(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Starting
            | SessionState::Working
            | SessionState::NeedsPermission
            | SessionState::NeedsInput
    )
}

/// True when pane binding came from an exact signal rather than CWD fallback or unknown.
pub(crate) fn is_exact_pane_resolution(res: PaneResolution) -> bool {
    matches!(
        res,
        PaneResolution::BindToken | PaneResolution::Env | PaneResolution::PidTree
    )
}

/// Detach any pane and stale tmux target/label metadata from `rec`.
pub(crate) fn clear_tmux_binding(rec: &mut SessionRecord) {
    rec.pane = None;
    rec.pane_resolution = PaneResolution::Unknown;
    rec.tmux_target = None;
    rec.tmux_label = None;
}

impl State {
    /// Inbox key for a spawned session whose id is not known yet.
    pub fn bind_inbox_key(token: &BindToken) -> SessionId {
        SessionId(format!("bind:{}", token.0))
    }

    pub fn push_inbox(&mut self, session: &SessionId, item: InboxItem) {
        self.inbox
            .entry(session.clone())
            .or_default()
            .push(InboxEntry {
                item,
                delivered: false,
                poke_pending: true,
            });
        self.recount_unread(session);
    }

    pub fn recount_unread(&mut self, session: &SessionId) {
        let unread = self
            .inbox
            .get(session)
            .map(|v| v.iter().filter(|e| !e.delivered).count() as u32)
            .unwrap_or(0);
        if let Some(s) = self.sessions.get_mut(session) {
            s.unread_inbox = unread;
        }
    }

    /// Move inbox entries from a bind key to the real session id.
    pub fn adopt_bind_inbox(&mut self, token: &BindToken, session: &SessionId) {
        if let Some(entries) = self.inbox.remove(&Self::bind_inbox_key(token)) {
            self.inbox
                .entry(session.clone())
                .or_default()
                .extend(entries);
            self.recount_unread(session);
        }
    }

    /// Migrate Feishu session-thread root and reply-target mappings when a legacy
    /// `tmux-%N` session record is upgraded/merged into a real hook `session_id`.
    pub fn migrate_session_feishu_thread(&mut self, from: &SessionId, to: &SessionId) {
        if from == to {
            return;
        }
        if let Some(root_mid) = self.feishu_session_threads.remove(from) {
            self.feishu_session_thread_order.retain(|s| s != from);
            if !self.feishu_session_threads.contains_key(to) {
                self.feishu_session_threads.insert(to.clone(), root_mid);
                self.feishu_session_thread_order.push(to.clone());
            }
        }
        for target in self.feishu_session_msgs.values_mut() {
            if target.session_id == *from {
                target.session_id = to.clone();
            }
        }
    }

    /// Merge a synthetic `discovered_only` legacy record (`leg_id`) into a real hook-backed
    /// record (`real_id`), inheriting tmux pane/label/metadata, inbox items, and Feishu thread.
    pub fn merge_legacy_into_real(&mut self, leg_id: &SessionId, real_id: &SessionId) -> bool {
        if leg_id == real_id || !self.sessions.contains_key(real_id) {
            return false;
        }
        let Some(legacy) = self.sessions.remove(leg_id) else {
            return false;
        };
        if let Some(p) = &legacy.pane {
            self.pane_watch.remove(p);
        }
        if let Some(entries) = self.inbox.remove(leg_id) {
            self.inbox
                .entry(real_id.clone())
                .or_default()
                .extend(entries);
            self.recount_unread(real_id);
        }
        self.migrate_session_feishu_thread(leg_id, real_id);
        self.reparent_children(leg_id, real_id);
        if let Some(real) = self.sessions.get_mut(real_id) {
            if real.pane.is_none() && legacy.pane.is_some() {
                real.pane = legacy.pane;
                real.pane_resolution = legacy.pane_resolution;
            }
            if real.tmux_target.is_none() && legacy.tmux_target.is_some() {
                real.tmux_target = legacy.tmux_target;
            }
            if real.tmux_label.is_none() && legacy.tmux_label.is_some() {
                real.tmux_label = legacy.tmux_label;
            }
            if real.git_root.is_none() && legacy.git_root.is_some() {
                real.git_root = legacy.git_root;
            }
            if real.model.is_none() && legacy.model.is_some() {
                real.model = legacy.model;
            }
            if real.effort.is_none() && legacy.effort.is_some() {
                real.effort = legacy.effort;
            }
            if real.transcript_path.is_none() && legacy.transcript_path.is_some() {
                real.transcript_path = legacy.transcript_path;
            }
            if real.metrics.is_none() && legacy.metrics.is_some() {
                real.metrics = legacy.metrics;
            }
        }
        true
    }

    /// Reparent any child subagent records pointing at `from_parent` onto `to_parent`,
    /// keeping `subagent_count` and `active_subagents` consistent.
    pub fn reparent_children(&mut self, from_parent: &SessionId, to_parent: &SessionId) {
        if from_parent == to_parent {
            return;
        }
        let mut total: u32 = 0;
        let mut active: u32 = 0;
        for s in self.sessions.values_mut() {
            if s.session_id == *to_parent {
                continue;
            }
            if s.parent_session_id.as_ref() == Some(from_parent) {
                s.parent_session_id = Some(to_parent.clone());
            }
            if s.parent_session_id.as_ref() == Some(to_parent) && s.is_subagent {
                total += 1;
                if is_subagent_active(s.state) {
                    active += 1;
                }
            }
        }
        if let Some(parent) = self.sessions.get_mut(to_parent) {
            if total > 0 {
                parent.subagent_count = parent.subagent_count.max(total);
                parent.active_subagents = active;
            }
        }
    }

    /// Self-heal an AGY root session that was mistakenly folded as a subagent (e.g. under a
    /// synthetic `tmux-%N` record in pre-v0.1.12 snapshots) once `presence/<conv_id>.lock`
    /// proves it is the live root conversation of `pane`.
    /// Also retires any rival non-`Ended` AGY roots on `pane` that lack ownership.
    /// Returns `Some(old_parent_if_different)` when promotion happened.
    pub fn promote_misfolded_agy_root(
        &mut self,
        root_id: &SessionId,
        leg_id: &SessionId,
        pane: &PaneId,
    ) -> Option<Option<SessionId>> {
        let needs_promote = self
            .sessions
            .get(root_id)
            .is_some_and(|s| s.agent == AgentKind::Agy && s.is_subagent);
        if !needs_promote {
            return None;
        }
        for s in self.sessions.values_mut() {
            if s.session_id != *root_id
                && s.session_id != *leg_id
                && !s.is_subagent
                && s.agent == AgentKind::Agy
                && s.pane.as_ref() == Some(pane)
                && s.state != SessionState::Ended
            {
                s.state = SessionState::Ended;
                clear_tmux_binding(s);
            }
        }
        let old_parent = self.sessions.get_mut(root_id).and_then(|rec| {
            let prev = rec.parent_session_id.take();
            rec.is_subagent = false;
            rec.discovered_only = false;
            rec.confidence = Confidence::Exact;
            rec.pane = Some(pane.clone());
            prev
        });
        self.merge_legacy_into_real(leg_id, root_id);
        let updated_old_parent = old_parent.filter(|op| op != leg_id && op != root_id);
        if let Some(op) = &updated_old_parent {
            let (mut total, mut active) = (0u32, 0u32);
            for s in self.sessions.values() {
                if s.is_subagent && s.parent_session_id.as_ref() == Some(op) {
                    total += 1;
                    if is_subagent_active(s.state) {
                        active += 1;
                    }
                }
            }
            if let Some(parent) = self.sessions.get_mut(op) {
                parent.subagent_count = total;
                parent.active_subagents = active;
            }
        }
        Some(updated_old_parent)
    }

    /// Deduplicate any split `(discovered_only tmux-%N, real hook session)` pairs that share
    /// the same pane or transcript/rollout UUID (used on startup snapshot load).
    pub fn dedup_split_sessions(&mut self) -> bool {
        let real_ids: Vec<(SessionId, AgentKind, Option<PaneId>, Option<String>)> = self
            .sessions
            .values()
            .filter(|s| !s.discovered_only && !s.is_subagent && s.state != SessionState::Ended)
            .map(|s| {
                (
                    s.session_id.clone(),
                    s.agent,
                    s.pane.clone(),
                    s.transcript_path.clone(),
                )
            })
            .collect();
        let mut pairs: Vec<(SessionId, SessionId)> = Vec::new();
        for (real_id, agent, pane, tp) in &real_ids {
            for leg in self.sessions.values() {
                if !leg.discovered_only || leg.is_subagent || leg.agent != *agent {
                    continue;
                }
                let pane_match = pane.is_some() && leg.pane == *pane;
                let tr_match = transcript_matches_session(
                    *agent,
                    real_id,
                    tp.as_deref(),
                    leg.transcript_path.as_deref(),
                );
                if pane_match || tr_match {
                    pairs.push((leg.session_id.clone(), real_id.clone()));
                }
            }
        }
        let mut changed = false;
        for (leg_id, real_id) in pairs {
            if self.merge_legacy_into_real(&leg_id, &real_id) {
                changed = true;
            }
        }
        changed
    }

    /// Clear stale tmux metadata on non-tmux (`pane == None`) sessions that previously held a
    /// `tmux_target`, `tmux_label`, or speculative `CwdMatch` binding before being detached.
    pub fn sanitize_orphan_tmux_metadata(&mut self) -> bool {
        let mut changed = false;
        for s in self.sessions.values_mut() {
            if s.is_subagent || s.discovered_only || s.pane.is_some() {
                continue;
            }
            let has_tmux_meta = s.tmux_target.is_some() || s.tmux_label.is_some();
            if s.state != SessionState::Ended {
                if has_tmux_meta || s.pane_resolution != PaneResolution::Unknown {
                    clear_tmux_binding(s);
                    changed = true;
                }
            } else if s.pane_resolution == PaneResolution::CwdMatch && has_tmux_meta {
                clear_tmux_binding(s);
                changed = true;
            }
        }
        changed
    }

    /// Prune already-delivered or pre-v0.1.11 already-injected (`poke_pending == false`)
    /// `InboxKind::Message` ghost entries from persisted snapshots and refresh `unread_inbox`.
    pub fn cleanup_stale_message_inbox(&mut self) -> bool {
        let mut changed = false;
        let sids: Vec<SessionId> = self.inbox.keys().cloned().collect();
        for sid in sids {
            if let Some(entries) = self.inbox.get_mut(&sid) {
                let before = entries.len();
                entries.retain(|e| {
                    !(e.item.kind == InboxKind::Message && (e.delivered || !e.poke_pending))
                });
                if entries.len() != before {
                    changed = true;
                }
            }
            if self.inbox.get(&sid).is_some_and(Vec::is_empty) {
                self.inbox.remove(&sid);
            }
            let prev_unread = self.sessions.get(&sid).map(|s| s.unread_inbox).unwrap_or(0);
            self.recount_unread(&sid);
            let next_unread = self.sessions.get(&sid).map(|s| s.unread_inbox).unwrap_or(0);
            if prev_unread != next_unread {
                changed = true;
            }
        }
        changed
    }

    pub fn session_for_pane(&self, pane: &PaneId) -> Option<&SessionRecord> {
        self.sessions
            .values()
            .filter(|s| {
                !s.is_subagent && s.pane.as_ref() == Some(pane) && s.state != SessionState::Ended
            })
            .max_by_key(|s| {
                (
                    is_exact_pane_resolution(s.pane_resolution),
                    !s.discovered_only,
                    s.last_event_ms,
                )
            })
    }

    /// Known project roots (from sessions and pipelines).
    pub fn project_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .sessions
            .values()
            .filter_map(|s| s.git_root.clone())
            .chain(self.pipelines.values().map(|p| p.project.root.clone()))
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }
}

/// True when a `discovered_only` session's `transcript_path` matches a real hook session either
/// by exact path equality or by embedded rollout/session UUID in the filename.
pub(crate) fn transcript_matches_session(
    agent: AgentKind,
    real_sid: &SessionId,
    real_tp: Option<&str>,
    candidate_tp: Option<&str>,
) -> bool {
    let Some(cand) = candidate_tp.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if let Some(rtp) = real_tp.map(str::trim).filter(|s| !s.is_empty()) {
        if rtp == cand {
            return true;
        }
    }
    let sid = real_sid.as_str().trim();
    if sid.is_empty() || sid.starts_with("tmux-") {
        return false;
    }
    if crate::core::metrics::extract_session_id_from_transcript_path(
        agent,
        std::path::Path::new(cand),
    )
    .as_deref()
        == Some(sid)
    {
        return true;
    }
    sid.len() >= 12
        && std::path::Path::new(cand)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|fname| fname.contains(sid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_rec(
        id: &str,
        agent: AgentKind,
        pane: Option<&str>,
        discovered_only: bool,
    ) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(id),
            agent,
            pane: pane.map(PaneId::new),
            pane_resolution: PaneResolution::Env,
            cwd: Some("/Users/demo/work/app-a".into()),
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
            confidence: if discovered_only {
                Confidence::Heuristic
            } else {
                Confidence::Exact
            },
            tmux_target: None,
            tmux_label: None,
            discovered_only,
            transcript_path: None,
            metrics: None,
        }
    }

    #[test]
    fn promotes_misfolded_agy_root_and_reparents_children() {
        let mut st = State::default();
        let leg_id = SessionId::new("tmux-%17");
        let root_id = SessionId::new("66fac8b3-93ac-4c7d-814c-b79bb0ec9c28");
        let child_id = SessionId::new("sub-worker-1");
        let pane17 = PaneId::new("%17");

        let mut leg = mk_rec(leg_id.as_str(), AgentKind::Agy, Some("%17"), true);
        leg.tmux_target = Some("prome:0.0".into());
        leg.tmux_label = Some("prome-prometheus-ros".into());
        leg.subagent_count = 2;
        leg.active_subagents = 2;
        st.sessions.insert(leg_id.clone(), leg);

        let mut root = mk_rec(root_id.as_str(), AgentKind::Agy, Some("%17"), false);
        root.state = SessionState::Working;
        root.is_subagent = true;
        root.parent_session_id = Some(leg_id.clone());
        st.sessions.insert(root_id.clone(), root);

        let mut child = mk_rec(child_id.as_str(), AgentKind::Agy, Some("%17"), false);
        child.state = SessionState::Working;
        child.is_subagent = true;
        child.parent_session_id = Some(leg_id.clone());
        st.sessions.insert(child_id.clone(), child);

        let mut rival = mk_rec("rival-cwd-hijack", AgentKind::Agy, Some("%17"), false);
        rival.pane_resolution = PaneResolution::CwdMatch;
        rival.tmux_target = Some("prome:0.0".into());
        rival.tmux_label = Some("prome-prometheus-ros".into());
        rival.last_event_ms = 9999;
        st.sessions.insert(rival.session_id.clone(), rival);

        let promoted = st.promote_misfolded_agy_root(&root_id, &leg_id, &pane17);
        assert!(promoted.is_some());
        assert!(!st.sessions.contains_key(&leg_id));
        let rival_after = st
            .sessions
            .get(&SessionId::new("rival-cwd-hijack"))
            .unwrap();
        assert_eq!(rival_after.state, SessionState::Ended);
        assert!(rival_after.pane.is_none());
        assert_eq!(rival_after.pane_resolution, PaneResolution::Unknown);
        assert!(rival_after.tmux_target.is_none());
        assert!(rival_after.tmux_label.is_none());

        let root_after = st.sessions.get(&root_id).expect("promoted root");
        assert!(!root_after.is_subagent);
        assert!(root_after.parent_session_id.is_none());
        assert!(!root_after.discovered_only);
        assert_eq!(root_after.pane.as_ref(), Some(&pane17));
        assert_eq!(
            root_after.tmux_label.as_deref(),
            Some("prome-prometheus-ros")
        );
        assert_eq!(root_after.subagent_count, 1);
        assert_eq!(root_after.active_subagents, 1);

        let child_after = st.sessions.get(&child_id).expect("reparented child");
        assert!(child_after.is_subagent);
        assert_eq!(child_after.parent_session_id.as_ref(), Some(&root_id));
    }

    #[test]
    fn sanitizes_orphan_tmux_metadata_without_touching_live_panes() {
        let mut st = State::default();

        let mut ghost_live = mk_rec("ghost-live", AgentKind::Agy, None, false);
        ghost_live.pane_resolution = PaneResolution::CwdMatch;
        ghost_live.tmux_target = Some("learning:7.0".into());
        ghost_live.tmux_label = Some("learning-cloud-server".into());
        st.sessions
            .insert(ghost_live.session_id.clone(), ghost_live);

        let mut ghost_ended = mk_rec("ghost-ended", AgentKind::Agy, None, false);
        ghost_ended.state = SessionState::Ended;
        ghost_ended.pane_resolution = PaneResolution::CwdMatch;
        ghost_ended.tmux_target = Some("learning:7.0".into());
        ghost_ended.tmux_label = Some("learning-cloud-server".into());
        st.sessions
            .insert(ghost_ended.session_id.clone(), ghost_ended);

        let mut live_exact = mk_rec("live-exact", AgentKind::Agy, Some("%42"), false);
        live_exact.tmux_target = Some("learning:7.0".into());
        live_exact.tmux_label = Some("learning-cloud-server".into());
        st.sessions
            .insert(live_exact.session_id.clone(), live_exact);

        let mut ended_exact = mk_rec("ended-exact", AgentKind::Claude, None, false);
        ended_exact.state = SessionState::Ended;
        ended_exact.pane_resolution = PaneResolution::Env;
        ended_exact.tmux_target = Some("learning:2.1".into());
        ended_exact.tmux_label = Some("learning-openkernel".into());
        st.sessions
            .insert(ended_exact.session_id.clone(), ended_exact);

        assert!(st.sanitize_orphan_tmux_metadata());
        assert!(!st.sanitize_orphan_tmux_metadata());

        let gl = &st.sessions[&SessionId::new("ghost-live")];
        assert!(gl.pane.is_none());
        assert_eq!(gl.pane_resolution, PaneResolution::Unknown);
        assert!(gl.tmux_target.is_none());
        assert!(gl.tmux_label.is_none());

        let ge = &st.sessions[&SessionId::new("ghost-ended")];
        assert_eq!(ge.pane_resolution, PaneResolution::Unknown);
        assert!(ge.tmux_target.is_none());
        assert!(ge.tmux_label.is_none());

        let le = &st.sessions[&SessionId::new("live-exact")];
        assert_eq!(le.pane.as_ref().map(|p| p.as_str()), Some("%42"));
        assert_eq!(le.tmux_target.as_deref(), Some("learning:7.0"));

        let ee = &st.sessions[&SessionId::new("ended-exact")];
        assert_eq!(ee.tmux_target.as_deref(), Some("learning:2.1"));
    }
}
