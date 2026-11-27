//! Session registry records and the shared state transition (contracts §1.3).

use crate::agent::AgentKind;
use crate::event::{Confidence, EventKind};
use crate::id::{EventId, HostId, PaneId, PipelineId, SessionId};
use crate::pipeline::StageKind;
use serde::{Deserialize, Serialize};

pub const LOCAL_HOST_ID: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    Local,
    Ssh,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Online,
    Degraded,
    Offline,
}

/// Summary of a local or federated host exposed in `/api/state` and `/api/hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub id: HostId,
    pub display_name: String,
    pub kind: HostKind,
    pub status: HostStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub session_count: usize,
    /// Pure network/transport round-trip latency in milliseconds (never discovery/dispatch wall-clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    /// Last discovery/sync wall-clock duration in milliseconds (diagnostics only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_paused: bool,
}

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
    Suspended,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResolution {
    BindToken,
    Env,
    PidTree,
    CwdMatch,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StageRef {
    pub pipeline_id: PipelineId,
    pub stage: StageKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnMetricSample {
    pub ts_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
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
    /// Time to first token (ms), when available from native step/transcript telemetry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Rolling average TTFT (ms) over `recent_turns` with valid `ttft_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cache_read_tokens: Option<u64>,
    /// Recent per-turn TTFT / TPS samples in chronological order (capped at 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_turns: Vec<TurnMetricSample>,
    /// Data source: `"agy_db"`, `"transcript"`, `"status_line"`, or `"none"`.
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
    #[serde(default)]
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
    #[serde(default)]
    pub state_since_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<EventId>,
    #[serde(default)]
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
    #[serde(default)]
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
    /// Immutable host id (`None` is treated as `"local"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    /// Human-friendly mutable host label (resolved from config/state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    /// Terminal multiplexer kind (`"tmux"`, `"zellij"`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mux_kind: Option<String>,
    /// Canonical agent resume/conversation identifier captured for suspend/wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_id: Option<String>,
    /// Epoch ms when this session was suspended (cleared on wake).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_at_ms: Option<u64>,
}

impl SessionRecord {
    /// Effective host id (defaults to `"local"` when unset).
    pub fn effective_host_id(&self) -> &str {
        self.host_id
            .as_ref()
            .map(HostId::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(LOCAL_HOST_ID)
    }

    /// True if this session belongs to the local host.
    pub fn is_local_host(&self) -> bool {
        self.effective_host_id() == LOCAL_HOST_ID
    }
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
        assert_eq!(S::Suspended.next(K::Ignored), S::Suspended);
        assert_eq!(S::Suspended.next(K::SessionStart), S::Starting);
        assert_eq!(S::Suspended.next(K::Working), S::Working);
        assert_eq!(S::Suspended.next(K::TurnComplete), S::Idle);
        assert_eq!(S::Suspended.next(K::SessionEnd), S::Ended);
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

    #[test]
    fn host_record_latency_fields_default_and_roundtrip() {
        let legacy_json = serde_json::json!({
            "id": "tencent",
            "display_name": "Tencent GPU",
            "kind": "ssh",
            "status": "online",
            "session_count": 2
        });
        let legacy: HostRecord = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.rtt_ms, None);
        assert_eq!(legacy.sync_ms, None);
        assert_eq!(legacy.next_retry_ms, None);
        assert!(!legacy.retry_paused);
        let ser_legacy = serde_json::to_value(&legacy).unwrap();
        assert!(ser_legacy.get("rtt_ms").is_none());
        assert!(ser_legacy.get("sync_ms").is_none());
        assert!(ser_legacy.get("next_retry_ms").is_none());
        assert!(ser_legacy.get("retry_paused").is_none());

        let with_net = HostRecord {
            rtt_ms: Some(142),
            sync_ms: Some(48),
            next_retry_ms: Some(1_700_000_015_000),
            retry_paused: true,
            ..legacy
        };
        let ser = serde_json::to_value(&with_net).unwrap();
        assert_eq!(ser["rtt_ms"], 142);
        assert_eq!(ser["sync_ms"], 48);
        assert_eq!(ser["next_retry_ms"], 1_700_000_015_000u64);
        assert_eq!(ser["retry_paused"], true);
        let back: HostRecord = serde_json::from_value(ser).unwrap();
        assert_eq!(back, with_net);
    }
}
