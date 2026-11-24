//! Session registry records and the shared state transition (contracts §1.3).

use crate::agent::AgentKind;
use crate::event::{Confidence, EventKind};
use crate::id::{EventId, PaneId, PipelineId, SessionId};
use crate::pipeline::StageKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Unknown,
    Starting,
    Working,
    Idle,
    NeedsPermission,
    NeedsInput,
    Error,
    Ended,
}

impl SessionState {
    /// Transition table for a session's own (non-subagent) events.
    pub fn next(self, kind: EventKind) -> SessionState {
        match kind {
            EventKind::SessionStart => {
                if matches!(
                    self,
                    SessionState::Working
                        | SessionState::NeedsPermission
                        | SessionState::NeedsInput
                ) {
                    self
                } else {
                    SessionState::Starting
                }
            }
            EventKind::Working
            | EventKind::ToolStart
            | EventKind::ToolEnd
            | EventKind::SubagentStart => SessionState::Working,
            EventKind::TurnComplete | EventKind::Interrupted => SessionState::Idle,
            EventKind::NeedsPermission => SessionState::NeedsPermission,
            EventKind::NeedsInput => SessionState::NeedsInput,
            EventKind::Error => SessionState::Error,
            EventKind::SessionEnd => SessionState::Ended,
            EventKind::SubagentEnd | EventKind::Ignored => self,
        }
    }

    /// Parent-state effect of a subagent event: `NeedsPermission` propagates to parent,
    /// and subsequent subagent progress/completion unlatches parent `NeedsPermission`.
    pub fn next_from_subagent(self, kind: EventKind) -> SessionState {
        if kind == EventKind::NeedsPermission {
            SessionState::NeedsPermission
        } else if self == SessionState::NeedsPermission
            && matches!(
                kind,
                EventKind::Working
                    | EventKind::ToolStart
                    | EventKind::ToolEnd
                    | EventKind::TurnComplete
                    | EventKind::SubagentEnd
            )
        {
            SessionState::Working
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResolution {
    BindToken,
    Env,
    PidTree,
    CwdMatch,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StageRef {
    pub pipeline_id: PipelineId,
    pub stage: StageKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f32>,
    /// Time to first token (ms), when available from proxy/request logs (e.g. `cc_switch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    /// Data source: `"cc_switch"`, `"transcript"`, `"status_line"`, or `"none"`.
    pub source: String,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Sub: `<root>/<agent_id>` (claude/codex); agy folded subs keep their conversationId.
    pub session_id: SessionId,
    pub agent: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    pub pane_resolution: PaneResolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model reasoning/thinking effort level (e.g. `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, `"fixed"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub state: SessionState,
    pub state_since_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<EventId>,
    pub last_event_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Concise error message captured from `StopFailure` / AGY `ERROR` hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    #[serde(default)]
    pub is_subagent: bool,
    /// Cumulative.
    #[serde(default)]
    pub subagent_count: u32,
    /// Currently not ended.
    #[serde(default)]
    pub active_subagents: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageRef>,
    #[serde(default)]
    pub unread_inbox: u32,
    /// Heuristic for agy folded subagents.
    pub confidence: Confidence,
    /// tmux target label, e.g. `"prome:4.1"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_target: Option<String>,
    /// Human-friendly tmux label `<session_name>-<window_name>`, e.g. `"learning-workflow"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_label: Option<String>,
    /// True when discovered directly from a live tmux pane without hook events yet.
    #[serde(default)]
    pub discovered_only: bool,
    /// Last known transcript JSONL path reported by hooks or exact PID/resume mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// Live context-window & generation-speed (TPS) telemetry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SessionMetrics>,
}

/// Build a claude/codex subagent session id: `<root>/<agent_id>`.
pub fn subagent_session_id(root: &SessionId, agent_id: &str) -> SessionId {
    SessionId(format!("{}/{}", root.0, agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        use EventKind as K;
        use SessionState as S;
        assert_eq!(S::Unknown.next(K::SessionStart), S::Starting);
        assert_eq!(S::Starting.next(K::Working), S::Working);
        assert_eq!(S::Working.next(K::TurnComplete), S::Idle);
        assert_eq!(S::Working.next(K::Interrupted), S::Idle);
        assert_eq!(S::Idle.next(K::NeedsPermission), S::NeedsPermission);
        assert_eq!(S::Idle.next(K::NeedsInput), S::NeedsInput);
        assert_eq!(S::Working.next(K::Error), S::Error);
        assert_eq!(S::Idle.next(K::SessionEnd), S::Ended);
        assert_eq!(S::Idle.next(K::Ignored), S::Idle);
        assert_eq!(S::Working.next(K::SubagentEnd), S::Working);
        assert_eq!(S::Idle.next(K::ToolStart), S::Working);
        assert_eq!(S::Working.next_from_subagent(K::TurnComplete), S::Working);
        assert_eq!(
            S::Working.next_from_subagent(K::NeedsPermission),
            S::NeedsPermission
        );
        // Async SessionStart arriving after UserPromptSubmit must not downgrade Working/Needs*.
        assert_eq!(S::Working.next(K::SessionStart), S::Working);
        assert_eq!(S::NeedsPermission.next(K::SessionStart), S::NeedsPermission);
        // Subagent progress/completion unlatches parent NeedsPermission.
        assert_eq!(
            S::NeedsPermission.next_from_subagent(K::ToolEnd),
            S::Working
        );
        assert_eq!(
            S::NeedsPermission.next_from_subagent(K::SubagentEnd),
            S::Working
        );
    }

    #[test]
    fn sub_id() {
        assert_eq!(
            subagent_session_id(&SessionId::new("s1"), "a9").as_str(),
            "s1/a9"
        );
    }
}
