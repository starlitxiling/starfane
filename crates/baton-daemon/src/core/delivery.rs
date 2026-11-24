//! Delivery ladder (architecture §5): L0 inbox → L1 `codex queue` → L2 gated
//! tmux paste; L4 interactive spawn for new sessions.

use super::state::{BindPurpose, PendingBind, State};
use super::{lock, util, Core};
use baton_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Long text thresholds above which `Dispatch` persists content to `<state_dir>/inputs/paste-<id>.md`
/// and injects a clean single-line file pointer instead of overflowing the terminal paste buffer.
const LONG_TEXT_CHAR_THRESHOLD: usize = 1000;
const LONG_TEXT_LINE_THRESHOLD: usize = 12;

/// Prepare dispatch text & optional paste / image attachments.
///
/// - Image attachments (`kind == "image"`, or `path` present with empty `content`) are strictly
///   validated to reside under `<state_dir>/inputs/` and formatted as clean single-line image path
///   references readable by Claude / Codex / AGY.
/// - Text attachments or long text (>1000 chars or >12 lines) are persisted to
///   `<state_dir>/inputs/paste-<id>.md` (0600) and referenced via a clean single-line pointer.
pub(crate) fn prepare_dispatch_payload(
    state_dir: &Path,
    raw_text: &str,
    attachments: &[PasteAttachment],
) -> Result<(String, Option<PathBuf>), ApiError> {
    let trimmed = raw_text.trim();
    let mut image_paths: Vec<PathBuf> = Vec::new();
    let mut text_attachments: Vec<&PasteAttachment> = Vec::new();

    for att in attachments {
        let is_image = att
            .kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case("image"))
            || (att.content.trim().is_empty() && att.path.is_some());
        if is_image {
            let raw_path = att
                .path
                .as_deref()
                .ok_or_else(|| ApiError::bad_request("image attachment missing path"))?;
            let valid = super::uploads::validate_uploaded_image_path(state_dir, raw_path)?;
            image_paths.push(valid);
        } else if !att.content.trim().is_empty() {
            text_attachments.push(att);
        }
    }

    if trimmed.is_empty() && text_attachments.is_empty() && image_paths.is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }

    let text_is_long = trimmed.chars().count() > LONG_TEXT_CHAR_THRESHOLD
        || trimmed.lines().count() > LONG_TEXT_LINE_THRESHOLD;

    if text_attachments.is_empty() && !text_is_long {
        if image_paths.is_empty() {
            return Ok((trimmed.to_string(), None));
        }
        let imgs_joined = image_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let pointer = if trimmed.is_empty() {
            if image_paths.len() == 1 {
                format!("请查看这张图片：{imgs_joined}")
            } else {
                format!("请查看以下图片：{imgs_joined}")
            }
        } else {
            let prefix = util::sanitize_line(trimmed, 800);
            format!("{prefix}（附图片：{imgs_joined}，请直接读取查看图片内容）")
        };
        return Ok((pointer, Some(image_paths[0].clone())));
    }

    let file_id = id::new_id("P");
    let now_ms = id::now_ms();
    let path = state_dir.join("inputs").join(format!("paste-{file_id}.md"));

    let mut md = format!("<!-- baton paste input | id: {file_id} | created_ms: {now_ms} -->\n");
    let mut content_lines: usize = 0;

    if text_is_long {
        md.push_str("\n## 粘贴文本\n\n");
        md.push_str(raw_text);
        if !raw_text.ends_with('\n') {
            md.push('\n');
        }
        content_lines += raw_text.lines().count().max(1);
    }

    for (idx, att) in text_attachments.iter().enumerate() {
        let heading = att
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|t| format!("## {t}"))
            .unwrap_or_else(|| {
                if text_attachments.len() == 1 {
                    "## 粘贴内容".to_string()
                } else {
                    format!("## 粘贴内容 {}", idx + 1)
                }
            });
        md.push('\n');
        md.push_str(&heading);
        md.push_str("\n\n");
        md.push_str(&att.content);
        if !att.content.ends_with('\n') {
            md.push('\n');
        }
        content_lines += att.content.lines().count().max(1);
    }

    crate::store::write_private(&path, md.as_bytes())
        .map_err(|e| ApiError::internal(format!("save paste input {}: {e}", path.display())))?;

    let line_count = content_lines.max(1);
    let img_suffix = if image_paths.is_empty() {
        String::new()
    } else {
        let imgs_joined = image_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        format!("；附图片：{imgs_joined}")
    };
    let pointer = if !text_is_long && !trimmed.is_empty() {
        let prefix = util::sanitize_line(trimmed, 800);
        format!(
            "{prefix}（附粘贴内容已保存至 {}，共 {line_count} 行{img_suffix}，请直接读取）",
            path.display()
        )
    } else {
        format!(
            "请阅读并处理以下粘贴内容：{}（共 {line_count} 行{img_suffix}）",
            path.display()
        )
    };

    Ok((pointer, Some(path)))
}

