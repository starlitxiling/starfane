//! Notification router: tmux status mirroring, focus-aware suppression,
//! escalation and away digests (architecture §8).

use super::transcript::{load_session_transcript, strip_codex_internal_tags, TranscriptRole};
use super::{lock, util, Core};
use crate::notice::{Notice, NoticeKind, SessionReplyTarget};
use starfane_proto::*;
use std::time::Duration;

/// Soft character budget for the last assistant turn embedded in a Feishu session card.
const MAX_FEISHU_OUTPUT_CHARS: usize = 1800;
/// Cross-session content fingerprint deduplication window (30 seconds) to guarantee that
/// even if two events fire for the same turn, Feishu receives at most one card.
const NOTICE_CONTENT_DEDUP_MS: u64 = 30_000;

/// Compute cross-session content dedup key: `(agent, normalized_cwd, pane_or_empty, kind, normalized_prefix_120)`.
pub(crate) fn notice_content_fingerprint(
    agent: AgentKind,
    cwd: Option<&str>,
    pane: Option<&PaneId>,
    kind: NoticeKind,
    body_md: &str,
) -> (AgentKind, String, String, NoticeKind, String) {
    let norm_cwd = cwd.unwrap_or("").trim().trim_end_matches('/').to_string();
    let norm_pane = pane.map(|p| p.0.clone()).unwrap_or_default();
    // Compare the semantic output portion when present so differing window/pane headers
    // (e.g. split `tmux-%16` vs real UUID session) produce the exact same fingerprint.
    let semantic = body_md
        .split_once("\n**最后一轮输出**：\n")
        .map(|(_, tail)| tail)
        .unwrap_or(body_md);
    let cleaned = strip_codex_internal_tags(semantic);
    let norm_prefix: String = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect();
    (agent, norm_cwd, norm_pane, kind, norm_prefix)
}

impl Core {
    /// Called for every applied hook event.
    pub(crate) async fn on_session_event(
        &self,
        session: &SessionRecord,
        ev: &NormalizedEvent,
        stage: Option<StageRef>,
        is_sub: bool,
    ) {
        if !is_sub {
            self.mirror_tmux_status(session);
        }
        // Pipeline bookkeeping (blocked / resumed / idle pokes).
        if let Some(stage) = &stage {
            if !is_sub || ev.kind == EventKind::NeedsPermission {
                self.on_stage_session_event(stage, session, ev.kind).await;
            }
        }
        if !is_sub && matches!(ev.kind, EventKind::TurnComplete | EventKind::Interrupted) {
            self.poke_pending_inbox(&session.session_id).await;
        }

        let notice_kind = match ev.kind {
            EventKind::NeedsPermission | EventKind::NeedsInput => NoticeKind::Blocked,
            EventKind::Error => NoticeKind::Error,
            EventKind::TurnComplete => NoticeKind::TurnComplete,
            _ => return,
        };
        // Heuristic tail-hash transitions (`tmux_idle_heuristic` on `discovered_only` panes) come
        // from terminal redraws, CLI auto-updates, or window resizes — never push Feishu completion cards.
        if notice_kind == NoticeKind::TurnComplete
            && (ev.confidence == Confidence::Heuristic
                || session.discovered_only
                || ev.native_event == "tmux_idle_heuristic")
        {
            return;
        }
        // Skip stale wall-clock events replayed from offline spool (> 5 minutes old).
        if ev.ts_ms >= 1_700_000_000_000 && id::now_ms().saturating_sub(ev.ts_ms) > 300_000 {
            log::debug!("feishu: skipping stale event {} (age > 5m)", ev.id);
            return;
        }
        let agy_folded_error =
            is_sub && session.agent == AgentKind::Agy && ev.kind == EventKind::Error;
        if is_sub && !(agy_folded_error || ev.kind == EventKind::NeedsPermission) {
            return; // subagent noise stays in the Web timeline only
        }
        // Pipeline sessions report milestones through the engine instead.
        if notice_kind == NoticeKind::TurnComplete && stage.is_some() {
            return;
        }
        if self.in_cooldown(&session.session_id, ev.kind) {
            return;
        }
        let last_output = self.resolve_last_output_for_notice(session, ev).await;
        let notice = build_notice_with_output(notice_kind, session, ev, last_output.as_deref());
        match notice_kind {
            NoticeKind::TurnComplete => {
                let delay = self.inner.cfg.notify.away_digest_after_secs;
                self.enqueue_away_turn_complete(session.clone(), notice, delay);
            }
            _ => {
                if self.user_watching(session).await {
                    let delay = self.inner.cfg.notify.escalate_after_secs;
                    self.schedule_if_unchanged(session.clone(), notice, delay);
                } else {
                    self.mark_notified(&session.session_id, ev.kind);
                    if self.claim_notice_content_slot(session, &notice, id::now_ms()) {
                        self.notify(notice);
                    }
                }
            }
        }
    }

