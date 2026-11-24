//! `pipeline.v1` — playbooks, stages and bindings (contracts §1.4).
//!
//! Stored as TOML at `<root>/.starfane/pipelines/<P-id>.toml`.

use crate::agent::{AgentKind, Effort, NativeMode};
use crate::artifact::review::ReviewDepth;
use crate::error::ProtoError;
use crate::id::{ArtifactId, PaneId, PipelineId, SessionId};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybookKind {
    Feature,
    Debug,
}

impl PlaybookKind {
    /// Feature => [Plan, Implement, TestReview, Report]; Debug => [Triage, Deep].
    pub fn stages(self) -> &'static [StageKind] {
        match self {
            PlaybookKind::Feature => &[
                StageKind::Plan,
                StageKind::Implement,
                StageKind::TestReview,
                StageKind::Report,
            ],
            PlaybookKind::Debug => &[StageKind::Triage, StageKind::Deep],
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PlaybookKind::Feature => "feature",
            PlaybookKind::Debug => "debug",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "feature" => Some(PlaybookKind::Feature),
            "debug" => Some(PlaybookKind::Debug),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Plan,
    Implement,
    TestReview,
    Report,
    Triage,
    Deep,
}

impl StageKind {
    pub const ALL: [StageKind; 6] = [
        StageKind::Plan,
        StageKind::Implement,
        StageKind::TestReview,
        StageKind::Report,
        StageKind::Triage,
        StageKind::Deep,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StageKind::Plan => "plan",
            StageKind::Implement => "implement",
            StageKind::TestReview => "test_review",
            StageKind::Report => "report",
            StageKind::Triage => "triage",
            StageKind::Deep => "deep",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_ascii_lowercase();
        StageKind::ALL.into_iter().find(|k| k.as_str() == t)
    }

    pub fn playbook(self) -> PlaybookKind {
        match self {
            StageKind::Triage | StageKind::Deep => PlaybookKind::Debug,
            _ => PlaybookKind::Feature,
        }
    }
}

impl fmt::Display for StageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Serialized as `"auto" | "spawn" | "%12" | "session:<id>"`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SessionTarget {
    #[default]
    Auto,
    Spawn,
    Pane(PaneId),
    Session(SessionId),
}

impl TryFrom<String> for SessionTarget {
    type Error = ProtoError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("auto") {
            Ok(SessionTarget::Auto)
        } else if t.eq_ignore_ascii_case("spawn") {
            Ok(SessionTarget::Spawn)
        } else if t.len() > 1 && t.starts_with('%') {
            Ok(SessionTarget::Pane(PaneId(t.to_string())))
        } else if let Some(id) = t.strip_prefix("session:").filter(|id| !id.is_empty()) {
            Ok(SessionTarget::Session(SessionId(id.to_string())))
        } else {
            Err(ProtoError::InvalidSessionTarget(s))
        }
    }
}

impl From<SessionTarget> for String {
    fn from(t: SessionTarget) -> String {
        match t {
            SessionTarget::Auto => "auto".into(),
            SessionTarget::Spawn => "spawn".into(),
            SessionTarget::Pane(p) => p.0,
            SessionTarget::Session(s) => format!("session:{}", s.0),
        }
    }
}

