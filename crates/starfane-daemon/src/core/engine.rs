//! Playbook engine: pipelines, stage lifecycle, inbox, handoffs and
//! plan / triage / rca / report artifacts. Reviews live in `review.rs`.

use super::delivery::SpawnPlan;
use super::state::{BindPurpose, StoredArtifact, WorktreeInfo};
use super::tasks::{self, StageCtx};
use super::{lock, to_json, util, Core};
use crate::config::TeamConfig;
use crate::notice::{ActionStyle, Notice, NoticeAction, NoticeKind};
use starfane_proto::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn action(op: &str, label: &str, style: ActionStyle) -> NoticeAction {
    NoticeAction {
        op: op.into(),
        label: label.into(),
        style,
        finding_id: None,
    }
}

fn is_write_stage(stage: StageKind) -> bool {
    matches!(stage, StageKind::Implement | StageKind::Deep)
}

pub(crate) fn g2_config() -> starfane_evidence::G2Config {
    starfane_evidence::G2Config::default()
}

impl Core {
    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    pub(crate) fn list_sessions(
        &self,
        project: Option<&str>,
        subs: bool,
        include_ended: bool,
    ) -> Vec<SessionRecord> {
        let root = project.and_then(|p| self.find_project(p).ok());
        lock(&self.inner.state)
            .sessions
            .values()
            .filter(|s| subs || !s.is_subagent)
            .filter(|s| include_ended || s.state != SessionState::Ended)
            .filter(|s| match &root {
                Some(r) => s.git_root.as_deref() == Some(r.as_str()),
                None => true,
            })
            .cloned()
            .collect()
    }

