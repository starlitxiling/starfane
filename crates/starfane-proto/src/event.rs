//! Normalized hook events (contracts §1.2).

use crate::agent::AgentKind;
use crate::id::{BindToken, EventId, PaneId, SessionId};
use crate::paths::ENV_BIND;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    Working,
    TurnComplete,
    NeedsPermission,
    NeedsInput,
    Error,
    Interrupted,
    SessionEnd,
    SubagentStart,
    SubagentEnd,
    /// agy permission heuristic only; never notified.
    ToolStart,
    /// agy permission heuristic only; never notified.
    ToolEnd,
    /// Recognized but carries no semantics (e.g. claude `Notification:auth_success`).
    Ignored,
}

impl EventKind {
    /// Kinds that may reach the notification router (root sessions only).
    pub fn is_attention(self) -> bool {
        matches!(
            self,
            EventKind::TurnComplete
                | EventKind::NeedsPermission
                | EventKind::NeedsInput
                | EventKind::Error
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    Heuristic,
}

/// Environment captured by `starfane emit` at hook time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvCtx {
    /// `$TMUX_PANE`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_pane: Option<String>,
    /// `$TMUX`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux: Option<String>,
    /// pid of the emit process itself.
    #[serde(default)]
    pub pid: u32,
    /// Parent pid (the hook shell).
    #[serde(default)]
    pub ppid: u32,
    /// emit's cwd (for agy this is the hooks.json dir — not trustworthy).
    #[serde(default)]
    pub cwd: String,
    /// `$STARFANE_BIND`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_token: Option<BindToken>,
}

impl EnvCtx {
    /// Capture from the current process environment.
    pub fn capture() -> Self {
        Self::capture_with(&crate::paths::env_get)
    }

    /// Capture using a custom env getter (pid/ppid/cwd still come from the process).
    pub fn capture_with(get: &dyn Fn(&str) -> Option<String>) -> Self {
        EnvCtx {
            tmux_pane: get("TMUX_PANE"),
            tmux: get("TMUX"),
            pid: std::process::id(),
            ppid: std::os::unix::process::parent_id(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            bind_token: get(ENV_BIND).or_else(|| get("BATON_BIND")).map(BindToken),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// One-line summary of `tool_input`, ≤200 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_summary: Option<String>,
    /// `last_assistant_message`, ≤500 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// claude
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    /// agy
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<String>,
    /// agy
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fully_idle: Option<bool>,
    /// agy
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// claude / codex subagents
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub v: u32,
    pub id: EventId,
    pub ts_ms: u64,
    pub agent: AgentKind,
    pub kind: EventKind,
    pub confidence: Confidence,
    /// Root session: claude/codex `session_id`, agy `conversationId`.
    pub session_id: SessionId,
    /// claude/codex `agent_id`; always None for agy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    /// From the payload's `cwd` (never `env.cwd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Original hook event name, e.g. `"Stop"`.
    pub native_event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_token: Option<BindToken>,
    #[serde(default)]
    pub detail: EventDetail,
}

/// Truncate to at most `max` chars (char-boundary safe), appending `…` when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_capture_with_getter() {
        let map: HashMap<&str, &str> = [("TMUX_PANE", "%7"), (ENV_BIND, "B-1")].into();
        let env = EnvCtx::capture_with(&|k| map.get(k).map(|v| v.to_string()));
        assert_eq!(env.tmux_pane.as_deref(), Some("%7"));
        assert_eq!(env.bind_token, Some(BindToken::new("B-1")));
        assert_eq!(env.pid, std::process::id());
        assert!(env.ppid > 0);
        assert!(!env.cwd.is_empty());
    }

    #[test]
    fn event_json_shape() {
        let ev = NormalizedEvent {
            v: 1,
            id: EventId::new("E-1"),
            ts_ms: 1,
            agent: AgentKind::Claude,
            kind: EventKind::NeedsPermission,
            confidence: Confidence::Exact,
            session_id: SessionId::new("s1"),
            subagent_id: None,
            pane: Some(PaneId::new("%3")),
            cwd: None,
            model: None,
            native_event: "PermissionRequest".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail::default(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], "needs_permission");
        assert_eq!(v["confidence"], "exact");
        assert!(v.get("subagent_id").is_none());
        let back: NormalizedEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("你好世界啊", 3), "你好…");
    }
}