    fn in_cooldown(&self, session: &SessionId, kind: EventKind) -> bool {
        let cooldown_ms = self.inner.cfg.notify.cooldown_secs * 1000;
        let st = lock(&self.inner.state);
        st.cooldown
            .get(&(session.clone(), kind))
            .is_some_and(|t| id::now_ms().saturating_sub(*t) < cooldown_ms)
    }

    fn mark_notified(&self, session: &SessionId, kind: EventKind) {
        lock(&self.inner.state)
            .cooldown
            .insert((session.clone(), kind), id::now_ms());
    }

    /// Return `true` and record the content fingerprint if no identical notice was sent
    /// within the last 30 seconds across any session for the same `(agent, cwd, kind)`.
    pub(crate) fn claim_notice_content_slot(
        &self,
        session: &SessionRecord,
        notice: &Notice,
        now_ms: u64,
    ) -> bool {
        let key = notice_content_fingerprint(
            session.agent,
            session.cwd.as_deref().or(session.git_root.as_deref()),
            session.pane.as_ref(),
            notice.kind,
            &notice.body_md,
        );
        let mut st = lock(&self.inner.state);
        if st.notice_fingerprints.len() > 64 {
            st.notice_fingerprints
                .retain(|_, ts| now_ms.saturating_sub(*ts) < NOTICE_CONTENT_DEDUP_MS);
        }
        // Suppress when same pane repeats OR either side lacked a pane (split tmux-%N vs hook);
        // keep two distinct live panes (e.g. %16 vs %17) in the same cwd independent.
        let duplicate = st.notice_fingerprints.iter().any(|(k, &prev)| {
            k.0 == key.0
                && k.1 == key.1
                && k.3 == key.3
                && k.4 == key.4
                && (k.2 == key.2 || k.2.is_empty() || key.2.is_empty())
                && now_ms.saturating_sub(prev) < NOTICE_CONTENT_DEDUP_MS
        });
        if duplicate {
            log::debug!(
                "feishu: suppressed duplicate notice {} within 30s content window",
                notice.key
            );
            return false;
        }
        st.notice_fingerprints.insert(key, now_ms);
        true
    }