    pub(crate) fn ack_session(&self, sid: &SessionId) -> Result<SessionRecord, ApiError> {
        let updated = {
            let mut st = lock(&self.inner.state);
            let pane = {
                let s = st
                    .sessions
                    .get_mut(sid)
                    .ok_or_else(|| ApiError::not_found(format!("session {sid}")))?;
                s.last_error = None;
                if matches!(
                    s.state,
                    SessionState::Error | SessionState::NeedsPermission | SessionState::NeedsInput
                ) {
                    s.state = SessionState::Idle;
                    s.state_since_ms = id::now_ms();
                }
                s.pane.clone()
            };
            st.unconfirmed.retain(|a| {
                a.session_id.as_ref() != Some(sid)
                    && pane
                        .as_ref()
                        .is_none_or(|p| a.session_id != Some(super::discover::legacy_session_id(p)))
            });
            st.sessions
                .get(sid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("session {sid}")))?
        };
        self.mark_dirty();
        self.publish_session(updated.clone());
        self.mirror_tmux_status(&updated);
        self.refresh_attention();
        Ok(updated)
    }

    pub(crate) fn list_pipelines(
        &self,
        project: Option<&str>,
        include_done: bool,
    ) -> Vec<PipelineSpec> {
        let root = project.and_then(|p| self.find_project(p).ok());
        lock(&self.inner.state)
            .pipelines
            .values()
            .filter(|p| {
                include_done
                    || !matches!(
                        p.status,
                        PipelineStatus::Done | PipelineStatus::Cancelled | PipelineStatus::Failed
                    )
            })
            .filter(|p| root.as_ref().is_none_or(|r| &p.project.root == r))
            .cloned()
            .collect()
    }

    pub(crate) fn list_handoffs(
        &self,
        session: Option<&SessionId>,
        pipeline: Option<&PipelineId>,
    ) -> Vec<HandoffPacket> {
        lock(&self.inner.state)
            .handoffs
            .values()
            .filter(|h| {
                session
                    .is_none_or(|s| &h.from.session_id == s || h.to.session_id.as_ref() == Some(s))
            })
            .filter(|h| pipeline.is_none_or(|p| h.pipeline_id.as_ref() == Some(p)))
            .cloned()
            .collect()
    }

    pub(crate) fn resolve_identity(
        &self,
        agent: AgentKind,
        cwd: Option<String>,
        pane: Option<PaneId>,
        bind_token: Option<BindToken>,
        hint: Option<SessionId>,
    ) -> IdentityResolution {
        let st = lock(&self.inner.state);
        if let Some(tok) = &bind_token {
            if let Some(sid) = st.binds.get(tok).and_then(|b| b.session_id.clone()) {
                return IdentityResolution::Resolved {
                    session_id: sid,
                    via: PaneResolution::BindToken,
                };
            }
        }
        if let Some(h) = hint.filter(|h| st.sessions.contains_key(h)) {
            return IdentityResolution::Resolved {
                session_id: h,
                via: PaneResolution::Env,
            };
        }
        if let Some(p) = &pane {
            if let Some(s) = st.session_for_pane(p).filter(|s| s.agent == agent) {
                return IdentityResolution::Resolved {
                    session_id: s.session_id.clone(),
                    via: PaneResolution::Env,
                };
            }
        }
        let Some(cwd) = cwd else {
            return IdentityResolution::Unknown;
        };
        let cwd_root = st.git_roots.get(&cwd).cloned().flatten();
        let candidates: Vec<SessionRecord> = st
            .sessions
            .values()
            .filter(|s| !s.is_subagent && s.agent == agent && s.state != SessionState::Ended)
            .filter(|s| {
                s.cwd.as_deref() == Some(cwd.as_str())
                    || (cwd_root.is_some() && s.git_root == cwd_root)
                    || s.git_root
                        .as_deref()
                        .is_some_and(|r| util::path_starts_with(&cwd, r))
            })
            .cloned()
            .collect();
        match candidates.len() {
            0 => IdentityResolution::Unknown,
            1 => IdentityResolution::Resolved {
                session_id: candidates[0].session_id.clone(),
                via: PaneResolution::CwdMatch,
            },
            _ => IdentityResolution::Ambiguous { candidates },
        }
    }

    pub(crate) fn read_inbox(
        &self,
        sid: &SessionId,
        mark: bool,
    ) -> Result<Vec<InboxItem>, ApiError> {
        let mut st = lock(&self.inner.state);
        if !st.sessions.contains_key(sid) {
            return Err(ApiError::not_found(format!("unknown session {sid}")));
        }
        let items: Vec<InboxItem> = match st.inbox.get_mut(sid) {
            Some(entries) => {
                let out: Vec<InboxItem> = entries
                    .iter_mut()
                    .filter(|e| !e.delivered)
                    .map(|e| {
                        if mark {
                            e.delivered = true;
                            e.poke_pending = false;
                        }
                        e.item.clone()
                    })
                    .collect();
                if mark {
                    entries.retain(|e| !(e.delivered && e.item.kind == InboxKind::Message));
                }
                out
            }
            None => Vec::new(),
        };
        if st.inbox.get(sid).is_some_and(Vec::is_empty) {
            st.inbox.remove(sid);
        }
        st.recount_unread(sid);
        let snapshot = st.sessions.get(sid).cloned();
        drop(st);
        self.mark_dirty();
        if let Some(s) = snapshot {
            self.publish_session(s);
        }
        Ok(items)
    }

    pub(crate) fn report_progress(
        &self,
        sid: &SessionId,
        text: &str,
        notify_human: bool,
    ) -> Result<serde_json::Value, ApiError> {
        let session = lock(&self.inner.state)
            .sessions
            .get(sid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown session {sid}")))?;
        if text.trim().is_empty() {
            return Err(ApiError::bad_request("text is empty"));
        }
        self.audit(
            "progress",
            serde_json::json!({"session": sid, "text": text}),
        );
        if notify_human {
            self.notify(Notice {
                key: format!(
                    "prog-{}-{}",
                    util::short_id(sid.as_str()),
                    id::now_ms() / 1000
                ),
                kind: NoticeKind::Progress,
                title: format!(
                    "📣 {}@{} 进展",
                    session.agent,
                    session.pane.as_ref().map(|p| p.as_str()).unwrap_or("?")
                ),
                body_md: text.to_string(),
                pipeline_id: session.stage.as_ref().map(|s| s.pipeline_id.clone()),
                session_id: Some(sid.clone()),
                review_id: None,
                actions: Vec::new(),
                binding_form: Vec::new(),
                reply_session: None,
            });
        }
        Ok(serde_json::json!({}))
    }

    // ------------------------------------------------------------------
    // Utterance parsing & pipeline creation
    // ------------------------------------------------------------------

    fn team_for(&self, project_root: Option<&str>) -> TeamConfig {
        match project_root {
            Some(r) => TeamConfig::load(Path::new(r)).unwrap_or_else(|e| {
                log::warn!("team.toml: {e}; using defaults");
                TeamConfig::builtin()
            }),
            None => TeamConfig::builtin(),
        }
    }

    pub(crate) fn parse_utterance(
        &self,
        text: &str,
        playbook: Option<PlaybookKind>,
        project: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        if text.trim().is_empty() {
            return Err(ApiError::bad_request("text is empty"));
        }
        let root = match project {
            Some(p) => Some(self.find_project(p)?),
            None => None,
        };
        let team = self.team_for(root.as_deref());
        let ctx = starfane_binding::BindingContext {
            aliases: team.aliases.clone(),
            known_models: BTreeMap::new(),
        };
        let defaults = starfane_binding::PlaybookDefaults {
            feature: team.feature.clone(),
            debug: team.debug.clone(),
        };
        let mut parsed = starfane_binding::parse_binding_utterance(text, playbook, &ctx);
        starfane_binding::apply_defaults(&mut parsed, &defaults);
        let mut seen: Vec<String> = parsed
            .warnings
            .iter()
            .filter_map(|w| serde_json::to_string(w).ok())
            .collect();
        for w in starfane_binding::validate(&parsed, &ctx) {
            let key = serde_json::to_string(&w).unwrap_or_default();
            if !seen.contains(&key) {
                seen.push(key);
                parsed.warnings.push(w);
            }
        }
        to_json(parsed)
    }

    pub(crate) async fn create_pipeline(
        &self,
        req: CreatePipelineReq,
    ) -> Result<serde_json::Value, ApiError> {
        let root = req.project_root.trim().to_string();
        if root.is_empty() || !Path::new(&root).is_dir() {
            return Err(ApiError::bad_request(format!(
                "project_root {root:?} is not a directory"
            )));
        }
        let root = {
            let r = root.clone();
            tokio::task::spawn_blocking(move || util::git_root(Path::new(&r)).unwrap_or(r))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
        };
        if req.goal.trim().is_empty() {
            return Err(ApiError::bad_request("goal is empty"));
        }
        let team = self.team_for(Some(&root));
        let options = req.options.clone().unwrap_or_else(|| team.options.clone());
        let mut bindings = team.defaults_for(req.playbook).clone();
        for (stage, b) in &req.bindings {
            if stage.playbook() != req.playbook {
                return Err(ApiError::bad_request(format!(
                    "stage {stage} is not part of the {} playbook",
                    req.playbook.as_str()
                )));
            }
            bindings.insert(*stage, b.clone());
        }
        let stages: Vec<StageSpec> = req
            .playbook
            .stages()
            .iter()
            .map(|s| {
                let mut b = bindings.get(s).cloned().unwrap_or_default();
                if *s == StageKind::TestReview && b.depth.is_none() {
                    b.depth = Some(options.review_depth);
                }
                StageSpec::template(*s, b, options.max_review_rounds)
            })
            .collect();
        let now = id::now_ms();
        let spec = PipelineSpec {
            v: PROTO_VERSION,
            id: PipelineId(id::new_id(id::prefix::PIPELINE)),
            playbook: req.playbook,
            goal: req.goal.trim().to_string(),
            status: PipelineStatus::AwaitingBindingConfirm,
            created_ms: now,
            updated_ms: now,
            project: ProjectRef {
                root: root.clone(),
                base_branch: None,
                worktree: WorktreeMode::Auto,
            },
            source: req.source.clone(),
            options,
            feishu_thread: None,
            stages,
        };
        lock(&self.inner.state)
            .pipelines
            .insert(spec.id.clone(), spec.clone());
        self.mark_dirty();
        self.publish_pipeline(spec.clone());
        if matches!(spec.source.via, Via::Feishu | Via::Mcp) {
            self.notify(binding_notice(&spec));
        }
        to_json(spec)
    }

    pub(crate) async fn confirm_pipeline(
        &self,
        pid: &PipelineId,
        overrides: Option<BTreeMap<StageKind, StageBinding>>,
    ) -> Result<serde_json::Value, ApiError> {
        let first = {
            let mut st = lock(&self.inner.state);
            let p = st
                .pipelines
                .get_mut(pid)
                .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))?;
            if p.status != PipelineStatus::AwaitingBindingConfirm {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    format!(
                        "pipeline {pid} is {:?}, not awaiting confirmation",
                        p.status
                    ),
                ));
            }
            if let Some(ov) = overrides {
                for (stage, b) in ov {
                    match p.stage_mut(stage) {
                        Some(s) => {
                            let depth = s.binding.depth;
                            s.binding = b;
                            if s.binding.depth.is_none() {
                                s.binding.depth = depth;
                            }
                        }
                        None => {
                            return Err(ApiError::bad_request(format!(
                                "stage {stage} is not part of this pipeline"
                            )))
                        }
                    }
                }
            }
            p.status = PipelineStatus::Running;
            p.updated_ms = id::now_ms();
            p.playbook.stages()[0]
        };
        self.mark_dirty();
        self.start_stage(pid, first).await?;
        self.pipeline_json(pid)
    }

    fn pipeline_json(&self, pid: &PipelineId) -> Result<serde_json::Value, ApiError> {
        lock(&self.inner.state)
            .pipelines
            .get(pid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))
            .and_then(to_json)
    }

    /// Mutate a pipeline under the lock, bump `updated_ms`, publish it.
    pub(crate) fn update_pipeline<R>(
        &self,
        pid: &PipelineId,
        f: impl FnOnce(&mut PipelineSpec) -> R,
    ) -> Result<R, ApiError> {
        let (r, snapshot) = {
            let mut st = lock(&self.inner.state);
            let p = st
                .pipelines
                .get_mut(pid)
                .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))?;
            let r = f(p);
            p.updated_ms = id::now_ms();
            (r, p.clone())
        };
        self.mark_dirty();
        self.publish_pipeline(snapshot);
        Ok(r)
    }

    // ------------------------------------------------------------------
    // Stage lifecycle
    // ------------------------------------------------------------------

    /// Working directory for a stage (creates the worktree for write stages).
    async fn stage_cwd(&self, p: &PipelineSpec, stage: StageKind) -> String {
        let root = p.project.root.clone();
        let existing = lock(&self.inner.state).worktrees.get(&p.id).cloned();
        match &p.project.worktree {
            WorktreeMode::None => root,
            WorktreeMode::Path(path) => path.clone(),
            WorktreeMode::Auto => {
                if let Some(w) = existing {
                    return w.path;
                }
                if !is_write_stage(stage) {
                    return root;
                }
                let pid = p.id.0.clone();
                let r = root.clone();
                match tokio::task::spawn_blocking(move || util::worktree_add(Path::new(&r), &pid))
                    .await
                {
                    Ok(Ok((path, branch, base))) => {
                        lock(&self.inner.state).worktrees.insert(
                            p.id.clone(),
                            WorktreeInfo {
                                path: path.clone(),
                                branch,
                                base,
                            },
                        );
                        self.mark_dirty();
                        path
                    }
                    Ok(Err(e)) => {
                        log::warn!("worktree for {}: {e}; using project root", p.id);
                        root
                    }
                    Err(e) => {
                        log::error!("worktree task: {e}");
                        root
                    }
                }
            }
        }
    }

    pub(crate) fn worktree_base(&self, pid: &PipelineId) -> Option<String> {
        lock(&self.inner.state)
            .worktrees
            .get(pid)
            .map(|w| w.base.clone())
    }

    /// Bind a stage to a session (adopt or L4 spawn) and deliver its task.
    pub(crate) async fn start_stage(
        &self,
        pid: &PipelineId,
        stage: StageKind,
    ) -> Result<(), ApiError> {
        let p = lock(&self.inner.state)
            .pipelines
            .get(pid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))?;
        let spec = p
            .stage(stage)
            .cloned()
            .ok_or_else(|| ApiError::bad_request(format!("stage {stage} not in pipeline")))?;

        // Resolve `same_as` chain (up to 6 hops) into a concrete binding / session.
        let mut binding = spec.binding.clone();
        for _ in 0..6 {
            let Some(src) = binding.same_as else {
                break;
            };
            let Some(src_stage) = p.stage(src).cloned() else {
                binding.same_as = None;
                break;
            };
            if src_stage.binding.same_as.is_some() {
                binding = src_stage.binding;
                continue;
            }
            let alive = src_stage.session_id.filter(|sid| {
                lock(&self.inner.state)
                    .sessions
                    .get(sid)
                    .is_some_and(|s| s.state != SessionState::Ended)
            });
            let mut b = src_stage.binding;
            b.same_as = None;
            b.session = alive.map(SessionTarget::Session).unwrap_or_default();
            binding = b;
            break;
        }
        binding.same_as = None;
        let agent = binding
            .agent
            .ok_or_else(|| ApiError::bad_request(format!("stage {stage} has no agent binding")))?;
        let cwd = self.stage_cwd(&p, stage).await;
        let (title, body) = self.render_task(&p, stage, &cwd);
        let item = InboxItem {
            id: id::new_id("T"),
            kind: InboxKind::Task,
            from: None,
            created_ms: id::now_ms(),
            title,
            body_md: body,
            stage: Some(StageRef {
                pipeline_id: pid.clone(),
                stage,
            }),
            artifact: None,
            handoff_id: None,
            review_id: None,
        };
        let mut pointer = tasks::pointer(pid, stage);
        if agent == AgentKind::Agy {
            if let Some(slash) = binding.native.and_then(|n| n.slash()) {
                pointer = format!("{slash} {pointer}");
            }
        }

        let adopt = self.pick_session(&p, stage, &binding, agent, &cwd)?;
        match adopt {
            Some(session_id) => {
                let pane = {
                    let mut st = lock(&self.inner.state);
                    if let Some(s) = st.sessions.get_mut(&session_id) {
                        s.stage = Some(StageRef {
                            pipeline_id: pid.clone(),
                            stage,
                        });
                    }
                    st.push_inbox(&session_id, item);
                    st.sessions.get(&session_id).and_then(|s| s.pane.clone())
                };
                self.update_pipeline(pid, |p| {
                    if let Some(s) = p.stage_mut(stage) {
                        s.status = StageStatus::Running;
                        s.session_id = Some(session_id.clone());
                        s.pane = pane.clone();
                        s.started_ms.get_or_insert(id::now_ms());
                    }
                })?;
                if let Some(s) = lock(&self.inner.state).sessions.get(&session_id).cloned() {
                    self.publish_session(s);
                }
                let allow = vec![DeliveryLevel::L1CodexQueue, DeliveryLevel::L2Inject];
                let core = self.clone();
                tokio::spawn(async move {
                    let r = core.deliver_line(&session_id, &pointer, &allow).await;
                    log::info!(
                        "stage task delivery to {session_id}: {:?} via {:?}",
                        r.status,
                        r.level
                    );
                });
            }
            None => {
                let plan = SpawnPlan {
                    purpose: BindPurpose::Stage {
                        pipeline_id: pid.clone(),
                        stage,
                    },
                    agent,
                    model: binding.model.clone(),
                    effort: binding.effort,
                    cwd: cwd.clone(),
                    name: format!("{}-{}", util::short_id(pid.as_str()), stage.as_str()),
                    prompt: pointer,
                    project_root: p.project.root.clone(),
                };
                match self.spawn_session(plan).await {
                    Ok((token, pane)) => {
                        {
                            let mut st = lock(&self.inner.state);
                            let key = st
                                .binds
                                .get(&token)
                                .and_then(|b| b.session_id.clone())
                                .unwrap_or_else(|| Core::inbox_key_for_bind(&token));
                            st.push_inbox(&key, item);
                        }
                        self.update_pipeline(pid, |p| {
                            if let Some(s) = p.stage_mut(stage) {
                                s.status = StageStatus::Running;
                                s.pane = Some(pane.clone());
                                s.started_ms.get_or_insert(id::now_ms());
                            }
                        })?;
                    }
                    Err(e) => {
                        self.update_pipeline(pid, |p| {
                            p.status = PipelineStatus::Blocked;
                            if let Some(s) = p.stage_mut(stage) {
                                s.status = StageStatus::Blocked;
                            }
                        })?;
                        self.notify(Notice {
                            key: format!(
                                "spawnfail-{}-{}",
                                util::short_id(pid.as_str()),
                                stage.as_str()
                            ),
                            kind: NoticeKind::Error,
                            title: format!("❌ 无法为 {stage} 拉起 {agent} 会话"),
                            body_md: e.message.clone(),
                            pipeline_id: Some(pid.clone()),
                            session_id: None,
                            review_id: None,
                            actions: Vec::new(),
                            binding_form: Vec::new(),
                            reply_session: None,
                        });
                        return Err(e);
                    }
                }
            }
        }
        if stage == StageKind::Triage {
            self.start_triage_timer(pid.clone(), p.options.triage_timebox_secs);
        }
        Ok(())
    }

    fn render_task(&self, p: &PipelineSpec, stage: StageKind, cwd: &str) -> (String, String) {
        let st = lock(&self.inner.state);
        let artifact_of = |kind: StageKind| -> Option<&ArtifactPayload> {
            p.stage(kind)
                .and_then(|s| s.artifacts.last())
                .and_then(|a| st.artifacts.get(a))
                .map(|a| &a.payload)
        };
        let plan = match artifact_of(StageKind::Plan) {
            Some(ArtifactPayload::Plan(d)) => Some(d.clone()),
            _ => None,
        };
        let triage = match artifact_of(StageKind::Triage) {
            Some(ArtifactPayload::Triage(t)) => Some(t.clone()),
            _ => None,
        };
        let handoff = st
            .handoffs
            .values()
            .filter(|h| h.pipeline_id.as_ref() == Some(&p.id) && h.intent == HandoffIntent::Review)
            .max_by_key(|h| h.created_ms)
            .cloned();
        let base = st.worktrees.get(&p.id).map(|w| w.base.clone());
        // Confirmed findings from the latest completed review (fix loop).
        let findings: Option<Vec<AdjudicatedFinding>> = if stage == StageKind::Implement {
            st.reviews
                .values()
                .filter(|r| r.pipeline_id.as_ref() == Some(&p.id) && r.record.complete)
                .max_by_key(|r| r.record.report.header.created_ms)
                .map(|r| {
                    r.record
                        .findings
                        .iter()
                        .filter(|f| f.disposition == Disposition::Delivered)
                        .cloned()
                        .collect()
                })
                .filter(|v: &Vec<AdjudicatedFinding>| !v.is_empty())
        } else {
            None
        };
        drop(st);
        tasks::render_stage_task(&StageCtx {
            pipeline: p,
            stage,
            cwd,
            plan: plan.as_ref(),
            handoff: handoff.as_ref(),
            triage: triage.as_ref(),
            base: base.as_deref(),
            review_findings: findings.as_deref(),
        })
    }

    /// Choose an existing idle session to adopt, or None → spawn.
    fn pick_session(
        &self,
        p: &PipelineSpec,
        stage: StageKind,
        binding: &StageBinding,
        agent: AgentKind,
        cwd: &str,
    ) -> Result<Option<SessionId>, ApiError> {
        let st = lock(&self.inner.state);
        match &binding.session {
            SessionTarget::Spawn => Ok(None),
            SessionTarget::Session(id) => match st.sessions.get(id) {
                Some(s) if s.state != SessionState::Ended => Ok(Some(id.clone())),
                _ => Err(ApiError::not_found(format!(
                    "bound session {id} is not alive"
                ))),
            },
            SessionTarget::Pane(pane) => st
                .session_for_pane(pane)
                .map(|s| Some(s.session_id.clone()))
                .ok_or_else(|| ApiError::not_found(format!("no live session in pane {pane}"))),
            SessionTarget::Auto => {
                let busy: Vec<SessionId> = st
                    .pipelines
                    .values()
                    .filter(|q| {
                        matches!(
                            q.status,
                            PipelineStatus::Running
                                | PipelineStatus::Blocked
                                | PipelineStatus::AwaitingHuman
                        )
                    })
                    .flat_map(|q| {
                        q.stages
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    StageStatus::Running
                                        | StageStatus::Blocked
                                        | StageStatus::AwaitingHuman
                                )
                            })
                            .filter_map(|s| s.session_id.clone())
                    })
                    .collect();
                let want_model = binding.model.as_ref().map(|m| m.to_lowercase());
                let found = st
                    .sessions
                    .values()
                    .filter(|s| !s.is_subagent && s.agent == agent && s.pane.is_some())
                    .filter(|s| matches!(s.state, SessionState::Idle))
                    .filter(|s| !busy.contains(&s.session_id))
                    .filter(|s| {
                        if is_write_stage(stage) && cwd != p.project.root {
                            s.cwd
                                .as_deref()
                                .is_some_and(|c| util::path_starts_with(c, cwd))
                        } else {
                            s.git_root.as_deref() == Some(p.project.root.as_str())
                                || s.cwd
                                    .as_deref()
                                    .is_some_and(|c| util::path_starts_with(c, &p.project.root))
                        }
                    })
                    .filter(|s| match (&want_model, &s.model) {
                        (None, _) => true,
                        (Some(w), Some(m)) => m.to_lowercase().contains(w.as_str()),
                        (Some(_), None) => false,
                    })
                    .max_by_key(|s| s.last_event_ms)
                    .map(|s| s.session_id.clone());
                Ok(found)
            }
        }
    }

    fn start_triage_timer(&self, pid: PipelineId, secs: u64) {
        if secs == 0 {
            return;
        }
        let core = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            let session = {
                let st = lock(&core.inner.state);
                st.pipelines
                    .get(&pid)
                    .and_then(|p| p.stage(StageKind::Triage))
                    .filter(|s| s.status == StageStatus::Running)
                    .and_then(|s| s.session_id.clone())
            };
            if let Some(sid) = session {
                let line = "starfane: 快筛软时限已到，请立即调用 submit_triage 提交当前最佳假设（可以不确定，但要有证据）";
                let r = core
                    .deliver_line(&sid, line, &[DeliveryLevel::L2Inject])
                    .await;
                log::info!("triage timebox poke {sid}: {:?}", r.status);
            }
        });
    }

    /// Called when a spawned session's first hook arrives.
    pub(crate) async fn on_spawn_bound(&self, purpose: BindPurpose, session_id: &SessionId) {
        match purpose {
            BindPurpose::Stage { pipeline_id, stage } => {
                let r = self.update_pipeline(&pipeline_id, |p| {
                    if let Some(s) = p.stage_mut(stage) {
                        s.session_id = Some(session_id.clone());
                    }
                });
                if let Err(e) = r {
                    log::warn!("bind {session_id} to {pipeline_id}/{stage}: {}", e.message);
                }
            }
            BindPurpose::Falsifier { review_id } => {
                if let Some(r) = lock(&self.inner.state).reviews.get_mut(&review_id) {
                    r.falsifier_session = Some(session_id.clone());
                }
                self.mark_dirty();
            }
        }
    }

    /// Stage session became blocked / resumed.
    pub(crate) async fn on_stage_session_event(
        &self,
        stage: &StageRef,
        _session: &SessionRecord,
        kind: EventKind,
    ) {
        let r = self.update_pipeline(&stage.pipeline_id, |p| {
            let Some(s) = p.stage_mut(stage.stage) else {
                return;
            };
            match kind {
                EventKind::NeedsPermission | EventKind::NeedsInput | EventKind::Error
                    if s.status == StageStatus::Running =>
                {
                    s.status = StageStatus::Blocked;
                    p.status = PipelineStatus::Blocked;
                }
                EventKind::Working
                | EventKind::ToolStart
                | EventKind::ToolEnd
                | EventKind::TurnComplete
                | EventKind::SubagentEnd
                    if s.status == StageStatus::Blocked =>
                {
                    s.status = StageStatus::Running;
                    if !p.stages.iter().any(|x| x.status == StageStatus::Blocked) {
                        p.status = PipelineStatus::Running;
                    }
                }
                _ => {}
            }
        });
        if let Err(e) = r {
            log::debug!("stage event for missing pipeline: {}", e.message);
        }
    }

    /// Mark `stage` Done and start the next pending stage (or finish).
    pub(crate) async fn advance(&self, pid: &PipelineId, done: StageKind) -> Result<(), ApiError> {
        let next = self.update_pipeline(pid, |p| {
            if let Some(s) = p.stage_mut(done) {
                s.status = StageStatus::Done;
                s.ended_ms = Some(id::now_ms());
            }
            let next = p
                .stages
                .iter()
                .find(|s| s.status == StageStatus::Pending)
                .map(|s| s.id);
            match next {
                Some(_) => p.status = PipelineStatus::Running,
                None => p.status = PipelineStatus::Done,
            }
            next
        })?;
        self.release_stage_sessions_if_done(pid);
        match next {
            Some(stage) => self.start_stage(pid, stage).await,
            None => Ok(()),
        }
    }

    fn release_stage_sessions_if_done(&self, pid: &PipelineId) {
        let mut st = lock(&self.inner.state);
        let done = st.pipelines.get(pid).is_some_and(|p| {
            matches!(
                p.status,
                PipelineStatus::Done | PipelineStatus::Cancelled | PipelineStatus::Failed
            )
        });
        if !done {
            return;
        }
        for s in st.sessions.values_mut() {
            if s.stage.as_ref().is_some_and(|r| &r.pipeline_id == pid) {
                s.stage = None;
            }
        }
        drop(st);
        self.mark_dirty();
    }

    pub(crate) async fn pipeline_action(
        &self,
        pid: &PipelineId,
        action: PipelineAction,
    ) -> Result<serde_json::Value, ApiError> {
        let p = lock(&self.inner.state)
            .pipelines
            .get(pid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))?;
        let conflict = |m: String| ApiError::new(ErrorCode::Conflict, m);
        match action {
            PipelineAction::ApprovePlan => {
                let plan = p
                    .stage(StageKind::Plan)
                    .ok_or_else(|| conflict("no plan stage".into()))?;
                if plan.status != StageStatus::AwaitingHuman {
                    return Err(conflict(format!("plan stage is {:?}", plan.status)));
                }
                self.advance(pid, StageKind::Plan).await?;
            }
            PipelineAction::RequestChanges { note } => {
                let note = note.trim().to_string();
                if note.is_empty() {
                    return Err(ApiError::bad_request("note is empty"));
                }
                let plan = p
                    .stage(StageKind::Plan)
                    .ok_or_else(|| conflict("no plan stage".into()))?;
                if plan.status != StageStatus::AwaitingHuman {
                    return Err(conflict(format!("plan stage is {:?}", plan.status)));
                }
                let sid = plan
                    .session_id
                    .clone()
                    .ok_or_else(|| conflict("plan stage has no session".into()))?;
                self.update_pipeline(pid, |p| {
                    p.status = PipelineStatus::Running;
                    if let Some(s) = p.stage_mut(StageKind::Plan) {
                        s.status = StageStatus::Running;
                        s.round += 1;
                    }
                })?;
                self.dispatch(
                    DispatchTarget::Session { id: sid },
                    format!(
                        "计划需要修改：{note}。请修正后重新调用 report_milestone(kind=plan) 提交。"
                    ),
                    vec![],
                    vec![],
                    false,
                )
                .await?;
            }
            PipelineAction::Cancel => {
                self.update_pipeline(pid, |p| {
                    p.status = PipelineStatus::Cancelled;
                    for s in p.stages.iter_mut() {
                        if s.status != StageStatus::Done {
                            s.status = StageStatus::Skipped;
                        }
                    }
                })?;
                self.release_stage_sessions_if_done(pid);
            }
            PipelineAction::ContinueDeep => {
                let deep = p
                    .stage(StageKind::Deep)
                    .ok_or_else(|| conflict("not a debug pipeline".into()))?;
                if !matches!(
                    deep.status,
                    StageStatus::Pending | StageStatus::AwaitingHuman
                ) {
                    return Err(conflict(format!("deep stage is {:?}", deep.status)));
                }
                self.update_pipeline(pid, |p| {
                    p.status = PipelineStatus::Running;
                    if let Some(s) = p.stage_mut(StageKind::Deep) {
                        s.status = StageStatus::Pending;
                    }
                })?;
                self.start_stage(pid, StageKind::Deep).await?;
            }
            PipelineAction::StopDeep => {
                p.stage(StageKind::Deep)
                    .ok_or_else(|| conflict("not a debug pipeline".into()))?;
                self.update_pipeline(pid, |p| {
                    if let Some(s) = p.stage_mut(StageKind::Deep) {
                        if s.status != StageStatus::Done {
                            s.status = StageStatus::Skipped;
                        }
                    }
                    p.status = PipelineStatus::Done;
                })?;
                self.release_stage_sessions_if_done(pid);
            }
            PipelineAction::AdoptMitigation => {
                self.audit(
                    "pipeline",
                    serde_json::json!({"pipeline": pid, "action": "adopt_mitigation"}),
                );
                if let Some(sid) = p
                    .stage(StageKind::Triage)
                    .and_then(|s| s.session_id.clone())
                {
                    let r = self
                        .dispatch(
                            DispatchTarget::Session { id: sid },
                            "用户决定采用快筛给出的止血方案，请执行并在完成后用 report_milestone(kind=progress) 汇报。".into(),
                            vec![],
                            vec![],
                            false,
                        )
                        .await;
                    if let Err(e) = r {
                        log::warn!("adopt mitigation dispatch: {}", e.message);
                    }
                }
            }
            PipelineAction::RetryStage { stage } => {
                p.stage(stage)
                    .ok_or_else(|| ApiError::bad_request(format!("no stage {stage}")))?;
                self.update_pipeline(pid, |p| {
                    p.status = PipelineStatus::Running;
                    if let Some(s) = p.stage_mut(stage) {
                        s.status = StageStatus::Pending;
                        s.session_id = None;
                        s.pane = None;
                    }
                })?;
                self.start_stage(pid, stage).await?;
            }
            PipelineAction::SkipStage { stage } => {
                p.stage(stage)
                    .ok_or_else(|| ApiError::bad_request(format!("no stage {stage}")))?;
                let was_current = p.current_stage().map(|s| s.id) == Some(stage);
                self.update_pipeline(pid, |p| {
                    if let Some(s) = p.stage_mut(stage) {
                        s.status = StageStatus::Skipped;
                    }
                })?;
                if was_current {
                    let next = lock(&self.inner.state).pipelines.get(pid).and_then(|p| {
                        p.stages
                            .iter()
                            .find(|s| s.status == StageStatus::Pending)
                            .map(|s| s.id)
                    });
                    match next {
                        Some(n) => self.start_stage(pid, n).await?,
                        None => {
                            self.update_pipeline(pid, |p| p.status = PipelineStatus::Done)?;
                            self.release_stage_sessions_if_done(pid);
                        }
                    }
                }
            }
        }
        self.pipeline_json(pid)
    }

    // ------------------------------------------------------------------
    // Handoffs
    // ------------------------------------------------------------------

    pub(crate) async fn create_handoff(
        &self,
        draft: HandoffDraft,
    ) -> Result<serde_json::Value, ApiError> {
        let from = lock(&self.inner.state)
            .sessions
            .get(&draft.from_session)
            .cloned()
            .ok_or_else(|| {
                ApiError::not_found(format!("unknown session {}", draft.from_session))
            })?;
        let cwd = from.cwd.clone().or_else(|| from.git_root.clone());
        let repo = match cwd {
            Some(dir) => tokio::task::spawn_blocking(move || {
                let d = PathBuf::from(&dir);
                util::git_root(&d).map(|root| RepoState {
                    root,
                    branch: util::git_branch(&d),
                    base: None,
                    head: util::git_head(&d),
                    dirty: util::git_dirty(&d),
                })
            })
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?,
            None => None,
        };
        let pipeline_stage = from.stage.clone();
        let mut packet = HandoffPacket {
            v: PROTO_VERSION,
            id: HandoffId(id::new_id(id::prefix::HANDOFF)),
            pipeline_id: pipeline_stage.as_ref().map(|s| s.pipeline_id.clone()),
            from: AuthorRef {
                agent: from.agent,
                session_id: from.session_id.clone(),
                model: from.model.clone(),
            },
            to: draft.to.clone(),
            intent: draft.intent,
            mode: draft.mode,
            repo,
            summary: draft.summary.clone(),
            body_md: draft.body_md.clone(),
            self_test: draft.self_test.clone(),
            todo: draft.todo.clone(),
            refs: draft.refs.clone(),
            status: HandoffStatus::Pending,
            created_ms: id::now_ms(),
        };
        if let (Some(r), Some(stage)) = (packet.repo.as_mut(), pipeline_stage.as_ref()) {
            r.base = self.worktree_base(&stage.pipeline_id);
        }
        let g1 = starfane_evidence::validate_g1_handoff(&packet);
        if !g1.ok() {
            return Err(ApiError::bad_request(format!(
                "handoff rejected by G1: {}",
                g1.report
                    .iter()
                    .map(|i| format!("{}: {}", i.path, i.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        // Pipeline flow: Builder → Reviewer.
        let builder_stage = pipeline_stage
            .clone()
            .filter(|s| s.stage == StageKind::Implement && draft.intent == HandoffIntent::Review);
        if let Some(stage) = builder_stage {
            packet.status = HandoffStatus::Delivered;
            self.store_handoff(&packet, &from);
            self.update_pipeline(&stage.pipeline_id, |p| {
                if let Some(s) = p.stage_mut(StageKind::TestReview) {
                    s.status = StageStatus::Pending;
                }
            })?;
            // The handoff itself succeeded; a failure to start the reviewer is
            // surfaced on the pipeline (Blocked + notice), not as a handoff error.
            if let Err(e) = self.advance(&stage.pipeline_id, StageKind::Implement).await {
                log::warn!(
                    "start review stage for {}: {}",
                    stage.pipeline_id,
                    e.message
                );
            }
            return to_json(CreateHandoffResult {
                handoff_id: packet.id.clone(),
                delivery: DeliveryReport {
                    level: DeliveryLevel::L0Inbox,
                    status: DeliveryStatus::Queued,
                    gate: None,
                    pane: None,
                    ack: None,
                },
            });
        }

        // Ad-hoc handoff.
        let target = self.resolve_handoff_target(&packet.to, &from)?;
        packet.to.session_id = Some(target.clone());
        self.store_handoff(&packet, &from);
        {
            let mut st = lock(&self.inner.state);
            st.push_inbox(
                &target,
                InboxItem {
                    id: packet.id.0.clone(),
                    kind: InboxKind::Handoff,
                    from: Some(packet.from.clone()),
                    created_ms: packet.created_ms,
                    title: util::sanitize_line(&packet.summary, 80),
                    body_md: render_handoff(&packet),
                    stage: None,
                    artifact: None,
                    handoff_id: Some(packet.id.clone()),
                    review_id: None,
                },
            );
        }
        let line = format!(
            "starfane: 你收到来自 {} 的交接 {}，请调用 starfane 的 read_inbox 工具获取",
            from.agent, packet.id
        );
        let report = self
            .deliver_line(
                &target,
                &line,
                &[DeliveryLevel::L1CodexQueue, DeliveryLevel::L2Inject],
            )
            .await;
        to_json(CreateHandoffResult {
            handoff_id: packet.id,
            delivery: report,
        })
    }

    fn resolve_handoff_target(
        &self,
        to: &HandoffTarget,
        from: &SessionRecord,
    ) -> Result<SessionId, ApiError> {
        let st = lock(&self.inner.state);
        if let Some(sid) = &to.session_id {
            return st
                .sessions
                .contains_key(sid)
                .then(|| sid.clone())
                .ok_or_else(|| ApiError::not_found(format!("session {sid}")));
        }
        if let Some(p) = &to.pane {
            return st
                .session_for_pane(p)
                .map(|s| s.session_id.clone())
                .ok_or_else(|| ApiError::not_found(format!("no live session in pane {p}")));
        }
        if let Some(stage) = &to.stage {
            return st
                .pipelines
                .get(&stage.pipeline_id)
                .and_then(|p| p.stage(stage.stage))
                .and_then(|s| s.session_id.clone())
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::Conflict,
                        format!("stage {} has no session", stage.stage),
                    )
                });
        }
        if let Some(agent) = to.agent {
            let candidates: Vec<&SessionRecord> = st
                .sessions
                .values()
                .filter(|s| !s.is_subagent && s.agent == agent && s.state != SessionState::Ended)
                .filter(|s| s.session_id != from.session_id)
                .filter(|s| from.git_root.is_none() || s.git_root == from.git_root)
                .collect();
            return match candidates.len() {
                1 => Ok(candidates[0].session_id.clone()),
                0 => Err(ApiError::not_found(format!(
                    "no live {agent} session in this project"
                ))),
                _ => Err(ApiError::new(
                    ErrorCode::Conflict,
                    format!(
                        "several {agent} sessions: {}; pass to.session_id or to.pane",
                        candidates
                            .iter()
                            .map(|s| format!(
                                "{} ({})",
                                s.session_id,
                                s.pane.as_ref().map(|p| p.as_str()).unwrap_or("?")
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            };
        }
        Err(ApiError::bad_request("handoff target is empty"))
    }

    fn store_handoff(&self, packet: &HandoffPacket, from: &SessionRecord) {
        lock(&self.inner.state)
            .handoffs
            .insert(packet.id.clone(), packet.clone());
        self.mark_dirty();
        if let Some(root) = packet
            .repo
            .as_ref()
            .map(|r| r.root.clone())
            .or_else(|| from.git_root.clone())
        {
            let path = paths::project_dir(Path::new(&root))
                .join("handoffs")
                .join(format!("{}.json", packet.id));
            match serde_json::to_vec_pretty(packet) {
                Ok(bytes) => {
                    if let Err(e) = crate::store::atomic_write(&path, &bytes) {
                        log::warn!("write {}: {e}", path.display());
                    }
                }
                Err(e) => log::error!("serialize handoff: {e}"),
            }
        }
    }

    pub(crate) fn ack_handoff(
        &self,
        sid: &SessionId,
        hid: &HandoffId,
        status: HandoffStatus,
        note: Option<String>,
    ) -> Result<serde_json::Value, ApiError> {
        if !matches!(status, HandoffStatus::Acked | HandoffStatus::Done) {
            return Err(ApiError::bad_request("status must be acked or done"));
        }
        let mut st = lock(&self.inner.state);
        let h = st
            .handoffs
            .get_mut(hid)
            .ok_or_else(|| ApiError::not_found(format!("handoff {hid}")))?;
        if h.to.session_id.as_ref().is_some_and(|t| t != sid) {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "handoff is addressed to another session",
            ));
        }
        h.status = status;
        drop(st);
        self.mark_dirty();
        self.audit(
            "handoff",
            serde_json::json!({"handoff": hid, "status": status, "note": note}),
        );
        Ok(serde_json::json!({}))
    }

    // ------------------------------------------------------------------
    // Artifacts (plan / triage / rca / report); reviews → review.rs
    // ------------------------------------------------------------------

    pub(crate) async fn submit_artifact(
        &self,
        sid: &SessionId,
        payload: ArtifactPayload,
    ) -> Result<serde_json::Value, ApiError> {
        let session = lock(&self.inner.state)
            .sessions
            .get(sid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown session {sid}")))?;
        match payload {
            ArtifactPayload::Review(r) => self.submit_review(&session, r).await,
            other => self.submit_simple_artifact(&session, other).await,
        }
    }

    /// Fill header, persist, return the artifact id.
    pub(crate) async fn store_artifact(
        &self,
        session: &SessionRecord,
        payload: &mut ArtifactPayload,
        project_root: Option<String>,
    ) -> ArtifactId {
        let aid = ArtifactId(id::new_id(id::prefix::ARTIFACT));
        let commit = match session.cwd.clone() {
            Some(c) => tokio::task::spawn_blocking(move || util::git_head(Path::new(&c)))
                .await
                .unwrap_or(None),
            None => None,
        };
        let kind = payload.kind();
        let header = ArtifactHeader {
            v: PROTO_VERSION,
            id: aid.clone(),
            kind,
            pipeline_id: session.stage.as_ref().map(|s| s.pipeline_id.clone()),
            stage: session.stage.as_ref().map(|s| s.stage),
            author: AuthorRef {
                agent: session.agent,
                session_id: session.session_id.clone(),
                model: session.model.clone(),
            },
            created_ms: id::now_ms(),
            commit,
        };
        match payload {
            ArtifactPayload::Plan(d) => d.header = header,
            ArtifactPayload::Review(r) => r.header = header,
            ArtifactPayload::Triage(t) => t.header = header,
            ArtifactPayload::Rca(r) => r.header = header,
            ArtifactPayload::Report(r) => r.header = header,
        }
        lock(&self.inner.state).artifacts.insert(
            aid.clone(),
            StoredArtifact {
                payload: payload.clone(),
                project_root: project_root.clone(),
            },
        );
        self.mark_dirty();
        if let Some(root) = project_root.or_else(|| session.git_root.clone()) {
            let dir = paths::project_dir(Path::new(&root)).join("artifacts").join(
                session
                    .stage
                    .as_ref()
                    .map(|s| s.pipeline_id.0.clone())
                    .unwrap_or_else(|| "adhoc".into()),
            );
            let json = serde_json::to_vec_pretty(payload).unwrap_or_default();
            let md = format!(
                "# {:?} artifact {}\n\n```json\n{}\n```\n",
                kind,
                aid,
                String::from_utf8_lossy(&json)
            );
            for (name, bytes) in [
                (format!("{aid}.json"), json.clone()),
                (format!("{aid}.md"), md.into_bytes()),
            ] {
                if let Err(e) = crate::store::atomic_write(&dir.join(name), &bytes) {
                    log::warn!("write artifact: {e}");
                }
            }
        }
        let pipeline_id = session.stage.as_ref().map(|s| s.pipeline_id.clone());
        let a = aid.clone();
        self.publish(move |seq| DaemonEvent::Artifact {
            seq,
            id: a,
            kind,
            pipeline_id,
        });
        aid
    }

    /// The running stage this session owns, if it matches `expected`.
    pub(crate) fn owned_stage(
        &self,
        session: &SessionRecord,
        expected: &[StageKind],
    ) -> Option<(PipelineSpec, StageKind)> {
        let r = session.stage.as_ref()?;
        if !expected.contains(&r.stage) {
            return None;
        }
        let st = lock(&self.inner.state);
        let p = st.pipelines.get(&r.pipeline_id)?;
        let s = p.stage(r.stage)?;
        matches!(
            s.status,
            StageStatus::Running | StageStatus::Blocked | StageStatus::AwaitingHuman
        )
        .then(|| (p.clone(), r.stage))
    }

    async fn submit_simple_artifact(
        &self,
        session: &SessionRecord,
        mut payload: ArtifactPayload,
    ) -> Result<serde_json::Value, ApiError> {
        let kind = payload.kind();
        let expected: &[StageKind] = match kind {
            ArtifactKind::Plan => &[StageKind::Plan],
            ArtifactKind::Triage => &[StageKind::Triage],
            ArtifactKind::Rca => &[StageKind::Deep],
            ArtifactKind::Report => &[StageKind::Report],
            _ => &[],
        };
        let owned = self.owned_stage(session, expected);
        let project_root = owned
            .as_ref()
            .map(|(p, _)| p.project.root.clone())
            .or_else(|| session.git_root.clone());

        let g1 = match &payload {
            ArtifactPayload::Plan(d) => starfane_evidence::validate_g1_plan(d),
            ArtifactPayload::Triage(t) => starfane_evidence::validate_g1_triage(t),
            ArtifactPayload::Rca(r) => starfane_evidence::validate_g1_rca(r),
            ArtifactPayload::Report(r) => starfane_evidence::validate_g1_report(r),
            ArtifactPayload::Review(r) => starfane_evidence::validate_g1_review(r),
        };
        let mut g2: BTreeMap<String, G2Outcome> = BTreeMap::new();
        if g1.ok() {
            g2 = self
                .g2_for_artifact(&payload, session, owned.as_ref().map(|(p, _)| p))
                .await;
        }
        let evidence_ok = match &payload {
            ArtifactPayload::Triage(_) => g2.values().any(|o| o.accepted()) || g2.is_empty(),
            ArtifactPayload::Rca(_) => g2.values().all(|o| o.accepted()),
            _ => true,
        };
        let accepted = g1.ok() && evidence_ok;
        if !accepted {
            return to_json(SubmitOutcome {
                artifact_id: ArtifactId::default(),
                accepted: false,
                g1,
                g2,
                review_id: None,
                pending_falsifier: Vec::new(),
                next: Some(if evidence_ok {
                    "G1 未通过：按 issues 补全字段后重新提交".into()
                } else {
                    "G2 未通过：证据引用无法在代码/日志中逐字找到，请修正后重新提交".into()
                }),
            });
        }
        let aid = self
            .store_artifact(session, &mut payload, project_root)
            .await;
        let next = match owned {
            Some((p, stage)) => self.on_artifact_accepted(&p, stage, &aid, &payload).await?,
            None => "已保存（该会话当前没有对应的流程阶段）".into(),
        };
        to_json(SubmitOutcome {
            artifact_id: aid,
            accepted: true,
            g1,
            g2,
            review_id: None,
            pending_falsifier: Vec::new(),
            next: Some(next),
        })
    }

    async fn g2_for_artifact(
        &self,
        payload: &ArtifactPayload,
        session: &SessionRecord,
        pipeline: Option<&PipelineSpec>,
    ) -> BTreeMap<String, G2Outcome> {
        let dir = match payload {
            ArtifactPayload::Rca(_) => pipeline
                .and_then(|p| {
                    lock(&self.inner.state)
                        .worktrees
                        .get(&p.id)
                        .map(|w| w.path.clone())
                })
                .or_else(|| pipeline.map(|p| p.project.root.clone())),
            _ => pipeline.map(|p| p.project.root.clone()),
        }
        .or_else(|| session.cwd.clone());
        let Some(dir) = dir else {
            return BTreeMap::new();
        };
        let payload = payload.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = BTreeMap::new();
            let base = PathBuf::from(&dir);
            let Some(head) = util::git_head(&base) else {
                return out;
            };
            let src = starfane_evidence::GitSource::new(base.clone());
            let cfg = g2_config();
            match &payload {
                ArtifactPayload::Triage(t) => {
                    for (i, h) in t.hypotheses.iter().enumerate() {
                        out.insert(
                            format!("hypotheses[{i}]"),
                            starfane_evidence::verify_g2_evidence(
                                &src,
                                &head,
                                &base,
                                &h.evidence_refs,
                                &[],
                                &cfg,
                            ),
                        );
                    }
                }
                ArtifactPayload::Rca(r) => {
                    out.insert(
                        "root_cause".into(),
                        starfane_evidence::verify_g2_evidence(
                            &src,
                            &head,
                            &base,
                            &r.root_cause.evidence,
                            &r.root_cause.trigger_path,
                            &cfg,
                        ),
                    );
                }
                _ => {}
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    async fn on_artifact_accepted(
        &self,
        p: &PipelineSpec,
        stage: StageKind,
        aid: &ArtifactId,
        payload: &ArtifactPayload,
    ) -> Result<String, ApiError> {
        let pid = p.id.clone();
        self.update_pipeline(&pid, |p| {
            if let Some(s) = p.stage_mut(stage) {
                s.artifacts.push(aid.clone());
            }
        })?;
        match (stage, payload) {
            (StageKind::Plan, ArtifactPayload::Plan(d)) => {
                self.update_pipeline(&pid, |p| {
                    p.status = PipelineStatus::AwaitingHuman;
                    if let Some(s) = p.stage_mut(StageKind::Plan) {
                        s.status = StageStatus::AwaitingHuman;
                    }
                })?;
                let mut body = format!("**目标**：{}\n\n**任务**：\n", p.goal);
                for t in d.tasks.iter().take(8) {
                    body.push_str(&format!("- {}\n", t.title));
                }
                body.push_str("\n**验收标准**：\n");
                for a in d.acceptance.iter().take(6) {
                    body.push_str(&format!("- {a}\n"));
                }
                self.notify(Notice {
                    key: format!(
                        "plan-{}-{}",
                        util::short_id(pid.as_str()),
                        util::short_id(aid.as_str())
                    ),
                    kind: NoticeKind::PlanApproval,
                    title: format!("📝 计划待批：{}", util::sanitize_line(&p.goal, 40)),
                    body_md: body,
                    pipeline_id: Some(pid.clone()),
                    session_id: None,
                    review_id: None,
                    actions: vec![
                        action("approve_plan", "批准计划", ActionStyle::Primary),
                        action(
                            "request_changes",
                            "要求修改（在话题中回复意见）",
                            ActionStyle::Default,
                        ),
                        action("cancel_pipeline", "取消", ActionStyle::Danger),
                    ],
                    binding_form: Vec::new(),
                    reply_session: None,
                });
                Ok("计划已提交，等待人工批准".into())
            }
            (StageKind::Triage, ArtifactPayload::Triage(t)) => {
                let auto = p.options.auto_deep;
                self.update_pipeline(&pid, |p| {
                    if let Some(s) = p.stage_mut(StageKind::Triage) {
                        s.status = StageStatus::Done;
                        s.ended_ms = Some(id::now_ms());
                    }
                    if !auto {
                        p.status = PipelineStatus::AwaitingHuman;
                        if let Some(s) = p.stage_mut(StageKind::Deep) {
                            s.status = StageStatus::AwaitingHuman;
                        }
                    }
                })?;
                let mut body = format!("**现象**：{}\n\n", t.symptom);
                for h in &t.hypotheses {
                    body.push_str(&format!(
                        "- **假设**（{:.0}%）：{}\n",
                        h.confidence * 100.0,
                        h.claim
                    ));
                }
                if let Some(m) = &t.mitigation {
                    body.push_str(&format!("\n**止血建议**：{m}\n"));
                }
                body.push_str(if auto {
                    "\n_初步假设，已自动进入深挖_\n"
                } else {
                    "\n_初步假设，等待你决定是否深挖_\n"
                });
                self.notify(Notice {
                    key: format!("triage-{}", util::short_id(aid.as_str())),
                    kind: NoticeKind::TriageReady,
                    title: format!("🔎 快筛结论：{}", util::sanitize_line(&p.goal, 40)),
                    body_md: body,
                    pipeline_id: Some(pid.clone()),
                    session_id: None,
                    review_id: None,
                    actions: vec![
                        action(
                            if auto { "stop_deep" } else { "continue_deep" },
                            if auto { "停止深挖" } else { "继续深挖" },
                            ActionStyle::Primary,
                        ),
                        action("adopt_mitigation", "采用止血方案", ActionStyle::Default),
                    ],
                    binding_form: Vec::new(),
                    reply_session: None,
                });
                if auto {
                    match self.start_stage(&pid, StageKind::Deep).await {
                        Ok(()) => Ok("快筛结论已推送，已自动进入深挖".into()),
                        Err(e) => Ok(format!("快筛结论已推送；深挖阶段启动失败：{}", e.message)),
                    }
                } else {
                    Ok("快筛结论已推送，等待人工决定是否深挖".into())
                }
            }
            (StageKind::Deep, ArtifactPayload::Rca(r)) => {
                self.advance(&pid, StageKind::Deep).await?;
                let mut body = format!("**根因**：{}\n", r.root_cause.claim);
                if let Some(f) = &r.fix {
                    body.push_str(&format!(
                        "**修复分支**：`{}`（{} 个文件）\n",
                        f.branch,
                        f.files.len()
                    ));
                }
                if let Some(v) = &r.verification_after {
                    body.push_str(&format!("**回归**：`{}` → {:?}\n", v.cmd, v.outcome));
                }
                if !r.ruled_out.is_empty() {
                    body.push_str(&format!("**已排除假设**：{} 个\n", r.ruled_out.len()));
                }
                self.notify(final_notice(p, "🧭 深挖完成", body));
                Ok("根因分析已提交，流程完成".into())
            }
            (StageKind::Report, ArtifactPayload::Report(r)) => {
                let doc = PathBuf::from(&p.project.root)
                    .join("docs")
                    .join("changes")
                    .join(format!("{}.md", pid));
                let text = format!("# {}\n\n{}\n", p.goal, r.summary_md);
                match crate::store::atomic_write(&doc, text.as_bytes()) {
                    Ok(()) => self.set_report_doc_path(aid, &doc),
                    Err(e) => log::warn!("write {}: {e}", doc.display()),
                }
                self.advance(&pid, StageKind::Report).await?;
                let facts = self.pipeline_facts(&pid);
                self.notify(final_notice(
                    p,
                    "🏁 流程完成",
                    format!("{}\n\n{}", facts, r.summary_md),
                ));
                Ok(format!("最终汇报已写入 {}", doc.display()))
            }
            _ => Ok("已保存".into()),
        }
    }

    fn set_report_doc_path(&self, aid: &ArtifactId, doc: &Path) {
        if let Some(a) = lock(&self.inner.state).artifacts.get_mut(aid) {
            if let ArtifactPayload::Report(r) = &mut a.payload {
                r.doc_path = Some(doc.to_string_lossy().into_owned());
            }
        }
        self.mark_dirty();
    }

    /// Deterministic facts line for the final card.
    fn pipeline_facts(&self, pid: &PipelineId) -> String {
        let st = lock(&self.inner.state);
        let Some(p) = st.pipelines.get(pid) else {
            return String::new();
        };
        let mins = id::now_ms().saturating_sub(p.created_ms) / 60_000;
        let rounds = p
            .stage(StageKind::Implement)
            .map(|s| s.round + 1)
            .unwrap_or(1);
        let (mut delivered, mut dropped, mut fabricated) = (0, 0, 0);
        for r in st
            .reviews
            .values()
            .filter(|r| r.pipeline_id.as_ref() == Some(pid))
        {
            for f in &r.record.findings {
                match f.disposition {
                    Disposition::Delivered => delivered += 1,
                    Disposition::Dropped => dropped += 1,
                    Disposition::Fabricated => fabricated += 1,
                    _ => {}
                }
            }
        }
        format!(
            "**耗时**：{mins} 分钟 · **实现轮次**：{rounds} · **Review**：交付 {delivered} / 证伪剔除 {dropped} / 臆造拦截 {fabricated}"
        )
    }
}

fn render_handoff(h: &HandoffPacket) -> String {
    let mut s = format!(
        "# 交接 {}\n\n- 来自：{}（{}）\n- 意图：{:?} · 模式：{:?}\n",
        h.id, h.from.agent, h.from.session_id, h.intent, h.mode
    );
    if let Some(r) = &h.repo {
        s.push_str(&format!(
            "- 仓库：`{}` 分支 `{}` head `{}`\n",
            r.root,
            r.branch.as_deref().unwrap_or("?"),
            r.head.as_deref().unwrap_or("?")
        ));
    }
    s.push_str(&format!(
        "\n## 摘要\n{}\n\n## 详情\n{}\n",
        h.summary, h.body_md
    ));
    if !h.todo.is_empty() {
        s.push_str("\n## 待办\n");
        for t in &h.todo {
            s.push_str(&format!("- {t}\n"));
        }
    }
    if let Some(t) = &h.self_test {
        s.push_str(&format!("\n自测：`{}` → {:?}\n", t.cmd, t.outcome));
    }
    s.push_str("\n> 处理后请调用 ack_handoff。\n");
    s
}

fn binding_notice(p: &PipelineSpec) -> Notice {
    let form: Vec<(String, String)> = p
        .stages
        .iter()
        .map(|s| (s.id.as_str().to_string(), s.binding.to_shorthand()))
        .collect();
    let mut body = format!(
        "**目标**：{}\n**项目**：`{}`\n\n",
        p.goal,
        util::project_name(&p.project.root)
    );
    for (stage, b) in &form {
        body.push_str(&format!("- {stage} → `{b}`\n"));
    }
    if let Some(u) = &p.source.utterance {
        body.push_str(&format!("\n_原话_：{}\n", util::sanitize_line(u, 200)));
    }
    Notice {
        key: format!("bind-{}", util::short_id(p.id.as_str())),
        kind: NoticeKind::BindingConfirm,
        title: format!(
            "{} 确认绑定：{}",
            if p.playbook == PlaybookKind::Debug {
                "🐞"
            } else {
                "🧩"
            },
            util::sanitize_line(&p.goal, 40)
        ),
        body_md: body,
        pipeline_id: Some(p.id.clone()),
        session_id: None,
        review_id: None,
        actions: vec![
            action("confirm_pipeline", "确认启动", ActionStyle::Primary),
            action("cancel_pipeline", "取消", ActionStyle::Danger),
        ],
        binding_form: form,
        reply_session: None,
    }
}

fn final_notice(p: &PipelineSpec, title: &str, body: String) -> Notice {
    Notice {
        key: format!("final-{}", util::short_id(p.id.as_str())),
        kind: NoticeKind::FinalReport,
        title: format!("{title}：{}", util::sanitize_line(&p.goal, 40)),
        body_md: body,
        pipeline_id: Some(p.id.clone()),
        session_id: None,
        review_id: None,
        actions: Vec::new(),
        binding_form: Vec::new(),
        reply_session: None,
    }
}
