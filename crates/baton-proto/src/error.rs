//! Error type for parsing protocol values (shorthands, targets, enums).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// `StageBinding` shorthand could not be parsed.
    InvalidShorthand { input: String, reason: String },
    /// `SessionTarget` string is not `auto` / `spawn` / `%N` / `session:<id>`.
    InvalidSessionTarget(String),
    /// Generic enum / value parse failure.
    InvalidValue { field: &'static str, value: String },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::InvalidShorthand { input, reason } => {
                write!(f, "invalid binding shorthand {input:?}: {reason}")
            }
            ProtoError::InvalidSessionTarget(s) => write!(
                f,
                "invalid session target {s:?} (expected auto | spawn | %<pane> | session:<id>)"
            ),
            ProtoError::InvalidValue { field, value } => {
                write!(f, "invalid value {value:?} for {field}")
            }
        }
    }
}

impl std::error::Error for ProtoError {}
