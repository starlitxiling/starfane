//! Agent adapters for `claude`, `codex`, and `agy` (contracts §3).
//!
//! Provides hook normalization, non-invasive hook/MCP install & uninstall planning,
//! read-only doctor checks, and strictly interactive L4 spawn command construction.

pub mod doctor;
pub mod error;
pub mod install;
pub mod normalize;
pub mod spawn;
pub mod trust;

pub use doctor::{doctor_checks, resolve_agent_bin, DoctorEnv, DoctorFinding, Level};
pub use error::AdapterError;
pub use install::{
    apply_plan, hook_config_path, install_plan, uninstall_plan, FileChange, InstallOptions,
    InstallPlan, Scope,
};
pub use normalize::{normalize_hook, normalize_request};
pub use spawn::{
    build_l4_spawn_cmd, build_resume_cmd, build_tmux_suspend_waiter_script,
    default_bypass_permission_flag, ensure_bypass_permission_arg, is_fixed_effort_agy_model,
    resolve_agy_model_id, shell_single_quote, ResumeRequest, SpawnRequest,
};
pub use trust::{
    agy_trust_settings_json, codex_mcp_env_vars_override_from_toml, codex_trust_override_arg,
    ensure_ancestor_trust_args, ensure_ancestor_trust_args_remote, ensure_codex_mcp_env_vars_arg,
    evaluate_agy_trust, evaluate_agy_trust_from_json, evaluate_agy_trust_remote,
    evaluate_claude_trust, evaluate_claude_trust_from_json, evaluate_codex_trust,
    evaluate_codex_trust_from_toml, evaluate_codex_trust_remote, FolderTrustDecision,
    CODEX_MCP_ENV_VARS,
};

/// Supported agent names (kept for M0 compatibility).
pub const SUPPORTED_AGENTS: [&str; 3] = ["claude", "codex", "agy"];