    /// Enqueue a `TurnComplete` notice into the coalesced away-digest buffer and arm a single
    /// shared timer so multi-session completions collapse into at most one digest card.
    pub(crate) fn enqueue_away_turn_complete(
        &self,
        session: SessionRecord,
        notice: Notice,
        delay_secs: u64,
    ) {
        let should_spawn = {
            let mut st = lock(&self.inner.state);
            st.pending_away_notices.insert(
                session.session_id.clone(),
                super::state::PendingAwayItem { session, notice },
            );
            if st.away_digest_scheduled {
                false
            } else {
                st.away_digest_scheduled = true;
                true
            }
        };
        if should_spawn {
            let core = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                core.flush_away_digest().await;
            });
        }
    }

    /// Flush all queued away `TurnComplete` notices: drop watched or superseded ones, deliver
    /// a single notice directly when `ready.len() == 1`, or coalesce into one digest card when `>= 2`.
    pub(crate) async fn flush_away_digest(&self) {
        let pending = {
            let mut st = lock(&self.inner.state);
            st.away_digest_scheduled = false;
            std::mem::take(&mut st.pending_away_notices)
        };
        if pending.is_empty() {
            return;
        }
        let mut ready: Vec<(SessionRecord, Notice)> = Vec::new();
        for (sid, item) in pending {
            let current = lock(&self.inner.state).sessions.get(&sid).cloned();
            let Some(cur) = current else { continue };
            if cur.state != SessionState::Idle || cur.state_since_ms != item.session.state_since_ms
            {
                continue;
            }
            if cur.discovered_only {
                continue;
            }
            if self.user_watching(&cur).await {
                continue;
            }
            if self.in_cooldown(&cur.session_id, EventKind::TurnComplete) {
                continue;
            }
            if !self.claim_notice_content_slot(&cur, &item.notice, id::now_ms()) {
                continue;
            }
            ready.push((cur, item.notice));
        }
        if ready.is_empty() {
            return;
        }
        for (cur, _) in &ready {
            self.mark_notified(&cur.session_id, EventKind::TurnComplete);
        }
        if ready.len() == 1 {
            let (_, single) = ready.into_iter().next().expect("checked len == 1");
            self.notify(single);
        } else {
            let digest = build_away_digest_notice(&ready, id::now_ms());
            self.notify(digest);
        }
    }

    /// After `delay_secs`, send if the session is still in the same state and
    /// the user is (still) not looking at the pane.
    fn schedule_if_unchanged(&self, session: SessionRecord, notice: Notice, delay_secs: u64) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            let current = lock(&core.inner.state)
                .sessions
                .get(&session.session_id)
                .cloned();
            let Some(cur) = current else { return };
            if cur.state != session.state || cur.state_since_ms != session.state_since_ms {
                return;
            }
            if core.user_watching(&cur).await {
                return;
            }
            let kind = match notice.kind {
                NoticeKind::Blocked => EventKind::NeedsPermission,
                NoticeKind::Error => EventKind::Error,
                _ => EventKind::TurnComplete,
            };
            if core.in_cooldown(&cur.session_id, kind) {
                return;
            }
            core.mark_notified(&cur.session_id, kind);
            if core.claim_notice_content_slot(&cur, &notice, id::now_ms()) {
                core.notify(notice);
            }
        });
    }

    /// Pane visible + recent client activity (focus-events-off safe).
    pub(crate) async fn user_watching(&self, session: &SessionRecord) -> bool {
        if !session.is_local_host() {
            return false;
        }
        let Some(pane) = session.pane.clone() else {
            return false;
        };
        let core = self.clone();
        let window = self.inner.cfg.notify.watch_window_secs;
        tokio::task::spawn_blocking(move || {
            let panes = core.inner.mux.list_all_panes().ok()?;
            let clients = core.inner.mux.list_all_clients().ok()?;
            let info = panes.into_iter().find(|p| p.pane_id == pane)?;
            Some(starfane_tmux::is_user_watching(
                &info,
                &clients,
                id::now_ms() / 1000,
                window,
            ))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
    }

    pub(crate) fn mirror_tmux_status(&self, session: &SessionRecord) {
        if !session.is_local_host() {
            return;
        }
        let Some(pane) = session.pane.clone() else {
            return;
        };
        let status = starfane_tmux::PaneStatus {
            state: session.state,
            role: session.stage.as_ref().map(|s| s.stage),
            pipeline: session.stage.as_ref().map(|s| s.pipeline_id.clone()),
        };
        {
            let mut st = lock(&self.inner.state);
            if !claim_tmux_mirror_slot(&mut st, &pane, &status) {
                return;
            }
        }
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = core.inner.mux.set_status(&pane, &status) {
                log::debug!("mux set_status {pane}: {e}");
            }
        });
    }
}

