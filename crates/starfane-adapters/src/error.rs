//! Error types for `starfane-adapters` (handwritten, zero anyhow/thiserror).

use starfane_proto::AgentKind;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum AdapterError {
    MissingField {
        agent: AgentKind,
        field: &'static str,
    },
    InvalidInput(String),
    Io {
        path: Option<PathBuf>,
        source: std::io::Error,
    },
    Json {
        path: Option<PathBuf>,
        message: String,
    },
    Toml {
        path: Option<PathBuf>,
        message: String,
    },
    CommandFailed {
        cmd: String,
        status: Option<i32>,
        stderr: String,
    },
}

impl AdapterError {
    pub fn io(path: impl Into<Option<PathBuf>>, source: std::io::Error) -> Self {
        AdapterError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn json(path: impl Into<Option<PathBuf>>, err: impl fmt::Display) -> Self {
        AdapterError::Json {
            path: path.into(),
            message: err.to_string(),
        }
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterError::MissingField { agent, field } => {
                write!(f, "{agent} hook payload missing required field `{field}`")
            }
            AdapterError::InvalidInput(msg) => write!(f, "invalid adapter input: {msg}"),
            AdapterError::Io {
                path: Some(p),
                source,
            } => write!(f, "I/O error at {}: {source}", p.display()),
            AdapterError::Io { path: None, source } => write!(f, "I/O error: {source}"),
            AdapterError::Json {
                path: Some(p),
                message,
            } => write!(f, "JSON error at {}: {message}", p.display()),
            AdapterError::Json {
                path: None,
                message,
            } => write!(f, "JSON error: {message}"),
            AdapterError::Toml {
                path: Some(p),
                message,
            } => write!(f, "TOML error at {}: {message}", p.display()),
            AdapterError::Toml {
                path: None,
                message,
            } => write!(f, "TOML error: {message}"),
            AdapterError::CommandFailed {
                cmd,
                status,
                stderr,
            } => write!(
                f,
                "command `{cmd}` failed (status={status:?}): {}",
                stderr.trim()
            ),
        }
    }
}

impl std::error::Error for AdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AdapterError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
