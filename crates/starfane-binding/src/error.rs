//! Error type for `starfane-binding` (no anyhow/thiserror per contracts §0).

use starfane_proto::ProtoError;
use std::fmt;

/// Alias matching the global convention (`crate::Error`).
pub type Error = BindingError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// Failed to parse `team.toml` configuration.
    TomlParse(String),
    /// Invalid shorthand inside `team.toml` or alias map.
    InvalidShorthand { key: String, source: ProtoError },
    /// Unknown stage name inside `team.toml`.
    UnknownStage {
        playbook: &'static str,
        stage: String,
    },
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindingError::TomlParse(msg) => write!(f, "failed to parse team.toml: {msg}"),
            BindingError::InvalidShorthand { key, source } => {
                write!(f, "invalid binding shorthand for {key:?}: {source}")
            }
            BindingError::UnknownStage { playbook, stage } => {
                write!(f, "unknown stage {stage:?} in [playbooks.{playbook}]")
            }
        }
    }
}

impl std::error::Error for BindingError {}