pub(crate) fn claim_tmux_mirror_slot(
    st: &mut super::state::State,
    pane: &PaneId,
    status: &starfane_tmux::PaneStatus,
) -> bool {
    let key_val = (status.state, status.role, status.pipeline.clone());
    if st.last_mirrored_tmux.get(pane) == Some(&key_val) {
        return false;
    }
    st.last_mirrored_tmux.insert(pane.clone(), key_val);
    true
}

impl Core {
    /// Resolve the cleanest last assistant output for a Feishu notification card:
    /// 1. Structured transcript's latest `assistant` message
    /// 2. Hook event `last_message` / `session.last_message`
    /// 3. Cleaned `capture-pane` tail fallback
    async fn resolve_last_output_for_notice(
        &self,
        session: &SessionRecord,
        ev: &NormalizedEvent,
    ) -> Option<String> {
        let sess_clone = session.clone();
        let ev_ts = ev.ts_ms;
        let from_tr = tokio::task::spawn_blocking(move || {
            let tr = load_session_transcript(&sess_clone, 6);
            tr.messages
                .into_iter()
                .rev()
                .find(|m| {
                    m.role == TranscriptRole::Assistant
                        && !m.text.trim().is_empty()
                        && m.ts_ms.is_none_or(|msg_ts| {
                            ev_ts < 1_700_000_000_000 || ev_ts.saturating_sub(msg_ts) <= 1_800_000
                        })
                })
                .map(|m| m.text)
        })
        .await
        .ok()
        .flatten();
        if let Some(txt) = from_tr {
            return Some(txt);
        }
        if let Some(m) = ev
            .detail
            .last_message
            .as_deref()
            .or(session.last_message.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(m.to_string());
        }
        let pane = session.pane.clone()?;
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            let cap = core
                .inner
                .mux
                .capture_pane(&pane, &starfane_tmux::CaptureRequest::tail(40))
                .ok()?;
            clean_terminal_tail_for_card(&cap.text)
        })
        .await
        .ok()
        .flatten()
    }
}

fn clean_terminal_tail_for_card(raw: &str) -> Option<String> {
    let mut kept = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.chars().all(|c| "─━═╭╮╰╯┌┐└┘│─ ".contains(c)) {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.contains("? for shortcuts")
            || lower.contains("shift+tab")
            || lower.contains("context left")
            || lower.contains("ask codex to do anything")
            || t == ">"
            || t == "❯"
            || t == "❯\u{a0}"
        {
            continue;
        }
        kept.push(t);
    }
    if kept.is_empty() {
        return None;
    }
    let start = kept.len().saturating_sub(18);
    Some(kept[start..].join("\n"))
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max_chars).collect();
    format!(
        "{}\n…（输出较长已截断，完整内容见 Web 控制台）",
        cut.trim_end()
    )
}

