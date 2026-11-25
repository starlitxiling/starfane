//! `DaemonCommand` — shared by UDS, REST, Feishu and CLI (contracts §1.8).

use crate::agent::{AgentKind, Effort};
use crate::artifact::handoff::{AccessMode, HandoffIntent, HandoffStatus, HandoffTarget};
use crate::artifact::plan::PlanDoc;
use crate::artifact::rca::RcaReport;
use crate::artifact::report::FinalReport;
use crate::artifact::review::{
    FalsifierSubmission, FalsifierVerdict, FeedbackLabel, G1Result, G2Outcome, ReviewReport,
};
use crate::artifact::triage::TriageReport;
use crate::artifact::{AuthorRef, TestRun};
use crate::id::{
    ArtifactId, BindToken, FindingId, HandoffId, PaneId, PipelineId, ReviewId, SessionId,
};
use crate::pipeline::{
    ArtifactKind, PipelineOptions, PipelineSource, PipelineSpec, PlaybookKind, StageBinding,
    StageKind,
};
use crate::session::{PaneResolution, SessionRecord, StageRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// serde default helper (`true`).
pub fn default_true() -> bool {
    true
}

/// Transient request message: variant size disparity is irrelevant here and
/// boxing would change the frozen field types, so the lint is allowed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// -> `{"pong":true,"version":String}`
    Ping,
    /// -> `StateSnapshot`
    GetState,
    /// -> `Vec<SessionRecord>`
    ListSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default)]
        include_subagents: bool,
        #[serde(default)]
        include_ended: bool,
    },
    /// -> `SessionRecord`
    AckSession { session_id: SessionId },
    /// -> `IdentityResolution`
    ResolveIdentity {
        agent: AgentKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<PaneId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind_token: Option<BindToken>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_hint: Option<SessionId>,
        /// Ancestor PID chain of the calling process (self -> ppid -> ...), used as a
        /// deterministic fallback when an agent CLI (e.g. Codex) strips `TMUX_PANE` from
        /// MCP subprocesses.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pid_chain: Vec<u32>,
    },
    /// -> `Vec<InboxItem>`
    ReadInbox {
        session_id: SessionId,
        #[serde(default = "default_true")]
        mark_delivered: bool,
    },
    /// -> `CreateHandoffResult`
    CreateHandoff { draft: HandoffDraft },
    /// -> `{}`
    AckHandoff {
        session_id: SessionId,
        handoff_id: HandoffId,
        status: HandoffStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// -> `SubmitOutcome`
    SubmitArtifact {
        session_id: SessionId,
        artifact: ArtifactPayload,
    },
    /// -> `FalsifierOutcome`
    SubmitFalsifierVerdict {
        session_id: SessionId,
        submission: FalsifierSubmission,
    },
    /// -> `{}`
    ReportProgress {
        session_id: SessionId,
        text: String,
        #[serde(default)]
        notify_human: bool,
    },
    /// -> `ParsedBinding` (defaults applied; type lives in starfane-binding)
    ParseUtterance {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playbook: Option<PlaybookKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// -> `PipelineSpec` (AwaitingBindingConfirm)
    CreatePipeline { req: CreatePipelineReq },
    /// -> `PipelineSpec` (Running)
    ConfirmPipeline {
        pipeline_id: PipelineId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overrides: Option<BTreeMap<StageKind, StageBinding>>,
    },
    /// -> `PipelineSpec`
    PipelineAction {
        pipeline_id: PipelineId,
        action: PipelineAction,
    },
    /// -> `Vec<PipelineSpec>`
    ListPipelines {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default)]
        include_done: bool,
    },
    /// -> `PipelineSpec`
    GetPipeline { pipeline_id: PipelineId },
    /// -> `ArtifactPayload`
    GetArtifact { id: ArtifactId },
    /// -> `Vec<HandoffPacket>`
    ListHandoffs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pipeline_id: Option<PipelineId>,
    },
    /// -> `ReviewRecord`
    GetReview { review_id: ReviewId },
    /// -> `Vec<ReviewRecord>`
    ListReviews {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pipeline_id: Option<PipelineId>,
    },
    /// -> `{}`
    ReviewFeedback {
        review_id: ReviewId,
        finding_id: FindingId,
        label: FeedbackLabel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        by: String,
    },
    /// -> `AdjudicatedFinding`
    HumanVerdict {
        review_id: ReviewId,
        finding_id: FindingId,
        verdict: FalsifierVerdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        by: String,
    },
    /// -> `Vec<PrecisionStat>`
    GetPrecision {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// -> `DeliveryReport` (empty `allow` = `[L0, L1, L2]`)
    Dispatch {
        target: DispatchTarget,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PasteAttachment>,
        #[serde(default)]
        allow: Vec<DeliveryLevel>,
        /// Explicit interactive send from UI: keep only hard tmux safety checks
        /// (`pane_dead`, `pane_in_mode`, `pane_synchronized`) and inject immediately.
        #[serde(default)]
        interactive: bool,
    },
    /// -> `{"session_id": SessionId, "pane_id": PaneId, "tmux_target": String, "tmux_label": Option<String>, "agent": AgentKind, "model": Option<String>, "effort": Option<String>, "cwd": String}`
    SpawnSession {
        agent: AgentKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_session: Option<SessionId>,
    },
    /// -> `{"message_id": String}`
    FeishuTest { text: String },
    /// -> `BackupSaveOutcome`
    TmuxBackup {
        #[serde(default)]
        force: bool,
    },
    /// -> `RestoreReport`
    TmuxRestore {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore_agents: Option<bool>,
        #[serde(default)]
        dry_run: bool,
    },
    /// -> `Vec<SnapshotSummary>`
    ListTmuxBackups,
    /// -> `{}`
    Shutdown,
}

