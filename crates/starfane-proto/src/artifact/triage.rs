//! `triage.v1` — Phase A fast triage. Conclusions are always hypotheses.

use crate::artifact::{ArtifactHeader, EvidenceRef};
use serde::{Deserialize, Serialize};

/// Fixed value: Phase A output is never a confirmed root cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisKind {
    #[default]
    Hypothesis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    #[serde(default)]
    pub kind: HypothesisKind,
    pub claim: String,
    /// 0.0..=1.0
    pub confidence: f32,
    /// G1: ≥1
    pub evidence_refs: Vec<EvidenceRef>,
    pub how_to_confirm: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageReport {
    #[serde(default)]
    pub header: ArtifactHeader,
    pub symptom: String,
    /// G1: 1..=2
    pub hypotheses: Vec<Hypothesis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigation: Option<String>,
    #[serde(default)]
    pub probes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
}
