//! `review.v1` plus gate results (G1 / G2 / Falsifier) and precision telemetry.

use crate::agent::AgentKind;
use crate::artifact::{ArtifactHeader, AuthorRef, CodeRef, EvidenceQuote};
use crate::id::{FindingId, ReviewId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Serialized as `"D0".."D3"`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub enum ReviewDepth {
    D0,
    #[default]
    D1,
    D2,
    D3,
}

impl ReviewDepth {
    /// Finding cap per depth (D0:5 / D1:8 / D2:8 / D3:10).
    pub fn max_findings(self) -> usize {
        match self {
            ReviewDepth::D0 => 5,
            ReviewDepth::D1 | ReviewDepth::D2 => 8,
            ReviewDepth::D3 => 10,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReviewDepth::D0 => "D0",
            ReviewDepth::D1 => "D1",
            ReviewDepth::D2 => "D2",
            ReviewDepth::D3 => "D3",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "D0" => Some(ReviewDepth::D0),
            "D1" => Some(ReviewDepth::D1),
            "D2" => Some(ReviewDepth::D2),
            "D3" => Some(ReviewDepth::D3),
            _ => None,
        }
    }
}

/// Ordering: `Blocker < Major < Minor < Nit` (Blocker is most severe; sorts first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    /// Blocker / Major are actionable by default and go through G3.
    pub fn is_actionable(self) -> bool {
        matches!(self, Severity::Blocker | Severity::Major)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Correctness,
    Concurrency,
    Security,
    Perf,
    Api,
    Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    CodeTrace,
    StaticCheck,
    ReproTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReproKind {
    Test,
    Input,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repro {
    pub kind: ReproKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationRange {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// Reviewer may leave empty; daemon assigns `F-…`.
    #[serde(default)]
    pub id: FindingId,
    pub category: Category,
    pub severity: Severity,
    /// 0.0..=1.0
    pub confidence: f32,
    pub claim: String,
    pub location: LocationRange,
    /// G1: ≥1
    pub evidence: Vec<EvidenceQuote>,
    #[serde(default)]
    pub trigger_path: Vec<CodeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preconditions: Option<String>,
    #[serde(default)]
    pub checked_guards: Vec<CodeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observable_impact: Option<String>,
    pub verification_method: VerificationMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro: Option<Repro>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    #[serde(default)]
    pub header: ArtifactHeader,
    pub depth: ReviewDepth,
    /// Base commit of the reviewed range.
    pub base: String,
    /// Reviewed commit — G2 verifies against this tree.
    pub head: String,
    #[serde(default)]
    pub scope_files: Vec<String>,
    /// G1: ≥1 when `findings` is empty.
    #[serde(default)]
    pub checked_areas: Vec<String>,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

// ---------------------------------------------------------------------------
// Gate results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct G1Issue {
    /// JSON-path-like, e.g. `findings[2].trigger_path`.
    pub path: String,
    pub reason: String,
}

impl G1Issue {
    pub fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        G1Issue {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct G1Result {
    #[serde(default)]
    pub report: Vec<G1Issue>,
    #[serde(default)]
    pub findings: BTreeMap<FindingId, Vec<G1Issue>>,
}

impl G1Result {
    /// True when there are no report-level or finding-level issues.
    pub fn ok(&self) -> bool {
        self.report.is_empty() && self.findings.values().all(|v| v.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum EvidenceIssue {
    FileMissing {
        file: String,
    },
    LineOutOfRange {
        file: String,
        line: u32,
        line_count: u32,
    },
    QuoteNotFound {
        file: String,
        line_start: u32,
    },
    /// Outside ±tol but unique in file — accepted (Relocated).
    QuoteRelocated {
        file: String,
        claimed: u32,
        actual: u32,
    },
    /// Outside ±tol and multiple matches — Fabricated.
    QuoteAmbiguous {
        file: String,
        occurrences: u32,
    },
    QuoteTooShort {
        file: String,
    },
    SymbolNotFound {
        symbol: String,
    },
    LogMissing {
        path: String,
    },
    LogQuoteNotFound {
        path: String,
    },
}

impl EvidenceIssue {
    /// Whether this issue alone makes the verdict `Fabricated`.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, EvidenceIssue::QuoteRelocated { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceVerdict {
    Verified,
    Relocated,
    Fabricated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct G2Outcome {
    pub verdict: EvidenceVerdict,
    #[serde(default)]
    pub issues: Vec<EvidenceIssue>,
}

impl G2Outcome {
    /// Derive the verdict from a list of issues (contracts §5 final-verdict rule).
    pub fn from_issues(issues: Vec<EvidenceIssue>) -> Self {
        let verdict = if issues.iter().any(EvidenceIssue::is_fatal) {
            EvidenceVerdict::Fabricated
        } else if issues.is_empty() {
            EvidenceVerdict::Verified
        } else {
            EvidenceVerdict::Relocated
        };
        G2Outcome { verdict, issues }
    }

    pub fn accepted(&self) -> bool {
        self.verdict != EvidenceVerdict::Fabricated
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FalsifierVerdict {
    Confirmed,
    NeedsHuman,
    Downgraded { to: Severity },
    Disproved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FalsifierSubmission {
    pub review_id: ReviewId,
    pub finding_id: FindingId,
    #[serde(flatten)]
    pub verdict: FalsifierVerdict,
    pub rationale: String,
    /// Disproved requires ≥1 and must pass G2, else downgraded to NeedsHuman.
    #[serde(default)]
    pub counter_evidence: Vec<EvidenceQuote>,
    /// Filled by the daemon from the caller identity when omitted.
    #[serde(default)]
    pub falsifier: AuthorRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    ReturnedG1,
    Fabricated,
    PendingFalsifier,
    Delivered,
    Folded,
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackLabel {
    Valid,
    FalsePositive,
    Wontfix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub label: FeedbackLabel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: String,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdjudicatedFinding {
    pub finding: ReviewFinding,
    #[serde(default)]
    pub g1: Vec<G1Issue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g2: Option<G2Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub falsifier: Option<FalsifierSubmission>,
    pub disposition: Disposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<FeedbackRecord>,
}

/// Stored at `<root>/.baton/reviews/<R-id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub v: u32,
    pub id: ReviewId,
    pub report: ReviewReport,
    #[serde(default)]
    pub report_g1: Vec<G1Issue>,
    #[serde(default)]
    pub findings: Vec<AdjudicatedFinding>,
    /// G1 return happens at most once.
    #[serde(default)]
    pub returned_once: bool,
    /// All PendingFalsifier resolved.
    #[serde(default)]
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrecisionStat {
    pub agent: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub depth: ReviewDepth,
    pub category: Category,
    pub delivered: u32,
    pub valid: u32,
    pub false_positive: u32,
    pub wontfix: u32,
    pub fabricated: u32,
    pub disproved: u32,
    /// valid / (valid + false_positive); None when the denominator is 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<f32>,
}

impl PrecisionStat {
    pub fn compute_precision(valid: u32, false_positive: u32) -> Option<f32> {
        let denom = valid + false_positive;
        (denom > 0).then(|| valid as f32 / denom as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_serde_and_caps() {
        assert_eq!(serde_json::to_string(&ReviewDepth::D2).unwrap(), "\"D2\"");
        assert_eq!(
            serde_json::from_str::<ReviewDepth>("\"D3\"").unwrap(),
            ReviewDepth::D3
        );
        assert_eq!(ReviewDepth::D0.max_findings(), 5);
        assert_eq!(ReviewDepth::D3.max_findings(), 10);
        assert_eq!(ReviewDepth::default(), ReviewDepth::D1);
        assert_eq!(ReviewDepth::parse("d2"), Some(ReviewDepth::D2));
    }

    #[test]
    fn severity_order() {
        assert!(Severity::Blocker < Severity::Major);
        assert!(Severity::Major.is_actionable());
        assert!(!Severity::Minor.is_actionable());
    }

    #[test]
    fn g2_verdict_rules() {
        assert_eq!(
            G2Outcome::from_issues(vec![]).verdict,
            EvidenceVerdict::Verified
        );
        let relocated = EvidenceIssue::QuoteRelocated {
            file: "a".into(),
            claimed: 1,
            actual: 40,
        };
        assert_eq!(
            G2Outcome::from_issues(vec![relocated.clone()]).verdict,
            EvidenceVerdict::Relocated
        );
        let o = G2Outcome::from_issues(vec![
            relocated,
            EvidenceIssue::SymbolNotFound { symbol: "x".into() },
        ]);
        assert_eq!(o.verdict, EvidenceVerdict::Fabricated);
        assert!(!o.accepted());
        let v = serde_json::to_value(&o.issues[1]).unwrap();
        assert_eq!(v["issue"], "symbol_not_found");
    }

    #[test]
    fn falsifier_flattened_shape() {
        let json = r#"{"review_id":"R-1","finding_id":"F-1","verdict":"downgraded","to":"minor","rationale":"guarded upstream"}"#;
        let s: FalsifierSubmission = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.verdict,
            FalsifierVerdict::Downgraded {
                to: Severity::Minor
            }
        );
        assert!(s.counter_evidence.is_empty());
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["verdict"], "downgraded");
        assert_eq!(back["to"], "minor");
        let c: FalsifierVerdict = serde_json::from_str(r#"{"verdict":"confirmed"}"#).unwrap();
        assert_eq!(c, FalsifierVerdict::Confirmed);
    }

    #[test]
    fn g1_ok_and_precision() {
        let mut r = G1Result::default();
        assert!(r.ok());
        r.findings.insert(FindingId::new("F-1"), vec![]);
        assert!(r.ok());
        r.findings
            .insert(FindingId::new("F-2"), vec![G1Issue::new("claim", "empty")]);
        assert!(!r.ok());
        assert_eq!(PrecisionStat::compute_precision(0, 0), None);
        assert_eq!(PrecisionStat::compute_precision(3, 1), Some(0.75));
    }
}
