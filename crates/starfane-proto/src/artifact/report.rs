//! `report.v1` — final human-facing summary.

use crate::artifact::ArtifactHeader;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalReport {
    #[serde(default)]
    pub header: ArtifactHeader,
    pub summary_md: String,
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub follow_ups: Vec<String>,
    /// Where the daemon wrote it; default `docs/changes/<P-id>.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_path: Option<String>,
}
