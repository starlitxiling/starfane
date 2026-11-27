//! Hook ingest: normalize → pane resolution → session tree update → fan-out.

use super::state::{is_exact_pane_resolution, BindPurpose, State};
use super::{lock, util, Core};
use starfane_proto::*;
use std::collections::HashSet;
use std::path::Path;

/// Everything the async part needs after the state mutation.
struct Applied {
    updated: Vec<SessionRecord>,
    /// Session whose state the router should consider (root or folded/sub).
    route: Option<(SessionRecord, bool)>,
    /// Optional synthesized/overridden event to pass to router (e.g. parent TurnComplete on AGY idle).
    route_event: Option<NormalizedEvent>,
    stage: Option<StageRef>,
    bound_now: Option<BindPurpose>,
}

/// True when a Codex hook event or persisted record belongs to Codex's internal background
/// memory-consolidation worker (`~/.codex/memories`) or ephemeral rootless probe (`cwd="/"`).
pub(crate) fn is_codex_memory_task(
    agent: AgentKind,
    cwd: Option<&str>,
    transcript_path: Option<&str>,
) -> bool {
    if agent != AgentKind::Codex {
        return false;
    }
    if cwd.map(str::trim) == Some("/") && transcript_path.is_none() {
        return true;
    }
    let cwd_hit = cwd.is_some_and(|c| {
        let norm = c.replace('\\', "/");
        norm.ends_with("/.codex/memories") || norm.contains("/.codex/memories/")
    });
    if cwd_hit {
        return true;
    }
    transcript_path.is_some_and(|p| {
        let norm = p.replace('\\', "/");
        norm.contains("/.codex/memories/")
            || (norm.contains("/.codex/") && norm.contains("memory_summary"))
    })
}

impl Core {
    pub async fn ingest(&self, req: EmitRequest) {
        let ev = match starfane_adapters::normalize_request(&req) {
            Ok(ev) => ev,
            Err(e) => {
                log::warn!(
                    "dropping {} hook {:?}: {e}",
                    req.agent,
                    req.event_name().unwrap_or("?")
                );
                return;
            }
        };
        if is_codex_memory_task(ev.agent, ev.cwd.as_deref(), ev.transcript_path.as_deref()) {
            log::debug!(
                "ignoring codex internal memory task {} (cwd={:?})",
                ev.session_id,
                ev.cwd
            );
            return;
        }
        if ev.agent == AgentKind::Codex
            && ev.detail.last_message.as_deref().map(str::trim) == Some("{\"suggestions\":[]}")
        {
            log::debug!(
                "ignoring codex background suggestion probe {}",
                ev.session_id
            );
            return;
        }
        if let Err(e) = self.inner.store.append_event(&ev) {
            log::error!("event log write failed: {e}");
        }

        let git_root = match ev.cwd.clone() {
            Some(cwd) => self.git_root_cached(cwd).await,
            None => None,
        };
        let (pane, resolution) = self.resolve_event_pane(&ev, &req.env).await;

        let applied = {
            let mut st = lock(&self.inner.state);
            apply_event(&mut st, &ev, git_root, pane, resolution)
        };
        if matches!(
            ev.kind,
            EventKind::SessionStart
                | EventKind::TurnComplete
                | EventKind::NeedsInput
                | EventKind::NeedsPermission
                | EventKind::Interrupted
        ) {
            for s in &applied.updated {
                if s.is_local_host() {
                    if let Some(ref p) = s.pane {
                        lock(&self.inner.discovery_cache).invalidate_tail_cache(p);
                    }
                }
            }
        }
        self.mark_dirty();

        self.publish(|seq| DaemonEvent::Event {
            seq,
            event: ev.clone(),
        });
        for s in &applied.updated {
            self.publish_session(s.clone());
        }
        if ev.kind == EventKind::Working {
            if let Some(s) = applied.updated.first() {
                self.resolve_acks(&s.session_id);
            }
            self.clear_unconfirmed(&ev.session_id);
        }
        if let Some(purpose) = applied.bound_now {
            self.on_spawn_bound(purpose, &ev.session_id).await;
        }
        if let Some((session, is_sub)) = applied.route {
            let route_ev = applied.route_event.as_ref().unwrap_or(&ev);
            self.on_session_event(&session, route_ev, applied.stage.clone(), is_sub)
                .await;
        }
        if matches!(
            ev.kind,
            EventKind::SessionStart | EventKind::TurnComplete | EventKind::Interrupted
        ) || ev.transcript_path.is_some()
        {
            self.refresh_session_metrics(&ev.session_id).await;
        }
        self.refresh_attention();
    }