impl DaemonCommand {
    /// snake_case command name (the `cmd` tag value).
    pub fn name(&self) -> &'static str {
        match self {
            DaemonCommand::Ping => "ping",
            DaemonCommand::GetState => "get_state",
            DaemonCommand::ListSessions { .. } => "list_sessions",
            DaemonCommand::AckSession { .. } => "ack_session",
            DaemonCommand::ResolveIdentity { .. } => "resolve_identity",
            DaemonCommand::ReadInbox { .. } => "read_inbox",
            DaemonCommand::CreateHandoff { .. } => "create_handoff",
            DaemonCommand::AckHandoff { .. } => "ack_handoff",
            DaemonCommand::SubmitArtifact { .. } => "submit_artifact",
            DaemonCommand::SubmitFalsifierVerdict { .. } => "submit_falsifier_verdict",
            DaemonCommand::ReportProgress { .. } => "report_progress",
            DaemonCommand::ParseUtterance { .. } => "parse_utterance",
            DaemonCommand::CreatePipeline { .. } => "create_pipeline",
            DaemonCommand::ConfirmPipeline { .. } => "confirm_pipeline",
            DaemonCommand::PipelineAction { .. } => "pipeline_action",
            DaemonCommand::ListPipelines { .. } => "list_pipelines",
            DaemonCommand::GetPipeline { .. } => "get_pipeline",
            DaemonCommand::GetArtifact { .. } => "get_artifact",
            DaemonCommand::ListHandoffs { .. } => "list_handoffs",
            DaemonCommand::GetReview { .. } => "get_review",
            DaemonCommand::ListReviews { .. } => "list_reviews",
            DaemonCommand::ReviewFeedback { .. } => "review_feedback",
            DaemonCommand::HumanVerdict { .. } => "human_verdict",
            DaemonCommand::GetPrecision { .. } => "get_precision",
            DaemonCommand::Dispatch { .. } => "dispatch",
            DaemonCommand::SpawnSession { .. } => "spawn_session",
            DaemonCommand::FeishuTest { .. } => "feishu_test",
            DaemonCommand::TmuxBackup { .. } => "tmux_backup",
            DaemonCommand::TmuxRestore { .. } => "tmux_restore",
            DaemonCommand::ListTmuxBackups => "list_tmux_backups",
            DaemonCommand::Shutdown => "shutdown",
        }
    }
}

