//! Outbound human notifications produced by the router / playbook engine and
//! consumed by the Feishu adapter (`lark` module).

use serde::{Deserialize, Serialize};
use starfane_proto::{AgentKind, ApiError, PaneId, PipelineId, ReviewId, SessionId};
use std::future::Future;
use std::pin::Pin;

/// Session metadata attached to a Feishu notification card so users can reply
/// directly via quote-reply (`parent_id`) or inline card form (`reply_session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplyTarget {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<PaneId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_label: Option<String>,
    pub agent: AgentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    BindingConfirm,
    PlanApproval,
    Blocked,
    Error,
    TurnComplete,
    TriageReady,
    ReviewResult,
    FinalReport,
    Progress,
    DeliveryUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStyle {
    Primary,
    Danger,
    Default,
}

/// A card button. `op` is one of the contract §7.4 card ops
/// (`confirm_pipeline`, `cancel_pipeline`, `approve_plan`, `request_changes`,
/// `continue_deep`, `stop_deep`, `adopt_mitigation`, `feedback_valid`,
/// `feedback_fp`, `feedback_wontfix`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeAction {
    pub op: String,
    pub label: String,
    pub style: ActionStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    /// Stable dedup / idempotency key (≤50 chars after truncation by the sender).
    pub key: String,
    pub kind: NoticeKind,
    pub title: String,
    pub body_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<PipelineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<ReviewId>,
    #[serde(default)]
    pub actions: Vec<NoticeAction>,
    /// Binding-confirm cards: stage → current shorthand (rendered as selects).
    #[serde(default)]
    pub binding_form: Vec<(String, String)>,
    /// When set, the Feishu card renders an inline reply form and registers its
    /// `message_id` for direct zero-prefix quote-reply forwarding to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_session: Option<SessionReplyTarget>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Implemented by the Feishu adapter and registered with `Core::set_feishu`.
pub trait FeishuSender: Send + Sync {
    /// Send a plain test message; returns the Feishu message id.
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>>;
}
