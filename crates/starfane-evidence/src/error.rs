//! Error type for `starfane-evidence` (no anyhow/thiserror per contracts §0).

use std::fmt;

/// Alias matching the global convention (`crate::Error`).
pub type Error = EvidenceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    /// Unsafe or invalid repo-relative path (e.g. `..`, leading `/`, leading `-`, NUL).
    InvalidPath { path: String, reason: &'static str },
    /// Unsafe or invalid git commit reference.
    InvalidCommit {
        commit: String,
        reason: &'static str,
    },
    /// Git subprocess failed or repository is invalid.
    GitFailed { command: String, stderr: String },
    /// Underlying I/O error (e.g. spawning `git` or reading a file).
    Io { context: String, message: String },
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvidenceError::InvalidPath { path, reason } => {
                write!(f, "invalid evidence path {path:?}: {reason}")
            }
            EvidenceError::InvalidCommit { commit, reason } => {
                write!(f, "invalid git commit {commit:?}: {reason}")
            }
            EvidenceError::GitFailed { command, stderr } => {
                write!(f, "git command failed ({command}): {stderr}")
            }
            EvidenceError::Io { context, message } => {
                write!(f, "I/O error ({context}): {message}")
            }
        }
    }
}

impl std::error::Error for EvidenceError {}