/// Header fields (`id` / `created_ms` / `author`) may be omitted; the daemon overwrites them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPayload {
    Plan(PlanDoc),
    Review(ReviewReport),
    Triage(TriageReport),
    Rca(RcaReport),
    Report(FinalReport),
}

impl ArtifactPayload {
    pub fn kind(&self) -> ArtifactKind {
        match self {
            ArtifactPayload::Plan(_) => ArtifactKind::Plan,
            ArtifactPayload::Review(_) => ArtifactKind::Review,
            ArtifactPayload::Triage(_) => ArtifactKind::Triage,
            ArtifactPayload::Rca(_) => ArtifactKind::Rca,
            ArtifactPayload::Report(_) => ArtifactKind::Report,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitOutcome {
    pub artifact_id: ArtifactId,
    /// G1 passed (report-level for reviews).
    pub accepted: bool,
    #[serde(default)]
    pub g1: G1Result,
    /// review: keyed by finding_id; triage/rca: `"hypotheses[0]"` / `"root_cause"`.
    #[serde(default)]
    pub g2: BTreeMap<String, G2Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<ReviewId>,
    #[serde(default)]
    pub pending_falsifier: Vec<FindingId>,
    /// Next-step hint for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Return data of `SubmitFalsifierVerdict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FalsifierOutcome {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g2: Option<G2Outcome>,
    pub effective: FalsifierVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffDraft {
    pub from_session: SessionId,
    pub to: HandoffTarget,
    pub intent: HandoffIntent,
    pub mode: AccessMode,
    pub summary: String,
    pub body_md: String,
    #[serde(default)]
    pub todo: Vec<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_test: Option<TestRun>,
}

/// Return data of `CreateHandoff`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateHandoffResult {
    pub handoff_id: HandoffId,
    pub delivery: DeliveryReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IdentityResolution {
    Resolved {
        session_id: SessionId,
        via: PaneResolution,
    },
    Ambiguous {
        candidates: Vec<SessionRecord>,
    },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Task,
    Handoff,
    Message,
    ReviewFindings,
    FalsifyRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub kind: InboxKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<AuthorRef>,
    pub created_ms: u64,
    pub title: String,
    /// Fully rendered; usable directly as the agent's task description.
    pub body_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<HandoffId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<ReviewId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatePipelineReq {
    pub playbook: PlaybookKind,
    pub project_root: String,
    pub goal: String,
    /// Missing stages fall back to team.toml defaults.
    #[serde(default)]
    pub bindings: BTreeMap<StageKind, StageBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<PipelineOptions>,
    pub source: PipelineSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PipelineAction {
    ApprovePlan,
    RequestChanges { note: String },
    Cancel,
    ContinueDeep,
    StopDeep,
    AdoptMitigation,
    RetryStage { stage: StageKind },
    SkipStage { stage: StageKind },
}

/// A pasted text snippet or uploaded image attached to a [`DaemonCommand::Dispatch`] request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteAttachment {
    /// Attachment kind: `"text"` (default) or `"image"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Server-side path under `<state_dir>/inputs/` for `"image"` attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional inline anchor token inside `Dispatch.text` (e.g. `"[粘贴 #1]"` or `"[图片 #1]"`).
    /// When present and found in `text`, the attachment is expanded in-place to preserve
    /// the user's exact interleaving order of quotes, images, and follow-up questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DispatchTarget {
    Session {
        id: SessionId,
    },
    Pane {
        id: PaneId,
    },
    Stage {
        pipeline_id: PipelineId,
        stage: StageKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryLevel {
    L0Inbox,
    L1CodexQueue,
    L2Inject,
    L4Spawn,
}

impl DeliveryLevel {
    /// Used when `Dispatch.allow` is empty.
    pub const DEFAULT_ALLOW: [DeliveryLevel; 3] = [
        DeliveryLevel::L0Inbox,
        DeliveryLevel::L1CodexQueue,
        DeliveryLevel::L2Inject,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Delivered,
    Queued,
    Unconfirmed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCheck {
    PaneAlive,
    NotInMode,
    NotSynchronized,
    AgentIdle,
    UserInactive,
    InputEmpty,
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    pub passed: bool,
    pub checks: Vec<(GateCheck, bool)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl GateReport {
    /// Build from individual check results; `passed` = all true.
    pub fn from_checks(checks: Vec<(GateCheck, bool)>, detail: Option<String>) -> Self {
        let passed = !checks.is_empty() && checks.iter().all(|(_, ok)| *ok);
        GateReport {
            passed,
            checks,
            detail,
        }
    }

    pub fn failed_checks(&self) -> Vec<GateCheck> {
        self.checks
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(c, _)| *c)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReport {
    pub level: DeliveryLevel,
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    /// Optional ACK status detail (e.g. `"unverified_legacy"` for discovered-only panes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// `binding_confirm` | `plan_approval` | `blocked` | `needs_human_finding` |
    /// `triage_ready` | `delivery_unconfirmed`
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<PipelineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<ReviewId>,
    pub since_ms: u64,
}

/// Attention kinds (`AttentionItem.kind`).
pub mod attention {
    pub const BINDING_CONFIRM: &str = "binding_confirm";
    pub const PLAN_APPROVAL: &str = "plan_approval";
    pub const BLOCKED: &str = "blocked";
    pub const NEEDS_HUMAN_FINDING: &str = "needs_human_finding";
    pub const TRIAGE_READY: &str = "triage_ready";
    pub const DELIVERY_UNCONFIRMED: &str = "delivery_unconfirmed";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub v: u32,
    pub version: String,
    pub started_ms: u64,
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    #[serde(default)]
    pub pipelines: Vec<PipelineSpec>,
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    pub event_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_tags() {
        let c: DaemonCommand = serde_json::from_value(json!({
            "cmd": "read_inbox", "session_id": "s1"
        }))
        .unwrap();
        match &c {
            DaemonCommand::ReadInbox {
                mark_delivered,
                session_id,
            } => {
                assert!(*mark_delivered);
                assert_eq!(session_id.as_str(), "s1");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(c.name(), "read_inbox");
        let v = serde_json::to_value(DaemonCommand::GetState).unwrap();
        assert_eq!(v, json!({"cmd": "get_state"}));
    }

    #[test]
    fn pipeline_action_and_dispatch_shapes() {
        let a = PipelineAction::RequestChanges { note: "x".into() };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"action":"request_changes","note":"x"})
        );
        let d = DispatchTarget::Stage {
            pipeline_id: PipelineId::new("P-1"),
            stage: StageKind::Implement,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"type":"stage","pipeline_id":"P-1","stage":"implement"})
        );
    }

    #[test]
    fn gate_report_passed() {
        let g = GateReport::from_checks(
            vec![(GateCheck::PaneAlive, true), (GateCheck::InputEmpty, false)],
            None,
        );
        assert!(!g.passed);
        assert_eq!(g.failed_checks(), vec![GateCheck::InputEmpty]);
        assert!(!GateReport::from_checks(vec![], None).passed);
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["checks"][0], json!(["pane_alive", true]));
    }

    #[test]
    fn identity_resolution_shape() {
        let r = IdentityResolution::Resolved {
            session_id: SessionId::new("s"),
            via: PaneResolution::BindToken,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"status":"resolved","session_id":"s","via":"bind_token"})
        );
        assert_eq!(
            serde_json::to_value(IdentityResolution::Unknown).unwrap(),
            json!({"status":"unknown"})
        );
    }
}
