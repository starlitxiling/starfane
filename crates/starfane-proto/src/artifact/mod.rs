//! Artifact common types (contracts §1.5) and per-kind submodules (§1.6).
//!
//! Canonical storage: `<root>/.starfane/artifacts/<P-id>/<A-id>.json` (+ rendered `.md`).

pub mod handoff;
pub mod plan;
pub mod rca;
pub mod report;
pub mod review;
pub mod triage;

use crate::agent::AgentKind;
use crate::id::{ArtifactId, PipelineId, SessionId};
use crate::pipeline::{ArtifactKind, StageKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorRef {
    pub agent: AgentKind,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Placeholder used when a submitter omits the author; the daemon always overwrites it.
impl Default for AuthorRef {
    fn default() -> Self {
        AuthorRef {
            agent: AgentKind::Claude,
            session_id: SessionId::default(),
            model: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactHeader {
    pub v: u32,
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<PipelineId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageKind>,
    #[serde(default)]
    pub author: AuthorRef,
    #[serde(default)]
    pub created_ms: u64,
    /// HEAD at the time the artifact was produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl ArtifactHeader {
    pub fn new(kind: ArtifactKind) -> Self {
        ArtifactHeader {
            v: crate::PROTO_VERSION,
            id: ArtifactId::default(),
            kind,
            pipeline_id: None,
            stage: None,
            author: AuthorRef::default(),
            created_ms: 0,
            commit: None,
        }
    }
}

/// Placeholder header for submissions that omit it (MCP); the daemon overwrites
/// every field including `kind` before storing.
impl Default for ArtifactHeader {
    fn default() -> Self {
        ArtifactHeader::new(ArtifactKind::Plan)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRef {
    pub file: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceQuote {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceRef {
    Code(EvidenceQuote),
    /// `path` relative to the repo root, or absolute.
    Log {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        quote: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Pass,
    Fail,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRun {
    pub cmd: String,
    pub outcome: TestOutcome,
    /// ≤2000 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_ref_tagging() {
        let code = EvidenceRef::Code(EvidenceQuote {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 2,
            quote: "let x = 1;".into(),
        });
        let v = serde_json::to_value(&code).unwrap();
        assert_eq!(v["type"], "code");
        assert_eq!(v["file"], "src/a.rs");
        let log: EvidenceRef =
            serde_json::from_str(r#"{"type":"log","path":"logs/a.log","quote":"500"}"#).unwrap();
        assert!(matches!(log, EvidenceRef::Log { line: None, .. }));
        assert_eq!(serde_json::from_value::<EvidenceRef>(v).unwrap(), code);
    }

    #[test]
    fn header_defaults_when_missing_fields() {
        let h: ArtifactHeader = serde_json::from_str(r#"{"v":1,"id":"","kind":"rca"}"#).unwrap();
        assert_eq!(h.kind, ArtifactKind::Rca);
        assert_eq!(h.author, AuthorRef::default());
    }
}