    async fn git_root_cached(&self, cwd: String) -> Option<String> {
        if let Some(hit) = lock(&self.inner.state).git_roots.get(&cwd) {
            return hit.clone();
        }
        let c = cwd.clone();
        let root = tokio::task::spawn_blocking(move || util::git_root(Path::new(&c)))
            .await
            .unwrap_or(None);
        lock(&self.inner.state).git_roots.insert(cwd, root.clone());
        root
    }

    /// Bind token → env pane → tmux pid-tree / cwd resolution (first sight only).
    /// Blocks non-tmux Claude/AGY processes and already-claimed panes from unsafe `CwdMatch` hijacking.
    async fn resolve_event_pane(
        &self,
        ev: &NormalizedEvent,
        env: &EnvCtx,
    ) -> (Option<PaneId>, PaneResolution) {
        let (existing_cwd_pane, claimed_panes): (Option<PaneId>, HashSet<PaneId>) = {
            let st = lock(&self.inner.state);
            if let Some(tok) = &ev.bind_token {
                if let Some(b) = st.binds.get(tok) {
                    if b.pane.is_some() {
                        return (b.pane.clone(), PaneResolution::BindToken);
                    }
                }
            }
            if let Some(p) = &ev.pane {
                return (Some(p.clone()), PaneResolution::Env);
            }
            let mut existing_cwd_pane = None;
            if let Some(s) = st.sessions.get(&ev.session_id) {
                if s.pane.is_some() && is_exact_pane_resolution(s.pane_resolution) {
                    return (s.pane.clone(), s.pane_resolution);
                }
                if s.pane.is_some() {
                    existing_cwd_pane = s.pane.clone();
                }
            }
            let claimed = st
                .sessions
                .values()
                .filter(|s| {
                    if s.session_id == ev.session_id
                        || s.is_subagent
                        || s.state == SessionState::Ended
                    {
                        return false;
                    }
                    if !s.discovered_only {
                        return true;
                    }
                    // Prevent `CwdMatch` from stealing a `discovered_only` placeholder pane that
                    // belongs to a different agent or already holds a different session's transcript.
                    s.agent != ev.agent
                        || (s.transcript_path.is_some()
                            && !super::state::transcript_matches_session(
                                ev.agent,
                                &ev.session_id,
                                ev.transcript_path.as_deref(),
                                s.transcript_path.as_deref(),
                            ))
                })
                .filter_map(|s| s.pane.clone())
                .collect();
            (existing_cwd_pane, claimed)
        };
        let has_mux_env = env.has_mux_env();
        let core = self.clone();
        let hint = starfane_tmux::PaneHint {
            agent: ev.agent,
            env_pane: None,
            pid_chain: Vec::new(),
            cwd: ev.cwd.clone(),
        };
        let ppid = env.ppid;
        let agent = ev.agent;
        tokio::task::spawn_blocking(move || {
            let panes = match core.inner.mux.list_all_panes() {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("pane resolution skipped: {e}");
                    return match existing_cwd_pane {
                        Some(p) if !claimed_panes.contains(&p) => {
                            (Some(p), PaneResolution::CwdMatch)
                        }
                        _ => (None, PaneResolution::Unknown),
                    };
                }
            };
            let mut hint = hint;
            if ppid > 0 {
                hint.pid_chain = starfane_tmux::pid_ancestors(ppid, 16);
            }
            let (pane, res) = starfane_tmux::resolve_pane(&panes, &hint);
            if res == PaneResolution::CwdMatch {
                if matches!(agent, AgentKind::Claude | AgentKind::Agy) && !has_mux_env {
                    return (None, PaneResolution::Unknown);
                }
                if pane.as_ref().is_some_and(|p| claimed_panes.contains(p)) {
                    return (None, PaneResolution::Unknown);
                }
            }
            if pane.is_none() {
                if let Some(prev) = existing_cwd_pane {
                    if !claimed_panes.contains(&prev)
                        && panes.iter().any(|p| !p.dead && p.pane_id == prev)
                    {
                        return (Some(prev), PaneResolution::CwdMatch);
                    }
                }
            }
            (pane, res)
        })
        .await
        .unwrap_or((None, PaneResolution::Unknown))
    }

    fn clear_unconfirmed(&self, session: &SessionId) {
        let mut st = lock(&self.inner.state);
        let before = st.unconfirmed.len();
        st.unconfirmed
            .retain(|a| a.session_id.as_ref() != Some(session));
        if st.unconfirmed.len() != before {
            drop(st);
            self.mark_dirty();
        }
    }
}

