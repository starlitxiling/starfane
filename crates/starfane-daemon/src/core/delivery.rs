//! Delivery ladder (architecture §5): L0 inbox → L1 `codex queue` → L2 gated
//! tmux paste; L4 interactive spawn for new sessions.

use super::state::{BindPurpose, PendingBind, State};
use super::{lock, util, Core};
use starfane_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Long text thresholds above which `Dispatch` persists content to `<state_dir>/inputs/paste-<id>.md`
/// and injects a clean single-line file pointer instead of overflowing the terminal paste buffer
/// (safely inside `validate_interactive_text`'s 4000-char bracketed-paste limit).
const LONG_TEXT_CHAR_THRESHOLD: usize = 2400;
const LONG_TEXT_LINE_THRESHOLD: usize = 40;

fn has_disallowed_control(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
}

enum ResolvedAttachment<'a> {
    Image {
        path: PathBuf,
        token: Option<&'a str>,
    },
    Text {
        att: &'a PasteAttachment,
        token: Option<&'a str>,
    },
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Prepare dispatch text & optional paste / image attachments.
///
/// - Image attachments (`kind == "image"`, or `path` present with empty `content`) are strictly
///   validated to reside under `<state_dir>/inputs/`.
/// - When attachments carry inline `token`s matching placeholders inside `raw_text` (e.g.
///   `"[粘贴 #1]"`, `"[图片 #1]"`), placeholders are expanded in a single forward pass in-place
///   so interleaved quotes, screenshots, and follow-up questions keep their exact original order.
/// - Within `LONG_TEXT_*` bounds, assembled multi-line content is returned directly for verbatim
///   bracketed paste; when longer (or containing unsafe control characters), the ordered assembled
///   content is written to `<state_dir>/inputs/paste-<id>.md` (0600) with a concise pointer.
pub(crate) fn prepare_dispatch_payload(
    state_dir: &Path,
    raw_text: &str,
    attachments: &[PasteAttachment],
) -> Result<(String, Option<PathBuf>), ApiError> {
    let mut resolved: Vec<ResolvedAttachment<'_>> = Vec::with_capacity(attachments.len());
    for att in attachments {
        let token = att
            .token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
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
            resolved.push(ResolvedAttachment::Image { path: valid, token });
        } else if !att.content.trim().is_empty() {
            resolved.push(ResolvedAttachment::Text { att, token });
        }
    }

    if raw_text.trim().is_empty() && resolved.is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }

    let norm_raw = normalize_newlines(raw_text);
    let mut token_entries: Vec<(&str, usize)> = Vec::new();
    for (idx, item) in resolved.iter().enumerate() {
        let tok_opt = match item {
            ResolvedAttachment::Image { token, .. } | ResolvedAttachment::Text { token, .. } => {
                *token
            }
        };
        if let Some(tok) = tok_opt {
            if norm_raw.contains(tok) && !token_entries.iter().any(|(existing, _)| *existing == tok)
            {
                token_entries.push((tok, idx));
            }
        }
    }

    if !token_entries.is_empty() {
        let mut anchored = vec![false; resolved.len()];
        let mut expanded = String::with_capacity(norm_raw.len());
        let mut non_token_text = String::with_capacity(norm_raw.len());
        let mut cursor = 0usize;

        while cursor < norm_raw.len() {
            let slice = &norm_raw[cursor..];
            let mut best: Option<(usize, usize, usize)> = None;
            for &(tok, idx) in &token_entries {
                if let Some(pos) = slice.find(tok) {
                    let is_better = match best {
                        None => true,
                        Some((best_pos, best_len, _)) => {
                            pos < best_pos || (pos == best_pos && tok.len() > best_len)
                        }
                    };
                    if is_better {
                        best = Some((pos, tok.len(), idx));
                    }
                }
            }

            let Some((pos, tok_len, idx)) = best else {
                expanded.push_str(slice);
                non_token_text.push_str(slice);
                break;
            };

            let prefix = &slice[..pos];
            expanded.push_str(prefix);
            non_token_text.push_str(prefix);
            anchored[idx] = true;
            let mut next_cursor = cursor + pos + tok_len;

            match &resolved[idx] {
                ResolvedAttachment::Image { path, .. } => {
                    expanded.push_str(&format!("[图片: {}]", path.display()));
                }
                ResolvedAttachment::Text { att, .. } => {
                    let body = normalize_newlines(&att.content);
                    if body.contains('\n') {
                        if !expanded.is_empty() && !expanded.ends_with('\n') {
                            while expanded.ends_with(' ') {
                                expanded.pop();
                            }
                            if !expanded.is_empty() && !expanded.ends_with('\n') {
                                expanded.push('\n');
                            }
                        }
                        expanded.push_str(&body);
                        let rest = &norm_raw[next_cursor..];
                        if !body.ends_with('\n') && !rest.is_empty() && !rest.starts_with('\n') {
                            expanded.push('\n');
                            if rest.starts_with(' ') {
                                next_cursor += 1;
                            }
                        }
                    } else {
                        expanded.push_str(&body);
                    }
                }
            }
            cursor = next_cursor;
        }

        let all_images: Vec<PathBuf> = resolved
            .iter()
            .filter_map(|r| match r {
                ResolvedAttachment::Image { path, .. } => Some(path.clone()),
                ResolvedAttachment::Text { .. } => None,
            })
            .collect();
        let has_any_text_att = resolved
            .iter()
            .any(|r| matches!(r, ResolvedAttachment::Text { .. }));

        if non_token_text.trim().is_empty() && !has_any_text_att && !all_images.is_empty() {
            let imgs_joined = all_images
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let pointer = if all_images.len() == 1 {
                format!("请查看这张图片：{imgs_joined}")
            } else {
                format!("请查看以下图片：{imgs_joined}")
            };
            return Ok((pointer, Some(all_images[0].clone())));
        }

        let mut assembled = expanded.trim().to_string();
        for (idx, item) in resolved.iter().enumerate() {
            if anchored[idx] {
                continue;
            }
            match item {
                ResolvedAttachment::Image { path, .. } => {
                    if !assembled.is_empty() {
                        assembled.push('\n');
                    }
                    assembled.push_str(&format!("[图片: {}]", path.display()));
                }
                ResolvedAttachment::Text { att, .. } => {
                    let body = normalize_newlines(&att.content);
                    if !assembled.is_empty() {
                        assembled.push_str("\n\n");
                        if let Some(title) = att
                            .title
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            assembled.push_str("## ");
                            assembled.push_str(title);
                            assembled.push('\n');
                        }
                    }
                    assembled.push_str(body.trim_end_matches('\n'));
                }
            }
        }

        let assembled_is_long = assembled.chars().count() > LONG_TEXT_CHAR_THRESHOLD
            || assembled.lines().count() > LONG_TEXT_LINE_THRESHOLD
            || has_disallowed_control(&assembled);

        if !assembled_is_long {
            return Ok((assembled, all_images.first().cloned()));
        }

        let file_id = id::new_id("P");
        let now_ms = id::now_ms();
        let path = state_dir.join("inputs").join(format!("paste-{file_id}.md"));
        let mut md = format!(
            "<!-- starfane paste input | id: {file_id} | created_ms: {now_ms} -->\n\n{assembled}"
        );
        if !md.ends_with('\n') {
            md.push('\n');
        }
        crate::store::write_private(&path, md.as_bytes())
            .map_err(|e| ApiError::internal(format!("save paste input {}: {e}", path.display())))?;

        let line_count = assembled.lines().count().max(1);
        let pointer = if all_images.is_empty() {
            format!(
                "请按原文顺序阅读并处理以下输入：{}（共 {line_count} 行）",
                path.display()
            )
        } else {
            let imgs_joined = all_images
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "请按原文顺序阅读并处理以下输入：{}（共 {line_count} 行；文内含图片：{imgs_joined}，请一并读取查看）",
                path.display()
            )
        };
        return Ok((pointer, Some(path)));
    }

    // Legacy / unanchored path (attachments omitted tokens or callers without tokens).
    let trimmed = norm_raw.trim();
    let mut image_paths: Vec<PathBuf> = Vec::new();
    let mut text_attachments: Vec<&PasteAttachment> = Vec::new();
    for item in resolved {
        match item {
            ResolvedAttachment::Image { path, .. } => image_paths.push(path),
            ResolvedAttachment::Text { att, .. } => text_attachments.push(att),
        }
    }

    let text_is_long = trimmed.chars().count() > LONG_TEXT_CHAR_THRESHOLD
        || trimmed.lines().count() > LONG_TEXT_LINE_THRESHOLD
        || has_disallowed_control(trimmed);

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

    let mut md = format!("<!-- starfane paste input | id: {file_id} | created_ms: {now_ms} -->\n");
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
    pub(crate) fn gate_config(&self) -> starfane_tmux::GateConfig {
        let g = &self.inner.cfg.gate;
        starfane_tmux::GateConfig {
            user_inactive_secs: g.user_inactive_secs,
            paste_settle_ms: g.paste_settle_ms,
            ack_timeout_ms: g.ack_timeout_ms,
            max_pointer_len: g.max_pointer_len,
            ..starfane_tmux::GateConfig::default()
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
            format!("starfane: 你有一条新消息（{item_id}），请调用 starfane 的 read_inbox 工具查看")
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

        // Remote SSH or Peer host interactive dispatch proxy.
        let (host_opt, local_pane) = starfane_tmux::split_host_prefix(pane.as_str());
        if let Some(host) = host_opt {
            let hid = HostId::new(host);
            let provider = self
                .inner
                .remote_nodes
                .get(&hid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("unknown remote host {host}")))?;
            let remote_sid = session_id
                .as_ref()
                .and_then(|s| s.as_str().strip_prefix(&format!("{host}:")))
                .map(SessionId::new);
            let settle_ms = self.gate_config().paste_settle_ms;
            let clean_text = text.replace('\r', "");
            return provider
                .dispatch_interactive(
                    remote_sid.as_ref(),
                    &PaneId::new(local_pane),
                    &clean_text,
                    settle_ms,
                )
                .await;
        }

        let core = self.clone();
        let pane_for_gate = pane.clone();
        let gate = tokio::task::spawn_blocking(move || {
            let panes = core.inner.mux.list_all_panes().map_err(|e| e.to_string())?;
            let info = panes
                .into_iter()
                .find(|p| p.pane_id == pane_for_gate)
                .ok_or_else(|| format!("pane {pane_for_gate} not found"))?;
            Ok::<_, String>(starfane_tmux::check_interactive_gate(&info))
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

        let core_paste = self.clone();
        let pane_for_paste = pane.clone();
        let clean_text = text.replace('\r', "");
        let settle_ms = self.gate_config().paste_settle_ms;
        let pasted = tokio::task::spawn_blocking(move || {
            core_paste
                .inner
                .mux
                .paste_interactive_text(&pane_for_paste, &clean_text)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

        let injected = match pasted {
            Ok(()) => {
                tokio::time::sleep(Duration::from_millis(settle_ms)).await;
                let core_enter = self.clone();
                let pane_for_enter = pane.clone();
                tokio::task::spawn_blocking(move || {
                    core_enter.inner.mux.submit_enter(&pane_for_enter)
                })
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
            }
            Err(e) => Err(e),
        };

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
            Err(starfane_tmux::TmuxError::InvalidText(m)) => Err(ApiError::bad_request(m)),
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
                    let core_paste = self.clone();
                    let (p_paste, text, cfg) = (pane.clone(), line.to_string(), self.gate_config());
                    let settle_ms = cfg.paste_settle_ms;
                    let pasted = tokio::task::spawn_blocking(move || {
                        core_paste
                            .inner
                            .mux
                            .paste_pointer_text(&p_paste, &text, &cfg)
                    })
                    .await;
                    let injected = match pasted {
                        Ok(Ok(())) => {
                            tokio::time::sleep(Duration::from_millis(settle_ms)).await;
                            let core_enter = self.clone();
                            let p_enter = pane.clone();
                            tokio::task::spawn_blocking(move || {
                                core_enter.inner.mux.submit_enter(&p_enter)
                            })
                            .await
                        }
                        other => other,
                    };
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
            let panes = core.inner.mux.list_all_panes().map_err(|e| e.to_string())?;
            let clients = core
                .inner
                .mux
                .list_all_clients()
                .map_err(|e| e.to_string())?;
            let info = panes
                .into_iter()
                .find(|p| p.pane_id == pane)
                .ok_or_else(|| format!("pane {pane} not found"))?;
            let line = core
                .inner
                .mux
                .cursor_line(&pane, info.cursor_y)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(starfane_tmux::check_injection_gate(
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
        let msg = if line.starts_with("starfane:") {
            line.to_string()
        } else {
            format!("starfane: {line}")
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
                    "starfane: 你有 {pending_count} 条未读任务/消息，请调用 starfane 的 read_inbox 工具获取"
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
        starfane_adapters::ensure_bypass_permission_arg(plan.agent, &mut extra);
        let argv = starfane_adapters::build_l4_spawn_cmd(&starfane_adapters::SpawnRequest {
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
            name: format!("starfane:{}", plan.name),
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
            core.inner.tmux.spawn_window(&starfane_tmux::SpawnWindow {
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
        let dir = std::env::temp_dir().join(format!("starfane-paste-test-{}", id::new_id("T")));
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
                token: None,
            }],
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);

        // 3. Moderate text (15 lines / 1200 chars) passes inline verbatim without spawning paste-*.md.
        let inline_quote = "a".repeat(1200);
        let (out_mod, p_mod) = prepare_dispatch_payload(&dir, &inline_quote, &[]).unwrap();
        assert_eq!(out_mod, inline_quote);
        assert!(p_mod.is_none());

        let moderate_lines = (1..=15)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (out_ml, p_ml) = prepare_dispatch_payload(&dir, &moderate_lines, &[]).unwrap();
        assert_eq!(out_ml, moderate_lines);
        assert!(p_ml.is_none());

        // 4. Text > 2400 chars is saved to <state_dir>/inputs/paste-*.md.
        let big = "a".repeat(2405);
        let (ptr_big, p_big) = prepare_dispatch_payload(&dir, &big, &[]).unwrap();
        let p_big = p_big.expect("saved file for >2400 chars");
        assert!(p_big.starts_with(dir.join("inputs")));
        assert!(!ptr_big.contains('\n'));
        assert!(ptr_big.contains(&p_big.display().to_string()));
        let content_big = std::fs::read_to_string(&p_big).unwrap();
        assert!(content_big.contains(&big));

        // 5. Text > 40 lines is saved to file.
        let many_lines = (1..=45)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (ptr_lines, p_lines) = prepare_dispatch_payload(&dir, &many_lines, &[]).unwrap();
        let p_lines = p_lines.expect("saved file for >40 lines");
        assert!(!ptr_lines.contains('\n'));
        assert!(ptr_lines.contains("共 45 行"));
        assert!(std::fs::read_to_string(&p_lines)
            .unwrap()
            .contains("line-45"));

        // 6. Legacy short instruction + unanchored attachment combines prefix + file path pointer.
        let att = vec![PasteAttachment {
            kind: Some("text".into()),
            title: Some("stacktrace".into()),
            content: "err1\nerr2\nerr3".into(),
            path: None,
            token: None,
        }];
        let (ptr_att, p_att) = prepare_dispatch_payload(&dir, "帮我定位这个报错", &att).unwrap();
        let p_att = p_att.expect("saved file for attachment");
        assert!(!ptr_att.contains('\n'));
        assert!(ptr_att.starts_with("帮我定位这个报错（附粘贴内容已保存至 "));
        assert!(ptr_att.contains("共 3 行"));
        let md = std::fs::read_to_string(&p_att).unwrap();
        assert!(md.contains("## stacktrace"));
        assert!(md.contains("err1\nerr2\nerr3"));

        // 7. Attachment-only (empty text) succeeds cleanly.
        let (ptr_only, p_only) = prepare_dispatch_payload(&dir, "", &att).unwrap();
        let p_only = p_only.expect("saved file for attachment-only");
        assert!(ptr_only.starts_with("请阅读并处理以下粘贴内容："));
        assert!(ptr_only.contains(&p_only.display().to_string()));

        // 8. Image attachment inside inputs/ works both image-only and with instruction;
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
            token: None,
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
            token: None,
        }];
        assert!(prepare_dispatch_payload(&dir, "look", &bad_img).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_dispatch_payload_preserves_inline_anchor_order() {
        let dir = std::env::temp_dir().join(format!("starfane-anchor-test-{}", id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();

        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let up = super::super::uploads::save_uploaded_image(
            &dir,
            Some("shot.png"),
            Some("image/png"),
            png_b64,
        )
        .unwrap();
        let canon_img = std::fs::canonicalize(&up.path).unwrap();
        let canon_img_str = canon_img.display().to_string();

        // 1. Interleaved quote A + follow-up A + screenshot + quote B + follow-up B (short enough for inline).
        let raw_inline = "先看第一段回复：[粘贴 #1]\n我对上面第一段的追问\n对照截图：[图片 #1]\n再看第二段：[粘贴 #2]\n针对第二段的修改要求";
        let atts_inline = vec![
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("粘贴片段 #1".into()),
                content: "Agent 回复第一段行1\nAgent 回复第一段行2".into(),
                path: None,
                token: Some("[粘贴 #1]".into()),
            },
            PasteAttachment {
                kind: Some("image".into()),
                title: Some("shot.png".into()),
                content: String::new(),
                path: Some(up.path.clone()),
                token: Some("[图片 #1]".into()),
            },
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("粘贴片段 #2".into()),
                content: "Agent 回复第二段".into(),
                path: None,
                token: Some("[粘贴 #2]".into()),
            },
        ];
        let (out_inline, path_inline) =
            prepare_dispatch_payload(&dir, raw_inline, &atts_inline).unwrap();
        assert_eq!(
            path_inline.as_deref(),
            Some(canon_img.as_path()),
            "inline anchored payload within bounds should return image path without spilling paste-*.md: {path_inline:?}"
        );
        let has_md = std::fs::read_dir(dir.join("inputs"))
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"));
        assert!(
            !has_md,
            "inline anchored payload should not create paste-*.md"
        );
        let p_q1 = out_inline.find("Agent 回复第一段行1").unwrap();
        let p_f1 = out_inline.find("我对上面第一段的追问").unwrap();
        let p_img = out_inline
            .find(&format!("[图片: {canon_img_str}]"))
            .unwrap();
        let p_q2 = out_inline.find("Agent 回复第二段").unwrap();
        let p_f2 = out_inline.find("针对第二段的修改要求").unwrap();
        assert!(
            p_q1 < p_f1 && p_f1 < p_img && p_img < p_q2 && p_q2 < p_f2,
            "order destroyed in inline output:\n{out_inline}"
        );

        // 2. Same interleaved layout exceeding LONG_TEXT_LINE_THRESHOLD writes ONE ordered paste-*.md.
        let long_q1 = (1..=42)
            .map(|i| format!("long-quote-line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let atts_long = vec![
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("粘贴片段 #1".into()),
                content: long_q1,
                path: None,
                token: Some("[粘贴 #1]".into()),
            },
            PasteAttachment {
                kind: Some("image".into()),
                title: Some("shot.png".into()),
                content: String::new(),
                path: Some(up.path.clone()),
                token: Some("[图片 #1]".into()),
            },
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("粘贴片段 #2".into()),
                content: "第二段结论\n还有第二行".into(),
                path: None,
                token: Some("[粘贴 #2]".into()),
            },
        ];
        let (ptr_long, p_long) = prepare_dispatch_payload(&dir, raw_inline, &atts_long).unwrap();
        let p_long = p_long.expect("long anchored payload spills to ordered markdown");
        assert!(!ptr_long.contains('\n'), "{ptr_long}");
        assert!(
            ptr_long.contains(&p_long.display().to_string()),
            "{ptr_long}"
        );
        assert!(ptr_long.contains(&canon_img_str), "{ptr_long}");
        let saved_md = std::fs::read_to_string(&p_long).unwrap();
        assert!(!saved_md.contains("## 粘贴文本"));
        assert!(!saved_md.contains("## 粘贴内容"));
        let mp_q1 = saved_md.find("long-quote-line-1").unwrap();
        let mp_f1 = saved_md.find("我对上面第一段的追问").unwrap();
        let mp_img = saved_md.find(&format!("[图片: {canon_img_str}]")).unwrap();
        let mp_q2 = saved_md.find("第二段结论").unwrap();
        let mp_f2 = saved_md.find("针对第二段的修改要求").unwrap();
        assert!(
            mp_q1 < mp_f1 && mp_f1 < mp_img && mp_img < mp_q2 && mp_q2 < mp_f2,
            "order destroyed in spilled markdown:\n{saved_md}"
        );

        // 3. Non-recursive expansion: literal `[图片 #1]` / `[粘贴 #2]` inside `[粘贴 #1]` is untouched.
        let atts_nonrec = vec![
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("粘贴片段 #1".into()),
                content: "原文内自带字面量 [图片 #1] 与 [粘贴 #2]".into(),
                path: None,
                token: Some("[粘贴 #1]".into()),
            },
            atts_inline[1].clone(),
            atts_inline[2].clone(),
        ];
        let (out_nonrec, _) = prepare_dispatch_payload(
            &dir,
            "引用：[粘贴 #1]\n图：[图片 #1]\n二：[粘贴 #2]",
            &atts_nonrec,
        )
        .unwrap();
        assert!(
            out_nonrec.contains("原文内自带字面量 [图片 #1] 与 [粘贴 #2]"),
            "{out_nonrec}"
        );

        // 4. Image-only token produces clean Chinese image pointer.
        let (ptr_tok_img, _) =
            prepare_dispatch_payload(&dir, "[图片 #1]", &[atts_inline[1].clone()]).unwrap();
        assert_eq!(ptr_tok_img, format!("请查看这张图片：{canon_img_str}"));

        // 5. Manually deleted token on a remaining pill is appended instead of silently dropped.
        let atts_mixed = vec![
            atts_inline[1].clone(),
            PasteAttachment {
                kind: Some("text".into()),
                title: Some("补充附件".into()),
                content: "未被占位符引用的补充片段".into(),
                path: None,
                token: Some("[粘贴 #9]".into()),
            },
        ];
        let (out_mixed, _) = prepare_dispatch_payload(&dir, "看图 [图片 #1]", &atts_mixed).unwrap();
        assert!(out_mixed.contains(&format!("[图片: {canon_img_str}]")));
        assert!(out_mixed.contains("未被占位符引用的补充片段"));

        // 6. Disallowed control characters (ESC) spill safely to paste-*.md.
        let atts_esc = vec![PasteAttachment {
            kind: Some("text".into()),
            title: None,
            content: "contains\x1besc".into(),
            path: None,
            token: Some("[粘贴 #1]".into()),
        }];
        let (ptr_esc, p_esc) =
            prepare_dispatch_payload(&dir, "check [粘贴 #1]", &atts_esc).unwrap();
        assert!(p_esc.is_some());
        assert!(!ptr_esc.chars().any(char::is_control));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