/// What to spawn for a stage / falsifier.
pub(crate) struct SpawnPlan {
    pub purpose: BindPurpose,
    pub agent: AgentKind,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub cwd: String,
    pub name: String,
    pub prompt: String,
    pub project_root: String,
}

impl Core {
    pub(crate) fn gate_config(&self) -> baton_tmux::GateConfig {
        let g = &self.inner.cfg.gate;
        baton_tmux::GateConfig {
            user_inactive_secs: g.user_inactive_secs,
            paste_settle_ms: g.paste_settle_ms,
            ack_timeout_ms: g.ack_timeout_ms,
            max_pointer_len: g.max_pointer_len,
            ..baton_tmux::GateConfig::default()
        }
    }

    /// `Dispatch`: store as an inbox message (L0) then try to wake the target.
    /// When `interactive == true` (explicit user send from Web UI), runs only hard tmux safety
    /// checks (`PaneAlive`, `NotInMode`, `NotSynchronized`) and injects immediately without waiting
    /// for a Hook ACK.
    pub(crate) async fn dispatch(
        &self,
        target: DispatchTarget,
        text: String,
        attachments: Vec<PasteAttachment>,
        allow: Vec<DeliveryLevel>,
        interactive: bool,
    ) -> Result<DeliveryReport, ApiError> {
        let (text, _saved_path) =
            prepare_dispatch_payload(&self.inner.state_dir, &text, &attachments)?;

        if interactive {
            let report = self.dispatch_interactive(&target, &text).await?;
            self.publish(|seq| DaemonEvent::Delivery {
                seq,
                report: report.clone(),
                target,
            });
            return Ok(report);
        }

        let session_id = self.resolve_dispatch_target(&target)?;
        let allow = if allow.is_empty() {
            DeliveryLevel::DEFAULT_ALLOW.to_vec()
        } else {
            allow
        };
        let item_id = id::new_id("M");
        {
            let mut st = lock(&self.inner.state);
            st.push_inbox(
                &session_id,
                InboxItem {
                    id: item_id.clone(),
                    kind: InboxKind::Message,
                    from: None,
                    created_ms: id::now_ms(),
                    title: util::sanitize_line(&text, 60),
                    body_md: text.clone(),
                    stage: None,
                    artifact: None,
                    handoff_id: None,
                    review_id: None,
                },
            );
        }
        self.mark_dirty();
        let max = self.inner.cfg.gate.max_pointer_len;
        let single = util::sanitize_line(&text, max);
        // Short single-line human messages are delivered verbatim; long ones as a pointer.
        let delivered_verbatim =
            !text.contains('\n') && text.chars().count() <= max && !single.starts_with('/');
        let line = if delivered_verbatim {
            single
        } else {
            format!("baton: 你有一条新消息（{item_id}），请调用 baton 的 read_inbox 工具查看")
        };
        let report = self.deliver_line(&session_id, &line, &allow).await;
        if delivered_verbatim && report.status == DeliveryStatus::Delivered {
            self.consume_delivered_message(&session_id, &item_id);
        }
        self.publish(|seq| DaemonEvent::Delivery {
            seq,
            report: report.clone(),
            target,
        });
        Ok(report)
    }