fn short_dir_label(path: &str) -> Option<String> {
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    if let Ok(home) = std::env::var("HOME") {
        if p == home || p == home.trim_end_matches('/') {
            return Some("~".to_string());
        }
    }
    let name = util::project_name(p);
    if name.is_empty() || name == "?" {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
pub(crate) fn build_notice(kind: NoticeKind, s: &SessionRecord, ev: &NormalizedEvent) -> Notice {
    let fallback = ev
        .detail
        .last_message
        .as_deref()
        .or(s.last_message.as_deref());
    build_notice_with_output(kind, s, ev, fallback)
}

pub(crate) fn build_notice_with_output(
    kind: NoticeKind,
    s: &SessionRecord,
    ev: &NormalizedEvent,
    last_output: Option<&str>,
) -> Notice {
    let pane_str = s.pane.as_ref().map(|p| p.as_str());
    let label = s
        .tmux_label
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let target = s
        .tmux_target
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let place = match (label, pane_str) {
        (Some(lbl), Some(p)) => format!("{} · {lbl} ({p})", s.agent),
        (Some(lbl), None) => format!("{} · {lbl}", s.agent),
        (None, Some(p)) => match target {
            Some(t) => format!("{} · {t} ({p})", s.agent),
            None => format!("{}@{p}", s.agent),
        },
        (None, None) => format!("{} · {}", s.agent, util::short_id(s.session_id.as_str())),
    };

    let dir_hint = s
        .git_root
        .as_deref()
        .and_then(short_dir_label)
        .or_else(|| s.cwd.as_deref().and_then(short_dir_label));
    let window_field = match (label, dir_hint) {
        (Some(lbl), Some(dir)) => {
            let win_part = lbl.rsplit('-').next().unwrap_or(lbl);
            if dir.eq_ignore_ascii_case(win_part) || dir.eq_ignore_ascii_case(lbl) {
                lbl.to_string()
            } else {
                format!("{lbl}（{dir}）")
            }
        }
        (Some(lbl), None) => lbl.to_string(),
        (None, Some(dir)) => dir,
        (None, None) => target
            .or(pane_str)
            .map(str::to_string)
            .unwrap_or_else(|| util::short_id(s.session_id.as_str())),
    };

    let (title, verb) = match (kind, ev.kind) {
        (NoticeKind::Blocked, EventKind::NeedsPermission) => {
            (format!("⏸ {place} 需要审批"), "等待审批")
        }
        (NoticeKind::Blocked, _) => (format!("⌨ {place} 等待输入"), "等待输入"),
        (NoticeKind::Error, _) => (format!("❌ {place} 出错"), "出错"),
        _ => (format!("✅ {place} 本轮完成"), "回合结束"),
    };
    let mut body = format!("**窗口**：{window_field}\n**状态**：{verb}\n");
    if let Some(t) = &ev.detail.tool_name {
        body.push_str(&format!("**工具**：{t}"));
        if let Some(sum) = &ev.detail.tool_summary {
            body.push_str(&format!(" `{}`", util::sanitize_line(sum, 160)));
        }
        body.push('\n');
    }
    if let Some(e) = ev
        .detail
        .error
        .as_ref()
        .or(ev.detail.termination_reason.as_ref())
    {
        body.push_str(&format!("**原因**：{}\n", util::sanitize_line(e, 200)));
    }
    if let Some(m) = last_output
        .map(strip_codex_internal_tags)
        .filter(|m| !m.is_empty())
    {
        body.push_str("\n**最后一轮输出**：\n");
        body.push_str(&truncate_chars(&m, MAX_FEISHU_OUTPUT_CHARS));
        body.push('\n');
    }
    if s.is_subagent {
        body.push_str("_来自子会话_\n");
    }
    Notice {
        key: format!(
            "{}-{:?}-{}",
            util::short_id(s.session_id.as_str()),
            kind,
            ev.ts_ms / 1000
        ),
        kind,
        title,
        body_md: body,
        pipeline_id: s.stage.as_ref().map(|r| r.pipeline_id.clone()),
        session_id: Some(s.session_id.clone()),
        review_id: None,
        actions: Vec::new(),
        binding_form: Vec::new(),
        reply_session: Some(SessionReplyTarget {
            session_id: s.session_id.clone(),
            pane_id: s.pane.clone(),
            tmux_label: s.tmux_label.clone().or_else(|| s.tmux_target.clone()),
            agent: s.agent,
        }),
    }
}

/// Consolidate `items` (`>= 2` idle unattended sessions) into a single summary Feishu card.
pub(crate) fn build_away_digest_notice(items: &[(SessionRecord, Notice)], now_ms: u64) -> Notice {
    const MAX_DIGEST_LINES: usize = 10;
    let total = items.len();
    let mut body = String::new();
    for (s, n) in items.iter().take(MAX_DIGEST_LINES) {
        let pane_str = s.pane.as_ref().map(|p| p.as_str());
        let label = s
            .tmux_label
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .or_else(|| {
                s.tmux_target
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
            });
        let place = match (label, pane_str) {
            (Some(lbl), Some(p)) => format!("{} · {lbl} ({p})", s.agent),
            (Some(lbl), None) => format!("{} · {lbl}", s.agent),
            (None, Some(p)) => format!("{}@{p}", s.agent),
            (None, None) => format!("{} · {}", s.agent, util::short_id(s.session_id.as_str())),
        };
        let snippet = n
            .body_md
            .split_once("\n**最后一轮输出**：\n")
            .map(|(_, tail)| util::sanitize_line(tail, 90))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "本轮已完成".to_string());
        body.push_str(&format!("- **{place}**：{snippet}\n"));
    }
    if total > MAX_DIGEST_LINES {
        body.push_str(&format!(
            "\n_…另有 {} 个会话已完成，请打开 Web 控制台查看_\n",
            total - MAX_DIGEST_LINES
        ));
    }
    Notice {
        key: format!("away-digest-{total}-{}", now_ms / 1000),
        kind: NoticeKind::TurnComplete,
        title: format!("📋 {total} 个会话已完成（离开摘要）"),
        body_md: body,
        pipeline_id: None,
        session_id: None,
        review_id: None,
        actions: Vec::new(),
        binding_form: Vec::new(),
        reply_session: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DaemonConfig, StarfaneConfig};

    fn mk_session(id: &str, pane: Option<&str>, label: Option<&str>, cwd: &str) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(id),
            agent: AgentKind::Codex,
            pane: pane.map(PaneId::new),
            pane_resolution: PaneResolution::Env,
            cwd: Some(cwd.into()),
            git_root: Some(cwd.into()),
            model: Some("gpt-6.2-sol".into()),
            effort: Some("high".into()),
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
            tmux_target: pane.map(|_| "prome:0.0".into()),
            tmux_label: label.map(str::to_string),
            discovered_only: id.starts_with("tmux-"),
            transcript_path: None,
            metrics: None,
            host_id: None,
            host_display_name: None,
            mux_kind: None,
        }
    }

    #[test]
    fn cross_session_30s_content_dedup_allows_only_one_card_and_strips_oai_mem_citation() {
        let dir = std::env::temp_dir().join(format!("starfane-router-dedup-{}", id::new_id("T")));
        let core = Core::new(&DaemonConfig {
            socket: dir.join("starfane.sock"),
            state_dir: dir.join("state"),
            web_dir: None,
            config: StarfaneConfig::default(),
            foreground: true,
        })
        .unwrap();

        let s_hook = mk_session(
            "01a18191-3525-7790-b3d3-36033faa913c",
            None,
            None,
            "/tmp/proj",
        );
        let s_tmux = mk_session("tmux-%16", Some("%16"), Some("prome-ros"), "/tmp/proj/");
        let ev = NormalizedEvent {
            v: PROTO_VERSION,
            id: EventId::new("E-1"),
            ts_ms: 10_000,
            agent: AgentKind::Codex,
            kind: EventKind::TurnComplete,
            confidence: Confidence::Exact,
            session_id: s_hook.session_id.clone(),
            subagent_id: None,
            pane: None,
            cwd: Some("/tmp/proj".into()),
            model: None,
            native_event: "Stop".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail::default(),
        };

        let raw_with_citation =
            "节点启动成功，无报错。\n\n<oai-mem-citation>\n<citation_entries>MEM</citation_entries>\n</oai-mem-citation>\n";
        let n1 = build_notice_with_output(
            NoticeKind::TurnComplete,
            &s_hook,
            &ev,
            Some(raw_with_citation),
        );
        let n2 = build_notice_with_output(
            NoticeKind::TurnComplete,
            &s_tmux,
            &ev,
            Some("节点启动成功，无报错。"),
        );
        assert!(!n1.body_md.contains("oai-mem-citation"), "{}", n1.body_md);

        // First card allowed; second card from split session within 30s suppressed!
        assert!(core.claim_notice_content_slot(&s_hook, &n1, 100_000));
        assert!(!core.claim_notice_content_slot(&s_tmux, &n2, 115_000));
        // After 30s window passes, next turn is allowed again.
        assert!(core.claim_notice_content_slot(&s_tmux, &n2, 131_000));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmux_mirror_status_deduplicates_unchanged_tuples() {
        let mut st = super::super::state::State::default();
        let pane = PaneId::new("%16");
        let idle = starfane_tmux::PaneStatus {
            state: SessionState::Idle,
            role: None,
            pipeline: None,
        };
        let working = starfane_tmux::PaneStatus {
            state: SessionState::Working,
            role: None,
            pipeline: None,
        };
        let working_stage = starfane_tmux::PaneStatus {
            state: SessionState::Working,
            role: Some(StageKind::Implement),
            pipeline: Some(PipelineId::new("P-1")),
        };

        assert!(claim_tmux_mirror_slot(&mut st, &pane, &idle));
        assert!(!claim_tmux_mirror_slot(&mut st, &pane, &idle));
        assert!(claim_tmux_mirror_slot(&mut st, &pane, &working));
        assert!(!claim_tmux_mirror_slot(&mut st, &pane, &working));
        assert!(claim_tmux_mirror_slot(&mut st, &pane, &working_stage));
        assert!(!claim_tmux_mirror_slot(&mut st, &pane, &working_stage));
        assert!(claim_tmux_mirror_slot(&mut st, &pane, &idle));
    }

    #[tokio::test]
    async fn heuristic_turn_complete_is_suppressed_from_feishu_away_queue() {
        let dir = std::env::temp_dir().join(format!("starfane-router-heur-{}", id::new_id("T")));
        let mut cfg = StarfaneConfig::default();
        cfg.feishu.enabled = true;
        cfg.notify.away_digest_after_secs = 600;
        let core = Core::new(&DaemonConfig {
            socket: dir.join("starfane.sock"),
            state_dir: dir.join("state"),
            web_dir: None,
            config: cfg,
            foreground: true,
        })
        .unwrap();

        let s_legacy = mk_session("tmux-%9991", Some("%9991"), Some("prome-a"), "/tmp/a");
        let s_real = mk_session("real-uuid-1", None, Some("prome-b"), "/tmp/b");
        {
            let mut st = super::super::lock(&core.inner.state);
            st.sessions
                .insert(s_legacy.session_id.clone(), s_legacy.clone());
            st.sessions
                .insert(s_real.session_id.clone(), s_real.clone());
        }

        let ev_heur = NormalizedEvent {
            v: PROTO_VERSION,
            id: EventId::new("E-H"),
            ts_ms: 1_000,
            agent: AgentKind::Codex,
            kind: EventKind::TurnComplete,
            confidence: Confidence::Heuristic,
            session_id: s_legacy.session_id.clone(),
            subagent_id: None,
            pane: Some(PaneId::new("%9991")),
            cwd: Some("/tmp/a".into()),
            model: None,
            native_event: "tmux_idle_heuristic".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail::default(),
        };
        core.on_session_event(&s_legacy, &ev_heur, None, false)
            .await;
        assert!(super::super::lock(&core.inner.state)
            .pending_away_notices
            .is_empty());

        let mut ev_exact = ev_heur.clone();
        ev_exact.id = EventId::new("E-R");
        ev_exact.confidence = Confidence::Exact;
        ev_exact.session_id = s_real.session_id.clone();
        ev_exact.pane = None;
        ev_exact.native_event = "Stop".into();
        core.on_session_event(&s_real, &ev_exact, None, false).await;
        assert_eq!(
            super::super::lock(&core.inner.state)
                .pending_away_notices
                .len(),
            1
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn away_digest_coalesces_multi_session_burst_into_single_card() {
        let dir = std::env::temp_dir().join(format!("starfane-router-digest-{}", id::new_id("T")));
        let mut cfg = StarfaneConfig::default();
        cfg.feishu.enabled = true;
        cfg.notify.away_digest_after_secs = 600;
        let core = Core::new(&DaemonConfig {
            socket: dir.join("starfane.sock"),
            state_dir: dir.join("state"),
            web_dir: None,
            config: cfg,
            foreground: true,
        })
        .unwrap();
        let mut rx = core.take_notice_rx().unwrap();

        let make_ev = |sid: &SessionId| NormalizedEvent {
            v: PROTO_VERSION,
            id: EventId::new(id::new_id("E")),
            ts_ms: 10_000,
            agent: AgentKind::Codex,
            kind: EventKind::TurnComplete,
            confidence: Confidence::Exact,
            session_id: sid.clone(),
            subagent_id: None,
            pane: None,
            cwd: Some("/tmp/proj".into()),
            model: None,
            native_event: "Stop".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail::default(),
        };

        // Case 1: Single idle session -> emits 1 individual card keeping `reply_session`.
        let s1 = mk_session("real-1", None, Some("win-1"), "/tmp/p1");
        super::super::lock(&core.inner.state)
            .sessions
            .insert(s1.session_id.clone(), s1.clone());
        let n1 = build_notice_with_output(
            NoticeKind::TurnComplete,
            &s1,
            &make_ev(&s1.session_id),
            Some("single output"),
        );
        core.enqueue_away_turn_complete(s1.clone(), n1, 600);
        core.flush_away_digest().await;
        let single = rx.try_recv().expect("expected single notice");
        assert!(single.reply_session.is_some());
        assert!(rx.try_recv().is_err());

        // Case 2: 5 idle sessions complete together -> coalesced into 1 digest card.
        for i in 2..=6 {
            let sid = SessionId::new(format!("real-{i}"));
            let s = mk_session(
                sid.as_str(),
                None,
                Some(&format!("win-{i}")),
                &format!("/tmp/p{i}"),
            );
            super::super::lock(&core.inner.state)
                .sessions
                .insert(sid.clone(), s.clone());
            let n = build_notice_with_output(
                NoticeKind::TurnComplete,
                &s,
                &make_ev(&sid),
                Some(&format!("output {i}")),
            );
            core.enqueue_away_turn_complete(s, n, 600);
        }
        core.flush_away_digest().await;
        let digest = rx.try_recv().expect("expected coalesced digest card");
        assert!(digest.title.contains("5 个会话已完成"), "{}", digest.title);
        assert!(digest.reply_session.is_none());
        assert!(digest.body_md.contains("win-2"));
        assert!(digest.body_md.contains("win-6"));
        assert!(rx.try_recv().is_err(), "must emit only 1 card for burst");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_feishu_rate_limit_caps_at_six_per_minute_and_recovers() {
        use super::super::{check_feishu_rate_limit, FeishuRateDecision};
        let mut st = super::super::state::State::default();
        for _ in 0..6 {
            assert_eq!(
                check_feishu_rate_limit(&mut st, 1_000),
                FeishuRateDecision::Allow
            );
        }
        assert_eq!(
            check_feishu_rate_limit(&mut st, 1_000),
            FeishuRateDecision::EmitThrottleSummary
        );
        for _ in 0..11 {
            assert_eq!(
                check_feishu_rate_limit(&mut st, 1_000),
                FeishuRateDecision::Drop
            );
        }
        // Once the 60s window expires, traffic is allowed again.
        assert_eq!(
            check_feishu_rate_limit(&mut st, 62_000),
            FeishuRateDecision::Allow
        );
    }
}