fn canonical_event_transcript_path(agent: AgentKind, raw: Option<&str>) -> Option<String> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty())?;
    if agent == AgentKind::Agy {
        Some(
            super::transcript::canonicalize_agy_transcript_path(Path::new(trimmed))
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        Some(trimmed.to_string())
    }
}

fn new_record(
    session_id: SessionId,
    ev: &NormalizedEvent,
    pane: Option<PaneId>,
    resolution: PaneResolution,
    git_root: Option<String>,
) -> SessionRecord {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let effort = super::metrics::infer_session_effort(
        ev.agent,
        ev.model.as_deref(),
        None,
        None,
        home.as_deref(),
    );
    let mux_kind = pane.as_ref().map(|p| {
        if p.as_str().starts_with("zj:") {
            "zellij".to_string()
        } else {
            "tmux".to_string()
        }
    });
    SessionRecord {
        session_id,
        agent: ev.agent,
        pane,
        pane_resolution: resolution,
        cwd: ev.cwd.clone(),
        git_root,
        model: ev.model.clone(),
        effort,
        state: SessionState::Unknown,
        state_since_ms: ev.ts_ms,
        last_event_id: None,
        last_event_ms: ev.ts_ms,
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
        transcript_path: canonical_event_transcript_path(ev.agent, ev.transcript_path.as_deref()),
        metrics: None,
        host_id: None,
        host_display_name: None,
        mux_kind,
        resume_id: None,
        suspended_at_ms: None,
    }
}

fn touch(rec: &mut SessionRecord, ev: &NormalizedEvent, next: SessionState) {
    if rec.state != next {
        rec.state = next;
        rec.state_since_ms = ev.ts_ms;
    }
    if ev.kind == EventKind::Error {
        if let Some(err) = ev
            .detail
            .error
            .as_deref()
            .map(|s| util::sanitize_line(s, 500))
            .filter(|s| !s.is_empty())
        {
            rec.last_error = Some(err);
        }
    } else if matches!(
        ev.kind,
        EventKind::Working
            | EventKind::ToolStart
            | EventKind::ToolEnd
            | EventKind::TurnComplete
            | EventKind::SessionEnd
    ) {
        rec.last_error = None;
    }
    rec.last_event_id = Some(ev.id.clone());
    rec.last_event_ms = ev.ts_ms;
    if let Some(m) = &ev.detail.last_message {
        let cleaned = super::transcript::strip_codex_internal_tags(m);
        if !cleaned.is_empty() {
            rec.last_message = Some(cleaned);
        }
    }
    if let Some(m) = ev.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let is_worker_model =
            !rec.is_subagent && rec.agent == AgentKind::Agy && m == "gemini-flash-worker-agy";
        let has_existing_model = rec
            .model
            .as_deref()
            .is_some_and(|cur| !cur.trim().is_empty());
        if !(is_worker_model && has_existing_model) {
            rec.model = Some(m.to_string());
        }
        if rec.agent == AgentKind::Agy {
            if let Some(eff) = rec
                .model
                .as_deref()
                .and_then(super::metrics::extract_agy_effort_from_model)
            {
                rec.effort = Some(eff);
            }
        }
    }
    if rec.cwd.is_none() {
        rec.cwd = ev.cwd.clone();
    }
    if let Some(tp) = canonical_event_transcript_path(rec.agent, ev.transcript_path.as_deref()) {
        rec.transcript_path = Some(tp);
    }
}

