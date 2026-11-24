//! Agent adapters for `claude`, `codex`, and `agy` (contracts §3).
//!
//! Provides hook normalization, non-invasive hook/MCP install & uninstall planning,
//! read-only doctor checks, and strictly interactive L4 spawn command construction.

pub mod doctor;
pub mod error;
pub mod install;
pub mod normalize;
pub mod spawn;

pub use doctor::{doctor_checks, resolve_agent_bin, DoctorEnv, DoctorFinding, Level};
pub use error::AdapterError;
pub use install::{
    apply_plan, hook_config_path, install_plan, uninstall_plan, FileChange, InstallOptions,
    InstallPlan, Scope,
};
pub use normalize::{normalize_hook, normalize_request};
pub use spawn::{
    build_l4_spawn_cmd, default_bypass_permission_flag, ensure_bypass_permission_arg,
    is_fixed_effort_agy_model, resolve_agy_model_id, SpawnRequest,
};

/// Supported agent names (kept for M0 compatibility).
pub const SUPPORTED_AGENTS: [&str; 3] = ["claude", "codex", "agy"];
