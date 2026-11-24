//! Deterministic G1/G2 evidence verifier for `review.v1`, `triage.v1`, `rca.v1`,
//! `plan.v1`, `handoff.v1`, `report.v1`, and G3 falsifier submissions (contracts §5).

pub mod error;
pub mod g1;
pub mod g2;
pub mod source;

pub use error::{Error, EvidenceError};
pub use g1::{
    validate_g1_falsifier, validate_g1_handoff, validate_g1_plan, validate_g1_rca,
    validate_g1_report, validate_g1_review, validate_g1_triage, DEFAULT_MIN_QUOTE_CHARS,
};
pub use g2::{
    normalize_ws, verify_g2_evidence, verify_g2_log_refs, verify_g2_quotes, verify_g2_review,
    G2Config,
};
pub use source::{GitSource, SourceProvider};

/// Default allowed line drift tolerance (in lines).
pub const MAX_LINE_DRIFT: usize = 3;
