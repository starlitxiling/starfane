//! `handoff.v1`

use crate::agent::AgentKind;
use crate::artifact::{AuthorRef, TestRun};
use crate::id::{HandoffId, PaneId, PipelineId, SessionId};
use crate::session::StageRef;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffIntent {
    Review,
    Implement,
    Continue,
    Ask,
    Fyi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    ReadOnly,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    Pending,
    Delivered,
    Acked,
    Done,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageRef>,
}

impl HandoffTarget {
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none()
            && self.pane.is_none()
            && self.agent.is_none()
            && self.stage.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPacket {
    pub v: u32,
    pub id: HandoffId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<PipelineId>,
    pub from: AuthorRef,
    pub to: HandoffTarget,
    pub intent: HandoffIntent,
    pub mode: AccessMode,
    /// Filled by the daemon via git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoState>,
    /// G1: non-empty, ≤500 chars.
    pub summary: String,
    pub body_md: String,
    /// G1: required when `intent == Review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_test: Option<TestRun>,
    #[serde(default)]
    pub todo: Vec<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    pub status: HandoffStatus,
    pub created_ms: u64,
}