fn is_default_target(t: &SessionTarget) -> bool {
    *t == SessionTarget::Auto
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageBinding {
    /// None when `same_as` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeMode>,
    #[serde(default, skip_serializing_if = "is_default_target")]
    pub session: SessionTarget,
    /// Only meaningful for TestReview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<ReviewDepth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<StageKind>,
}

impl StageBinding {
    /// Parse `<agent>[:<model>][@<effort>][ /<native>]` or `same_as:<stage>`.
    ///
    /// Examples: `claude:opus@high`, `codex`, `agy:gemini-4-neon-high /swarm`,
    /// `claude /subagents`, `same_as:plan`.
    pub fn from_shorthand(s: &str) -> Result<Self, ProtoError> {
        let err = |reason: &str| ProtoError::InvalidShorthand {
            input: s.to_string(),
            reason: reason.to_string(),
        };
        let input = s.trim();
        if input.is_empty() {
            return Err(err("empty"));
        }
        if let Some(stage) = input.strip_prefix("same_as:") {
            let stage = StageKind::parse(stage).ok_or_else(|| err("unknown stage in same_as"))?;
            return Ok(StageBinding {
                same_as: Some(stage),
                ..Default::default()
            });
        }
        let mut binding = StageBinding::default();
        let mut agent_seen = false;
        for token in input.split_whitespace() {
            if token.starts_with('/') {
                if binding.native.is_some() {
                    return Err(err("more than one native mode"));
                }
                binding.native =
                    Some(NativeMode::parse(token).ok_or_else(|| err("unknown native mode"))?);
                continue;
            }
            if agent_seen {
                return Err(err("unexpected extra token"));
            }
            agent_seen = true;
            let (head, effort) = match token.rsplit_once('@') {
                Some((h, e)) => (
                    h,
                    Some(Effort::parse(e).ok_or_else(|| err("unknown effort"))?),
                ),
                None => (token, None),
            };
            let (agent, model) = match head.split_once(':') {
                Some((a, m)) if !m.is_empty() => (a, Some(m.to_string())),
                Some(_) => return Err(err("empty model")),
                None => (head, None),
            };
            binding.agent = Some(AgentKind::parse(agent).ok_or_else(|| err("unknown agent"))?);
            binding.model = model;
            binding.effort = effort;
        }
        if !agent_seen {
            return Err(err("missing agent"));
        }
        Ok(binding)
    }

    /// Inverse of [`StageBinding::from_shorthand`] (session/depth are not encoded).
    pub fn to_shorthand(&self) -> String {
        if let Some(stage) = self.same_as {
            return format!("same_as:{}", stage.as_str());
        }
        let mut out = self.agent.map(|a| a.as_str()).unwrap_or("?").to_string();
        if let Some(m) = &self.model {
            out.push(':');
            out.push_str(m);
        }
        if let Some(e) = self.effort {
            out.push('@');
            out.push_str(e.as_str());
        }
        if let Some(n) = self.native {
            out.push_str(" /");
            out.push_str(n.as_str());
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    HumanConfirm,
    ReviewGates,
    EvidenceG2,
    SchemaG1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Plan,
    Handoff,
    Review,
    Triage,
    Rca,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    AwaitingHuman,
    Blocked,
    Done,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    AwaitingBindingConfirm,
    Running,
    AwaitingHuman,
    Blocked,
    Done,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopSpec {
    pub back_to: StageKind,
    pub max_rounds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageSpec {
    pub id: StageKind,
    pub binding: StageBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateKind>,
    pub artifact: ArtifactKind,
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_spec: Option<LoopSpec>,
    pub status: StageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub artifacts: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
}

impl StageSpec {
    /// Default stage skeleton (gate / artifact / loop) for a playbook stage.
    pub fn template(id: StageKind, binding: StageBinding, max_review_rounds: u32) -> Self {
        let (gate, artifact, loop_spec) = match id {
            StageKind::Plan => (Some(GateKind::HumanConfirm), ArtifactKind::Plan, None),
            StageKind::Implement => (Some(GateKind::SchemaG1), ArtifactKind::Handoff, None),
            StageKind::TestReview => (
                Some(GateKind::ReviewGates),
                ArtifactKind::Review,
                Some(LoopSpec {
                    back_to: StageKind::Implement,
                    max_rounds: max_review_rounds,
                }),
            ),
            StageKind::Report => (Some(GateKind::SchemaG1), ArtifactKind::Report, None),
            StageKind::Triage => (Some(GateKind::EvidenceG2), ArtifactKind::Triage, None),
            StageKind::Deep => (Some(GateKind::EvidenceG2), ArtifactKind::Rca, None),
        };
        StageSpec {
            id,
            binding,
            gate,
            artifact,
            loop_spec,
            status: StageStatus::Pending,
            session_id: None,
            pane: None,
            round: 0,
            artifacts: Vec::new(),
            started_ms: None,
            ended_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Web,
    Feishu,
    Cli,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineSource {
    pub via: Via,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utterance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
}

/// Serialized as `"auto" | "none" | <path>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum WorktreeMode {
    #[default]
    Auto,
    None,
    Path(String),
}

impl TryFrom<String> for WorktreeMode {
    type Error = ProtoError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("auto") {
            Ok(WorktreeMode::Auto)
        } else if t.eq_ignore_ascii_case("none") {
            Ok(WorktreeMode::None)
        } else if t.is_empty() {
            Err(ProtoError::InvalidValue {
                field: "worktree",
                value: s,
            })
        } else {
            Ok(WorktreeMode::Path(t.to_string()))
        }
    }
}

impl From<WorktreeMode> for String {
    fn from(w: WorktreeMode) -> String {
        match w {
            WorktreeMode::Auto => "auto".into(),
            WorktreeMode::None => "none".into(),
            WorktreeMode::Path(p) => p,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub worktree: WorktreeMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineOptions {
    pub review_depth: ReviewDepth,
    pub max_review_rounds: u32,
    pub triage_timebox_secs: u64,
    pub auto_deep: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        PipelineOptions {
            review_depth: ReviewDepth::D1,
            max_review_rounds: 2,
            triage_timebox_secs: 600,
            auto_deep: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeishuThreadRef {
    pub chat_id: String,
    pub root_message_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineSpec {
    pub v: u32,
    pub id: PipelineId,
    pub playbook: PlaybookKind,
    pub goal: String,
    pub status: PipelineStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub project: ProjectRef,
    pub source: PipelineSource,
    #[serde(default)]
    pub options: PipelineOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feishu_thread: Option<FeishuThreadRef>,
    /// Same order as `playbook.stages()`.
    pub stages: Vec<StageSpec>,
}

impl PipelineSpec {
    pub fn stage(&self, id: StageKind) -> Option<&StageSpec> {
        self.stages.iter().find(|s| s.id == id)
    }

    pub fn stage_mut(&mut self, id: StageKind) -> Option<&mut StageSpec> {
        self.stages.iter_mut().find(|s| s.id == id)
    }

    /// First stage that is not Done / Skipped.
    pub fn current_stage(&self) -> Option<&StageSpec> {
        self.stages
            .iter()
            .find(|s| !matches!(s.status, StageStatus::Done | StageStatus::Skipped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_roundtrip() {
        for s in [
            "claude:opus@high",
            "codex",
            "agy:gemini-4-neon-high /swarm",
            "claude /subagents",
            "same_as:plan",
            "agy@xhigh /boost",
        ] {
            let b = StageBinding::from_shorthand(s).unwrap();
            assert_eq!(b.to_shorthand(), s, "roundtrip {s}");
        }
        let b = StageBinding::from_shorthand("  AGY:neon@high   /teamwork ").unwrap();
        assert_eq!(b.agent, Some(AgentKind::Agy));
        assert_eq!(b.model.as_deref(), Some("neon"));
        assert_eq!(b.effort, Some(Effort::High));
        assert_eq!(b.native, Some(NativeMode::Teamwork));
    }

    #[test]
    fn shorthand_errors() {
        for s in [
            "",
            "gpt",
            "claude:",
            "claude@ultra",
            "claude codex",
            "/swarm",
            "agy /swarm /boost",
            "same_as:nope",
        ] {
            assert!(
                StageBinding::from_shorthand(s).is_err(),
                "should fail: {s:?}"
            );
        }
    }

    #[test]
    fn session_target_strings() {
        let cases = [
            ("auto", SessionTarget::Auto),
            ("spawn", SessionTarget::Spawn),
            ("%3", SessionTarget::Pane(PaneId::new("%3"))),
            ("session:abc", SessionTarget::Session(SessionId::new("abc"))),
        ];
        for (s, t) in cases {
            assert_eq!(SessionTarget::try_from(s.to_string()).unwrap(), t);
            assert_eq!(String::from(t), s);
        }
        assert!(SessionTarget::try_from("%".to_string()).is_err());
        assert!(SessionTarget::try_from("session:".to_string()).is_err());
        assert!(serde_json::from_str::<SessionTarget>("\"bogus\"").is_err());
    }

    #[test]
    fn worktree_strings() {
        assert_eq!(
            WorktreeMode::try_from("auto".to_string()).unwrap(),
            WorktreeMode::Auto
        );
        assert_eq!(
            WorktreeMode::try_from("/tmp/w".to_string()).unwrap(),
            WorktreeMode::Path("/tmp/w".into())
        );
        assert!(WorktreeMode::try_from(" ".to_string()).is_err());
    }

    #[test]
    fn playbook_stages_and_templates() {
        assert_eq!(PlaybookKind::Feature.stages().len(), 4);
        assert_eq!(
            PlaybookKind::Debug.stages(),
            &[StageKind::Triage, StageKind::Deep]
        );
        for k in StageKind::ALL {
            assert_eq!(StageKind::parse(k.as_str()), Some(k));
            assert!(k.playbook().stages().contains(&k));
        }
        let t = StageSpec::template(StageKind::TestReview, StageBinding::default(), 2);
        assert_eq!(t.gate, Some(GateKind::ReviewGates));
        assert_eq!(
            t.loop_spec.as_ref().map(|l| l.back_to),
            Some(StageKind::Implement)
        );
        assert_eq!(
            serde_json::to_value(StageKind::TestReview).unwrap(),
            "test_review"
        );
    }

    #[test]
    fn options_default() {
        let o: PipelineOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o, PipelineOptions::default());
        assert_eq!(o.review_depth, ReviewDepth::D1);
    }
}
