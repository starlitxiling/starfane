//! `rca.v1` — Phase B deep root-cause analysis.

use crate::artifact::{ArtifactHeader, CodeRef, EvidenceRef, TestRun};
use crate::id::ArtifactId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootCause {
    pub claim: String,
    pub trigger_path: Vec<CodeRef>,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixRef {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuledOut {
    pub hypothesis: String,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RcaReport {
    #[serde(default)]
    pub header: ArtifactHeader,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_id: Option<ArtifactId>,
    pub root_cause: RootCause,
    /// G1: outcome must be Fail unless `repro_note` explains why not reproducible.
    pub repro_before: TestRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixRef>,
    /// G1: required with outcome Pass when `fix` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_after: Option<TestRun>,
    #[serde(default)]
    pub regression: Vec<TestRun>,
    #[serde(default)]
    pub ruled_out: Vec<RuledOut>,
}