fn prune_unconfirmed(st: &mut State, session: &SessionId, pane: Option<&PaneId>) {
    st.unconfirmed.retain(|a| {
        a.session_id.as_ref() != Some(session)
            && pane.is_none_or(|p| a.session_id != Some(super::discover::legacy_session_id(p)))
    });
}

fn apply_event(
    st: &mut State,
    ev: &NormalizedEvent,
    git_root: Option<String>,
    pane: Option<PaneId>,
    resolution: PaneResolution,
) -> Applied {
    let root_id = ev.session_id.clone();
    let mut applied = Applied {
        updated: Vec::new(),
        route: None,
        route_event: None,
        stage: None,
        bound_now: None,
    };

    // --- bind token: first hook of a spawned session -------------------
    if let Some(tok) = &ev.bind_token {
        if let Some(b) = st.binds.get_mut(tok) {
            if b.session_id.is_none() {
                b.session_id = Some(root_id.clone());
                applied.bound_now = Some(b.purpose.clone());
            }
        }
        if applied.bound_now.is_some() {
            st.adopt_bind_inbox(tok, &root_id);
        }
    }

    // --- agy heuristic folding: new conversation in a busy agy pane ----
    // Never fold under a synthetic `discovered_only` (`tmux-%N`) record: the first real
    // AGY hook on that pane IS the root conversation upgrading `tmux-%N`, not a subagent!
    let agy_parent = if ev.agent == AgentKind::Agy && !st.sessions.contains_key(&root_id) {
        pane.as_ref().and_then(|p| {
            st.sessions
                .values()
                .filter(|s| {
                    s.agent == AgentKind::Agy
                        && !s.is_subagent
                        && !s.discovered_only
                        && s.pane.as_ref() == Some(p)
                        && matches!(s.state, SessionState::Working | SessionState::Starting)
                })
                .max_by_key(|s| s.last_event_ms)
                .map(|s| s.session_id.clone())
        })
    } else {
        None
    };
    let folded_parent = agy_parent.or_else(|| {
        st.sessions
            .get(&root_id)
            .filter(|s| s.is_subagent && s.agent == AgentKind::Agy)
            .and_then(|s| s.parent_session_id.clone())
    });

    // --- claude/codex subagent -----------------------------------------
    if let Some(agent_id) = &ev.subagent_id {
        let sub_id = session::subagent_session_id(&root_id, agent_id);
        let sub_exists = st.sessions.contains_key(&sub_id);
        let parent = st.sessions.entry(root_id.clone()).or_insert_with(|| {
            new_record(
                root_id.clone(),
                ev,
                pane.clone(),
                resolution,
                git_root.clone(),
            )
        });
        if ev.kind == EventKind::SubagentStart && !sub_exists {
            parent.subagent_count += 1;
            parent.active_subagents += 1;
        }
        if ev.kind == EventKind::SubagentEnd {
            parent.active_subagents = parent.active_subagents.saturating_sub(1);
        }
        let parent_next = parent.state.next_from_subagent(ev.kind);
        touch(parent, ev, parent_next);
        let parent_snapshot = parent.clone();

        let sub = st.sessions.entry(sub_id.clone()).or_insert_with(|| {
            let mut r = new_record(
                sub_id.clone(),
                ev,
                pane.clone(),
                resolution,
                git_root.clone(),
            );
            r.parent_session_id = Some(root_id.clone());
            r.is_subagent = true;
            r
        });
        let sub_next = match ev.kind {
            EventKind::SubagentEnd => SessionState::Ended,
            EventKind::SubagentStart => SessionState::Working,
            k => sub.state.next(k),
        };
        touch(sub, ev, sub_next);
        let sub_snapshot = sub.clone();
        applied.updated.push(sub_snapshot.clone());
        applied.updated.push(parent_snapshot.clone());
        applied.stage = parent_snapshot.stage.clone();
        if ev.kind == EventKind::NeedsPermission {
            applied.route = Some((parent_snapshot, true));
        } else {
            applied.route = Some((sub_snapshot, true));
        }
        return applied;
    }

    // --- agy folded sub-conversation ------------------------------------
    if let Some(parent_id) = folded_parent {
        let is_new = !st.sessions.contains_key(&root_id);
        if is_new {
            if let Some(parent) = st.sessions.get_mut(&parent_id) {
                parent.subagent_count += 1;
                parent.active_subagents += 1;
            }
        }
        let rec = st.sessions.entry(root_id.clone()).or_insert_with(|| {
            let mut r = new_record(
                root_id.clone(),
                ev,
                pane.clone(),
                resolution,
                git_root.clone(),
            );
            r.parent_session_id = Some(parent_id.clone());
            r.is_subagent = true;
            r.confidence = Confidence::Heuristic;
            r
        });
        let next = rec.state.next(ev.kind);
        touch(rec, ev, next);
        let ended = matches!(
            next,
            SessionState::Idle | SessionState::Ended | SessionState::Error
        );
        let snapshot = rec.clone();
        applied.updated.insert(0, snapshot.clone());
        let mut parent_unblocked: Option<SessionRecord> = None;
        if let Some(parent) = st.sessions.get_mut(&parent_id) {
            let mut parent_changed = false;
            if ended && !is_new {
                parent.active_subagents = parent.active_subagents.saturating_sub(1);
                parent_changed = true;
            }
            if ev.detail.fully_idle == Some(true)
                && parent.state == SessionState::Working
                && parent.active_subagents == 0
            {
                parent.state = SessionState::Idle;
                parent.state_since_ms = ev.ts_ms;
                parent.last_error = None;
                parent_changed = true;
                parent_unblocked = Some(parent.clone());
            }
            if parent_changed {
                applied.updated.push(parent.clone());
            }
        }
        if let Some(parent_snap) = parent_unblocked {
            let mut synth = ev.clone();
            synth.kind = EventKind::TurnComplete;
            synth.session_id = parent_snap.session_id.clone();
            synth.subagent_id = None;
            applied.stage = parent_snap.stage.clone();
            applied.route = Some((parent_snap, false));
            applied.route_event = Some(synth);
            return applied;
        }
        applied.route = Some((snapshot, true));
        return applied;
    }

    // --- root session ----------------------------------------------------
    // If this pane (or transcript/rollout UUID when hook ran without `$TMUX_PANE`, e.g. Codex
    // shared app-server) previously had a synthetic `tmux-<pane_id>` discovered record,
    // upgrade/merge it seamlessly into the real hook `session_id` without duplicates.
    let mut pane = pane;
    let mut resolution = resolution;
    let mut inherited_target: Option<String> = None;
    let mut inherited_label: Option<String> = None;
    let mut inherited_model: Option<String> = None;
    let mut inherited_effort: Option<String> = None;
    let mut inherited_transcript: Option<String> = None;
    let mut inherited_metrics: Option<SessionMetrics> = None;

    if let Some(p) = pane
        .as_ref()
        .filter(|_| is_exact_pane_resolution(resolution))
    {
        let active_pipelines: HashSet<PipelineId> = st
            .pipelines
            .iter()
            .filter(|(_, pipe)| {
                !matches!(
                    pipe.status,
                    PipelineStatus::Done | PipelineStatus::Cancelled | PipelineStatus::Failed
                )
            })
            .map(|(pid, _)| pid.clone())
            .collect();
        for rival in st.sessions.values_mut() {
            if rival.session_id != root_id
                && !rival.is_subagent
                && !rival.discovered_only
                && rival.state != SessionState::Ended
                && rival.pane.as_ref() == Some(p)
            {
                let has_active_stage = rival
                    .stage
                    .as_ref()
                    .is_some_and(|sr| active_pipelines.contains(&sr.pipeline_id));
                if !has_active_stage {
                    rival.state = SessionState::Ended;
                    super::state::clear_tmux_binding(rival);
                    rival.state_since_ms = ev.ts_ms;
                    applied.updated.push(rival.clone());
                }
            }
        }
    }

    let mut matched_legacy_id: Option<SessionId> = pane
        .as_ref()
        .map(super::discover::legacy_session_id)
        .filter(|leg| {
            if leg == &root_id {
                return false;
            }
            let Some(legacy) = st.sessions.get(leg) else {
                return false;
            };
            if is_exact_pane_resolution(resolution) {
                return true;
            }
            // Under speculative `CwdMatch`, never steal a discovered-only pane that already
            // belongs to a different agent or a non-matching transcript file.
            legacy.agent == ev.agent
                && (legacy.transcript_path.is_none()
                    || super::state::transcript_matches_session(
                        ev.agent,
                        &root_id,
                        ev.transcript_path.as_deref(),
                        legacy.transcript_path.as_deref(),
                    ))
        });

    if matched_legacy_id.is_none() {
        matched_legacy_id = st
            .sessions
            .values()
            .filter(|s| s.discovered_only && !s.is_subagent && s.agent == ev.agent)
            .find(|s| {
                super::state::transcript_matches_session(
                    ev.agent,
                    &root_id,
                    ev.transcript_path.as_deref(),
                    s.transcript_path.as_deref(),
                )
            })
            .map(|s| s.session_id.clone());
    }

    if let Some(leg_id) = matched_legacy_id {
        if let Some(mut legacy) = st.sessions.remove(&leg_id) {
            if pane.is_none() && legacy.pane.is_some() {
                pane = legacy.pane.clone();
                resolution = legacy.pane_resolution;
            }
            inherited_target = legacy.tmux_target.clone();
            inherited_label = legacy.tmux_label.clone();
            inherited_model = legacy.model.clone();
            inherited_effort = legacy.effort.clone();
            inherited_transcript = legacy.transcript_path.clone();
            inherited_metrics = legacy.metrics.clone();
            if let Some(p) = &legacy.pane {
                st.pane_watch.remove(p);
            }
            if let Some(entries) = st.inbox.remove(&leg_id) {
                st.inbox.entry(root_id.clone()).or_default().extend(entries);
            }
            // Emit Ended tombstone for the superseded legacy record so SSE clients drop it immediately.
            legacy.state = SessionState::Ended;
            legacy.state_since_ms = ev.ts_ms;
            legacy.last_event_ms = ev.ts_ms;
            applied.updated.push(legacy);
        }
        st.reparent_children(&leg_id, &root_id);
        st.migrate_session_feishu_thread(&leg_id, &root_id);
    } else if let Some(p) = &pane {
        let leg_id = super::discover::legacy_session_id(p);
        if leg_id != root_id {
            st.migrate_session_feishu_thread(&leg_id, &root_id);
        }
    }

    if let Some(p) = &pane {
        if inherited_target.is_none() {
            inherited_target = super::discover::known_tmux_target(st, p);
        }
        if inherited_label.is_none() {
            inherited_label = super::discover::known_tmux_label(st, p);
        }
    }

    let rec = st.sessions.entry(root_id.clone()).or_insert_with(|| {
        new_record(
            root_id.clone(),
            ev,
            pane.clone(),
            resolution,
            git_root.clone(),
        )
    });
    rec.discovered_only = false;
    if rec.pane.is_none() && pane.is_none() {
        super::state::clear_tmux_binding(rec);
    } else if rec.pane.is_some() && pane.is_some() && is_exact_pane_resolution(resolution) {
        let pane_changed = rec.pane != pane;
        rec.pane = pane;
        rec.pane_resolution = resolution;
        if pane_changed {
            rec.tmux_target = inherited_target.take();
            rec.tmux_label = inherited_label.take();
        }
    } else if rec.pane.is_none() && pane.is_some() {
        rec.pane = pane;
        rec.pane_resolution = resolution;
    }
    if rec.tmux_target.is_none() && inherited_target.is_some() {
        rec.tmux_target = inherited_target;
    }
    if rec.tmux_label.is_none() && inherited_label.is_some() {
        rec.tmux_label = inherited_label;
    }
    if rec.model.is_none() && inherited_model.is_some() {
        rec.model = inherited_model;
    }
    if rec.effort.is_none() && inherited_effort.is_some() {
        rec.effort = inherited_effort;
    }
    if rec.transcript_path.is_none() && inherited_transcript.is_some() {
        rec.transcript_path = inherited_transcript;
    }
    if rec.metrics.is_none() && inherited_metrics.is_some() {
        rec.metrics = inherited_metrics;
    }
    if rec.git_root.is_none() {
        rec.git_root = git_root;
    }
    // A spawned session gets its stage from the bind purpose.
    if let Some(BindPurpose::Stage { pipeline_id, stage }) = &applied.bound_now {
        rec.stage = Some(StageRef {
            pipeline_id: pipeline_id.clone(),
            stage: *stage,
        });
    }
    let next = rec.state.next(ev.kind);
    touch(rec, ev, next);
    let snapshot = rec.clone();
    if applied.bound_now.is_some()
        || matches!(ev.kind, EventKind::TurnComplete | EventKind::SessionEnd)
    {
        prune_unconfirmed(st, &root_id, snapshot.pane.as_ref());
    }
    st.recount_unread(&root_id);
    let snapshot = st.sessions.get(&root_id).cloned().unwrap_or(snapshot);
    applied.stage = snapshot.stage.clone();
    applied.updated.push(snapshot.clone());
    applied.route = Some((snapshot, false));
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_agy_idle_event(session_id: &str, ts_ms: u64) -> NormalizedEvent {
        NormalizedEvent {
            v: PROTO_VERSION,
            id: EventId::new(format!("E-{ts_ms}")),
            ts_ms,
            agent: AgentKind::Agy,
            kind: EventKind::TurnComplete,
            confidence: Confidence::Exact,
            session_id: SessionId::new(session_id),
            subagent_id: None,
            pane: None,
            cwd: Some("/Users/demo".into()),
            model: Some("gemini-4-hydrogen-high".into()),
            native_event: "Stop".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail::default(),
        }
    }

    #[test]
    fn reviving_detached_session_via_non_tmux_hook_clears_stale_tmux_metadata() {
        let mut st = State::default();
        let pane = PaneId::new("%42");

        // Rival previously held CwdMatch on %42 and was evicted.
        st.sessions.insert(
            SessionId::new("767462d7-ghost"),
            SessionRecord {
                session_id: SessionId::new("767462d7-ghost"),
                agent: AgentKind::Agy,
                pane: Some(pane.clone()),
                pane_resolution: PaneResolution::CwdMatch,
                cwd: Some("/Users/demo".into()),
                git_root: None,
                model: Some("gemini-4-hydrogen-high".into()),
                effort: Some("high".into()),
                state: SessionState::Idle,
                state_since_ms: 100,
                last_event_id: None,
                last_event_ms: 100,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Exact,
                tmux_target: Some("learning:7.0".into()),
                tmux_label: Some("learning-cloud-server".into()),
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

        // Exact-pane owner emits a hook on %42 -> evicts rival completely (clears tmux metadata).
        let owner_ev = mk_agy_idle_event("81166b09-owner", 200);
        apply_event(
            &mut st,
            &owner_ev,
            None,
            Some(pane.clone()),
            PaneResolution::Env,
        );
        let evicted = &st.sessions[&SessionId::new("767462d7-ghost")];
        assert_eq!(evicted.state, SessionState::Ended);
        assert!(evicted.pane.is_none());
        assert_eq!(evicted.pane_resolution, PaneResolution::Unknown);
        assert!(evicted.tmux_target.is_none());
        assert!(evicted.tmux_label.is_none());

        // Later, non-tmux ghost emits a new hook (`pane: None`) -> revived as non-tmux only.
        let ghost_ev = mk_agy_idle_event("767462d7-ghost", 300);
        apply_event(&mut st, &ghost_ev, None, None, PaneResolution::Unknown);
        let revived = &st.sessions[&SessionId::new("767462d7-ghost")];
        assert_eq!(revived.state, SessionState::Idle);
        assert!(revived.pane.is_none());
        assert_eq!(revived.pane_resolution, PaneResolution::Unknown);
        assert!(revived.tmux_target.is_none());
        assert!(revived.tmux_label.is_none());
    }

    #[test]
    fn exact_resolution_on_same_pane_upgrades_cwd_match_and_protects_discovered_placeholder() {
        let mut st = State::default();
        let pane = PaneId::new("%55");

        // 1. Session initially attaches via CwdMatch on %55.
        let ev1 = mk_agy_idle_event("sess-upgrade", 100);
        apply_event(
            &mut st,
            &ev1,
            None,
            Some(pane.clone()),
            PaneResolution::CwdMatch,
        );
        assert_eq!(
            st.sessions[&SessionId::new("sess-upgrade")].pane_resolution,
            PaneResolution::CwdMatch
        );

        // 2. Subsequent hook reports exact `Env` (or `PidTree`) on the SAME pane %55 -> MUST upgrade!
        let ev2 = mk_agy_idle_event("sess-upgrade", 200);
        apply_event(&mut st, &ev2, None, Some(pane.clone()), PaneResolution::Env);
        let upgraded = &st.sessions[&SessionId::new("sess-upgrade")];
        assert_eq!(upgraded.pane.as_ref(), Some(&pane));
        assert_eq!(upgraded.pane_resolution, PaneResolution::Env);

        // 3. A `discovered_only` placeholder on %66 with another session's transcript must NOT be stolen by CwdMatch.
        let p66 = PaneId::new("%66");
        let leg66 = SessionId::new("tmux-%66");
        let mut ph = new_record(
            leg66.clone(),
            &mk_agy_idle_event("tmux-%66", 300),
            Some(p66.clone()),
            PaneResolution::Env,
            None,
        );
        ph.discovered_only = true;
        ph.transcript_path =
            Some("/home/.gemini/antigravity-cli/brain/other-conv/.system_generated/logs/transcript.jsonl".into());
        st.sessions.insert(leg66.clone(), ph);

        let rival_ev = mk_agy_idle_event("unrelated-conv", 400);
        apply_event(
            &mut st,
            &rival_ev,
            None,
            Some(p66),
            PaneResolution::CwdMatch,
        );
        assert!(
            st.sessions.contains_key(&leg66),
            "CwdMatch must not hijack discovered_only placeholder with a different transcript"
        );
    }
}
