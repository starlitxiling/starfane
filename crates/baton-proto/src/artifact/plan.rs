//! `plan.v1`

use crate::agent::NativeMode;
use crate::artifact::ArtifactHeader;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDoc {
    /// May be omitted on submission; the daemon overwrites it.
    #[serde(default)]
    pub header: ArtifactHeader,
    pub goal: String,
    pub design_md: String,
    /// G1: ≥1
    pub tasks: Vec<PlanTask>,
    /// G1: ≥1
    pub acceptance: Vec<String>,
    /// G1: ≥1
    pub test_plan: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_strategy: Option<NativeMode>,
}