    async fn dispatch_interactive(
        &self,
        target: &DispatchTarget,
        text: &str,
    ) -> Result<DeliveryReport, ApiError> {
        let (session_id, pane, discovered_only) = self.resolve_interactive_target(target)?;

        let core = self.clone();
        let pane_for_gate = pane.clone();
        let gate = tokio::task::spawn_blocking(move || {
            let panes = core.inner.tmux.list_panes().map_err(|e| e.to_string())?;
            let info = panes
                .into_iter()
                .find(|p| p.pane_id == pane_for_gate)
                .ok_or_else(|| format!("pane {pane_for_gate} not found"))?;
            Ok::<_, String>(baton_tmux::check_interactive_gate(&info))
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .unwrap_or_else(|e| GateReport::from_checks(vec![(GateCheck::PaneAlive, false)], Some(e)));

        if !gate.passed {
            if let Some(sid) = &session_id {
                self.enqueue_message_fallback(sid, text);
            }
            return Ok(DeliveryReport {
                level: DeliveryLevel::L0Inbox,
                status: DeliveryStatus::Queued,
                gate: Some(gate),
                pane: Some(pane),
                ack: None,
            });
        }

        let core = self.clone();
        let pane_for_inject = pane.clone();
        let clean_text = text.replace('\r', "");
        let cfg = self.gate_config();
        let injected = tokio::task::spawn_blocking(move || {
            core.inner
                .tmux
                .inject_interactive(&pane_for_inject, &clean_text, &cfg)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

        match injected {
            Ok(()) => {
                // Direct L2 injection already delivered the text to the pane; do NOT leave
                // an unread item behind in `st.inbox` so `read_inbox` never replays old messages.
                Ok(DeliveryReport {
                    level: DeliveryLevel::L2Inject,
                    status: DeliveryStatus::Delivered,
                    gate: Some(gate),
                    pane: Some(pane),
                    ack: Some(if discovered_only {
                        "unverified_legacy".to_string()
                    } else {
                        "sent".to_string()
                    }),
                })
            }
            Err(baton_tmux::TmuxError::InvalidText(m)) => Err(ApiError::bad_request(m)),
            Err(e) => {
                if let Some(sid) = &session_id {
                    self.enqueue_message_fallback(sid, text);
                }
                Ok(DeliveryReport {
                    level: DeliveryLevel::L2Inject,
                    status: DeliveryStatus::Failed,
                    gate: Some(GateReport::from_checks(
                        gate.checks,
                        Some(format!("inject failed: {e}")),
                    )),
                    pane: Some(pane),
                    ack: None,
                })
            }
        }
    }

    fn enqueue_message_fallback(&self, sid: &SessionId, text: &str) {
        let item_id = id::new_id("M");
        let snapshot = {
            let mut st = lock(&self.inner.state);
            st.push_inbox(
                sid,
                InboxItem {
                    id: item_id,
                    kind: InboxKind::Message,
                    from: None,
                    created_ms: id::now_ms(),
                    title: util::sanitize_line(text, 60),
                    body_md: text.to_string(),
                    stage: None,
                    artifact: None,
                    handoff_id: None,
                    review_id: None,
                },
            );
            st.sessions.get(sid).cloned()
        };
        self.mark_dirty();
        if let Some(s) = snapshot {
            self.publish_session(s);
        }
    }

    fn consume_delivered_message(&self, sid: &SessionId, item_id: &str) {
        let snapshot = {
            let mut st = lock(&self.inner.state);
            if let Some(entries) = st.inbox.get_mut(sid) {
                entries.retain(|e| !(e.item.id == item_id && e.item.kind == InboxKind::Message));
            }
            if st.inbox.get(sid).is_some_and(Vec::is_empty) {
                st.inbox.remove(sid);
            }
            st.recount_unread(sid);
            st.sessions.get(sid).cloned()
        };
        self.mark_dirty();
        if let Some(s) = snapshot {
            self.publish_session(s);
        }
    }

    fn resolve_interactive_target(
        &self,
        target: &DispatchTarget,
    ) -> Result<(Option<SessionId>, PaneId, bool), ApiError> {
        let st = lock(&self.inner.state);
        match target {
            DispatchTarget::Session { id } => {
                let s = st
                    .sessions
                    .get(id)
                    .ok_or_else(|| ApiError::not_found(format!("session {id}")))?;
                if s.state == SessionState::Ended {
                    return Err(ApiError::new(
                        ErrorCode::Conflict,
                        format!("session {id} has ended"),
                    ));
                }
                let pane = s
                    .pane
                    .clone()
                    .ok_or_else(|| ApiError::not_found(format!("session {id} has no tmux pane")))?;
                Ok((Some(id.clone()), pane, s.discovered_only))
            }
            DispatchTarget::Pane { id } => match st.session_for_pane(id) {
                Some(s) => Ok((Some(s.session_id.clone()), id.clone(), s.discovered_only)),
                None => Ok((None, id.clone(), true)),
            },
            DispatchTarget::Stage { pipeline_id, stage } => {
                let p = st
                    .pipelines
                    .get(pipeline_id)
                    .ok_or_else(|| ApiError::not_found(format!("pipeline {pipeline_id}")))?;
                let stage_spec = p
                    .stage(*stage)
                    .ok_or_else(|| ApiError::not_found(format!("stage {stage}")))?;
                if let Some(sid) = &stage_spec.session_id {
                    if let Some(s) = st.sessions.get(sid) {
                        if let Some(pane) = &s.pane {
                            return Ok((Some(sid.clone()), pane.clone(), s.discovered_only));
                        }
                    }
                }
                if let Some(pane) = &stage_spec.pane {
                    return Ok((stage_spec.session_id.clone(), pane.clone(), true));
                }
                Err(ApiError::new(
                    ErrorCode::Conflict,
                    format!("stage {stage} of {pipeline_id} has no bound pane yet"),
                ))
            }
        }
    }

    fn resolve_dispatch_target(&self, target: &DispatchTarget) -> Result<SessionId, ApiError> {
        let st = lock(&self.inner.state);
        match target {
            DispatchTarget::Session { id } => st
                .sessions
                .contains_key(id)
                .then(|| id.clone())
                .ok_or_else(|| ApiError::not_found(format!("session {id}"))),
            DispatchTarget::Pane { id } => st
                .session_for_pane(id)
                .map(|s| s.session_id.clone())
                .ok_or_else(|| ApiError::not_found(format!("no live session in pane {id}"))),
            DispatchTarget::Stage { pipeline_id, stage } => {
                let p = st
                    .pipelines
                    .get(pipeline_id)
                    .ok_or_else(|| ApiError::not_found(format!("pipeline {pipeline_id}")))?;
                p.stage(*stage)
                    .and_then(|s| s.session_id.clone())
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorCode::Conflict,
                            format!("stage {stage} of {pipeline_id} has no bound session yet"),
                        )
                    })
            }
        }
    }

    /// Try L1 (codex) then L2 for an already-stored inbox entry.
    pub(crate) async fn deliver_line(
        &self,
        session_id: &SessionId,
        line: &str,
        allow: &[DeliveryLevel],
    ) -> DeliveryReport {
        let session = lock(&self.inner.state).sessions.get(session_id).cloned();
        let Some(session) = session else {
            return DeliveryReport {
                level: DeliveryLevel::L0Inbox,
                status: DeliveryStatus::Queued,
                gate: None,
                pane: None,
                ack: None,
            };
        };
        let ack_timeout = Duration::from_millis(self.inner.cfg.gate.ack_timeout_ms.max(1000));

        if allow.contains(&DeliveryLevel::L1CodexQueue)
            && session.agent == AgentKind::Codex
            && !session.is_subagent
            && !session.discovered_only
        {
            let ack = self.register_ack(session_id);
            match self.codex_queue(&session, line).await {
                Ok(()) => {
                    if tokio::time::timeout(ack_timeout, ack).await.is_ok() {
                        self.mark_inbox_poked(session_id);
                        return DeliveryReport {
                            level: DeliveryLevel::L1CodexQueue,
                            status: DeliveryStatus::Delivered,
                            gate: None,
                            pane: session.pane.clone(),
                            ack: None,
                        };
                    }
                    log::info!("codex queue for {session_id}: no ACK, falling back to L2");
                }
                Err(e) => log::info!("codex queue for {session_id} failed: {e}; trying L2"),
            }
        }

        if allow.contains(&DeliveryLevel::L2Inject) {
            if let Some(pane) = session.pane.clone() {
                let gate = self.check_gate(&session, &pane).await;
                if gate.passed {
                    let ack = self.register_ack(session_id);
                    let core = self.clone();
                    let (p, text, cfg) = (pane.clone(), line.to_string(), self.gate_config());
                    let injected = tokio::task::spawn_blocking(move || {
                        core.inner.tmux.inject_pointer(&p, &text, &cfg)
                    })
                    .await;
                    match injected {
                        Ok(Ok(())) => {
                            if session.discovered_only {
                                self.mark_inbox_poked(session_id);
                                return DeliveryReport {
                                    level: DeliveryLevel::L2Inject,
                                    status: DeliveryStatus::Delivered,
                                    gate: Some(gate),
                                    pane: Some(pane),
                                    ack: Some("unverified_legacy".into()),
                                };
                            }
                            let acked = tokio::time::timeout(ack_timeout, ack).await.is_ok();
                            if acked {
                                self.mark_inbox_poked(session_id);
                            } else {
                                self.add_unconfirmed(&session, line);
                            }
                            return DeliveryReport {
                                level: DeliveryLevel::L2Inject,
                                status: if acked {
                                    DeliveryStatus::Delivered
                                } else {
                                    DeliveryStatus::Unconfirmed
                                },
                                gate: Some(gate),
                                pane: Some(pane),
                                ack: None,
                            };
                        }
                        Ok(Err(e)) => log::warn!("inject into {pane} failed: {e}"),
                        Err(e) => log::error!("inject task failed: {e}"),
                    }
                    return DeliveryReport {
                        level: DeliveryLevel::L2Inject,
                        status: DeliveryStatus::Failed,
                        gate: Some(gate),
                        pane: Some(pane),
                        ack: None,
                    };
                }
                return DeliveryReport {
                    level: DeliveryLevel::L0Inbox,
                    status: DeliveryStatus::Queued,
                    gate: Some(gate),
                    pane: Some(pane),
                    ack: None,
                };
            }
        }
        DeliveryReport {
            level: DeliveryLevel::L0Inbox,
            status: DeliveryStatus::Queued,
            gate: None,
            pane: session.pane.clone(),
            ack: None,
        }
    }

    async fn check_gate(&self, session: &SessionRecord, pane: &PaneId) -> GateReport {
        let core = self.clone();
        let (pane, state, agent, cfg) = (
            pane.clone(),
            session.state,
            session.agent,
            self.gate_config(),
        );
        tokio::task::spawn_blocking(move || {
            let panes = core.inner.tmux.list_panes().map_err(|e| e.to_string())?;
            let clients = core.inner.tmux.list_clients().map_err(|e| e.to_string())?;
            let info = panes
                .into_iter()
                .find(|p| p.pane_id == pane)
                .ok_or_else(|| format!("pane {pane} not found"))?;
            let line = core
                .inner
                .tmux
                .cursor_line(&pane, info.cursor_y)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(baton_tmux::check_injection_gate(
                &info,
                &clients,
                state,
                agent,
                &line,
                &cfg,
                id::now_ms() / 1000,
            ))
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
        .unwrap_or_else(|e| GateReport::from_checks(vec![(GateCheck::PaneAlive, false)], Some(e)))
    }

    async fn codex_queue(&self, session: &SessionRecord, line: &str) -> Result<(), String> {
        let bin = self.inner.cfg.agent_bin(AgentKind::Codex);
        let thread = session.session_id.0.clone();
        // `--message=` form + safe prefix: codex rejects `--` and leading dashes (spikes §2).
        let msg = if line.starts_with("baton:") {
            line.to_string()
        } else {
            format!("baton: {line}")
        };
        tokio::task::spawn_blocking(move || {
            let out = Command::new(&bin)
                .args([
                    "queue",
                    "--thread",
                    &thread,
                    &format!("--message={msg}"),
                    "--resume",
                ])
                .output()
                .map_err(|e| format!("{bin} queue: {e}"))?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                return Err(format!("exit {:?}: {}", out.status.code(), stderr.trim()));
            }
            if stderr.contains("could not resume thread") {
                return Err(format!("queued but not resumed: {}", stderr.trim()));
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn mark_inbox_poked(&self, session: &SessionId) {
        let mut st = lock(&self.inner.state);
        if let Some(entries) = st.inbox.get_mut(session) {
            for e in entries.iter_mut() {
                e.poke_pending = false;
            }
        }
        drop(st);
        self.mark_dirty();
    }

    fn add_unconfirmed(&self, session: &SessionRecord, line: &str) {
        {
            let mut st = lock(&self.inner.state);
            st.unconfirmed
                .retain(|a| a.session_id.as_ref() != Some(&session.session_id));
            st.unconfirmed.push(AttentionItem {
                kind: attention::DELIVERY_UNCONFIRMED.into(),
                title: format!(
                    "投递未确认：{}@{} — {}",
                    session.agent,
                    session.pane.as_ref().map(|p| p.as_str()).unwrap_or("?"),
                    util::sanitize_line(line, 60)
                ),
                pipeline_id: session.stage.as_ref().map(|s| s.pipeline_id.clone()),
                session_id: Some(session.session_id.clone()),
                review_id: None,
                since_ms: id::now_ms(),
            });
        }
        self.mark_dirty();
        self.notify(crate::notice::Notice {
            key: format!("unconf-{}", util::short_id(session.session_id.as_str())),
            kind: crate::notice::NoticeKind::DeliveryUnconfirmed,
            title: "⚠ 投递未确认".into(),
            body_md: format!(
                "向 {}@{} 注入的消息 3–5 秒内未收到确认，请到终端查看：\n`{}`",
                session.agent,
                session.pane.as_ref().map(|p| p.as_str()).unwrap_or("?"),
                util::sanitize_line(line, 200)
            ),
            pipeline_id: session.stage.as_ref().map(|s| s.pipeline_id.clone()),
            session_id: Some(session.session_id.clone()),
            review_id: None,
            actions: Vec::new(),
            binding_form: Vec::new(),
            reply_session: None,
        });
    }

    /// On idle: retry delivery for undelivered inbox items.
    /// For a single short queued user message (≤ 300 chars, single-line, no attachment pointer),
    /// injects the message text directly instead of wrapping it in a `read_inbox` pointer.
    pub(crate) async fn poke_pending_inbox(&self, session_id: &SessionId) {
        let (pending_count, direct_msg) = {
            let st = lock(&self.inner.state);
            let pending: Vec<&super::state::InboxEntry> = st
                .inbox
                .get(session_id)
                .map(|v| {
                    v.iter()
                        .filter(|e| !e.delivered && e.poke_pending)
                        .collect()
                })
                .unwrap_or_default();
            let direct = if pending.len() == 1 && pending[0].item.kind == InboxKind::Message {
                let body = pending[0].item.body_md.trim();
                let is_short_plain = !body.is_empty()
                    && !body.contains('\n')
                    && body.chars().count() <= 300
                    && !body.starts_with('/')
                    && !body.contains("附粘贴内容")
                    && !body.contains("请阅读并处理以下粘贴内容")
                    && !body.contains("附图片：")
                    && !body.starts_with("请查看这张图片：")
                    && !body.starts_with("请查看以下图片：");
                is_short_plain.then(|| (pending[0].item.id.clone(), body.to_string()))
            } else {
                None
            };
            (pending.len(), direct)
        };
        if pending_count == 0 {
            return;
        }
        let (direct_id, line) = match direct_msg {
            Some((id, text)) => (Some(id), text),
            None => (
                None,
                format!(
                    "baton: 你有 {pending_count} 条未读任务/消息，请调用 baton 的 read_inbox 工具获取"
                ),
            ),
        };
        let core = self.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let report = core
                .deliver_line(&sid, &line, &[DeliveryLevel::L2Inject])
                .await;
            if report.status == DeliveryStatus::Delivered {
                if let Some(id) = direct_id {
                    core.consume_delivered_message(&sid, &id);
                }
            }
            log::debug!("idle poke {sid}: {:?}", report.status);
        });
    }

    /// L4: spawn a new interactive session in a tmux window.
    pub(crate) async fn spawn_session(
        &self,
        plan: SpawnPlan,
    ) -> Result<(BindToken, PaneId), ApiError> {
        let token = BindToken(id::new_id(id::prefix::BIND));
        let bin = PathBuf::from(self.inner.cfg.agent_bin(plan.agent));
        let mut extra = crate::config::TeamConfig::load(std::path::Path::new(&plan.project_root))
            .map(|t| t.spawn_args.get(&plan.agent).cloned().unwrap_or_default())
            .unwrap_or_else(|e| {
                log::warn!("team.toml: {e}");
                Vec::new()
            });
        baton_adapters::ensure_bypass_permission_arg(plan.agent, &mut extra);
        let argv = baton_adapters::build_l4_spawn_cmd(&baton_adapters::SpawnRequest {
            agent: plan.agent,
            bin,
            cwd: PathBuf::from(&plan.cwd),
            model: plan.model.clone(),
            effort: plan.effort,
            name: Some(plan.name.clone()),
            prompt: plan.prompt.clone(),
            extra_args: extra,
        })
        .map_err(|e| ApiError::bad_request(format!("spawn command: {e}")))?;

        // Register the bind before the process can emit its first hook.
        lock(&self.inner.state).binds.insert(
            token.clone(),
            PendingBind {
                purpose: plan.purpose.clone(),
                agent: plan.agent,
                pane: None,
                session_id: None,
                created_ms: id::now_ms(),
            },
        );
        let core = self.clone();
        let root = plan.project_root.clone();
        let window = SpawnWindowArgs {
            name: format!("baton:{}", plan.name),
            cwd: plan.cwd.clone(),
            token: token.clone(),
            argv,
        };
        let spawned = tokio::task::spawn_blocking(move || {
            let target_session = core.inner.tmux.list_panes().ok().and_then(|panes| {
                panes
                    .into_iter()
                    .find(|p| super::util::path_starts_with(&p.current_path, &root))
                    .map(|p| p.session_name)
            });
            core.inner.tmux.spawn_window(&baton_tmux::SpawnWindow {
                target_session,
                create_session: true,
                window_name: window.name,
                cwd: PathBuf::from(window.cwd),
                env: vec![(paths::ENV_BIND.to_string(), window.token.0.clone())],
                argv: window.argv,
            })
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
        match spawned {
            Ok(pane) => {
                if let Some(b) = lock(&self.inner.state).binds.get_mut(&token) {
                    b.pane = Some(pane.clone());
                }
                self.mark_dirty();
                self.watch_spawn_startup(token.clone(), pane.clone());
                Ok((token, pane))
            }
            Err(e) => {
                lock(&self.inner.state).binds.remove(&token);
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("tmux spawn: {e}"),
                ))
            }
        }
    }

    /// Spawned TUIs can stall on a workspace-trust prompt (spikes §1): warn if
    /// no hook arrives within 15s.
    fn watch_spawn_startup(&self, token: BindToken, pane: PaneId) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let stalled = lock(&core.inner.state)
                .binds
                .get(&token)
                .is_some_and(|b| b.session_id.is_none());
            if !stalled {
                return;
            }
            {
                let mut st = lock(&core.inner.state);
                st.unconfirmed.push(AttentionItem {
                    kind: attention::DELIVERY_UNCONFIRMED.into(),
                    title: format!("新窗口 {pane} 15 秒内未上报 hook：可能停在信任确认/登录界面"),
                    pipeline_id: None,
                    session_id: Some(super::discover::legacy_session_id(&pane)),
                    review_id: None,
                    since_ms: id::now_ms(),
                });
            }
            core.mark_dirty();
            core.refresh_attention();
        });
    }

    /// Inbox key for a target: the live session, else the pending bind key.
    pub(crate) fn inbox_key_for_bind(token: &BindToken) -> SessionId {
        State::bind_inbox_key(token)
    }
}

struct SpawnWindowArgs {
    name: String,
    cwd: String,
    token: BindToken,
    argv: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_dispatch_payload_handles_short_long_and_attachments() {
        let dir = std::env::temp_dir().join(format!("baton-paste-test-{}", id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Short text passes verbatim with no file created.
        let (out, path) = prepare_dispatch_payload(&dir, "  hello world  ", &[]).unwrap();
        assert_eq!(out, "hello world");
        assert!(path.is_none());

        // 2. Both empty is rejected.
        let err = prepare_dispatch_payload(
            &dir,
            "   ",
            &[PasteAttachment {
                kind: None,
                title: Some("empty".into()),
                content: "   ".into(),
                path: None,
            }],
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);

        // 3. Text > 1000 chars is saved to <state_dir>/inputs/paste-*.md.
        let big = "a".repeat(1005);
        let (ptr_big, p_big) = prepare_dispatch_payload(&dir, &big, &[]).unwrap();
        let p_big = p_big.expect("saved file for >1000 chars");
        assert!(p_big.starts_with(dir.join("inputs")));
        assert!(!ptr_big.contains('\n'));
        assert!(ptr_big.contains(&p_big.display().to_string()));
        let content_big = std::fs::read_to_string(&p_big).unwrap();
        assert!(content_big.contains(&big));

        // 4. Text > 12 lines is saved to file.
        let many_lines = (1..=15)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (ptr_lines, p_lines) = prepare_dispatch_payload(&dir, &many_lines, &[]).unwrap();
        let p_lines = p_lines.expect("saved file for >12 lines");
        assert!(!ptr_lines.contains('\n'));
        assert!(ptr_lines.contains("共 15 行"));
        assert!(std::fs::read_to_string(&p_lines)
            .unwrap()
            .contains("line-15"));

        // 5. Short instruction + attachment combines prefix + file path pointer.
        let att = vec![PasteAttachment {
            kind: Some("text".into()),
            title: Some("stacktrace".into()),
            content: "err1\nerr2\nerr3".into(),
            path: None,
        }];
        let (ptr_att, p_att) = prepare_dispatch_payload(&dir, "帮我定位这个报错", &att).unwrap();
        let p_att = p_att.expect("saved file for attachment");
        assert!(!ptr_att.contains('\n'));
        assert!(ptr_att.starts_with("帮我定位这个报错（附粘贴内容已保存至 "));
        assert!(ptr_att.contains("共 3 行"));
        let md = std::fs::read_to_string(&p_att).unwrap();
        assert!(md.contains("## stacktrace"));
        assert!(md.contains("err1\nerr2\nerr3"));

        // 6. Attachment-only (empty text) succeeds cleanly.
        let (ptr_only, p_only) = prepare_dispatch_payload(&dir, "", &att).unwrap();
        let p_only = p_only.expect("saved file for attachment-only");
        assert!(ptr_only.starts_with("请阅读并处理以下粘贴内容："));
        assert!(ptr_only.contains(&p_only.display().to_string()));

        // 7. Image attachment inside inputs/ works both image-only and with instruction;
        //    outside paths are rejected.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let up = super::super::uploads::save_uploaded_image(
            &dir,
            Some("shot.png"),
            Some("image/png"),
            png_b64,
        )
        .unwrap();
        let img_att = vec![PasteAttachment {
            kind: Some("image".into()),
            title: Some("shot.png".into()),
            content: String::new(),
            path: Some(up.path.clone()),
        }];
        let (ptr_img_only, _) = prepare_dispatch_payload(&dir, "", &img_att).unwrap();
        assert!(
            ptr_img_only.starts_with("请查看这张图片："),
            "{ptr_img_only}"
        );
        assert!(ptr_img_only.contains(&up.id));

        let (ptr_img_txt, _) = prepare_dispatch_payload(&dir, "分析这张截图", &img_att).unwrap();
        assert!(
            ptr_img_txt.starts_with("分析这张截图（附图片："),
            "{ptr_img_txt}"
        );
        assert!(ptr_img_txt.ends_with("，请直接读取查看图片内容）"));

        let bad_img = vec![PasteAttachment {
            kind: Some("image".into()),
            title: None,
            content: String::new(),
            path: Some("/etc/passwd".into()),
        }];
        assert!(prepare_dispatch_payload(&dir, "look", &bad_img).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
