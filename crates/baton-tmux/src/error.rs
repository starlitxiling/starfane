//! Error type for the tmux layer (hand-written, no thiserror).

use std::fmt;

#[derive(Debug)]
pub enum TmuxError {
    /// The tmux binary could not be spawned / waited on / written to.
    Io {
        what: String,
        source: std::io::Error,
    },
    /// tmux ran but exited non-zero.
    Command {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    /// tmux did not finish within the per-command timeout and was killed.
    Timeout { args: String, secs: u64 },
    /// tmux output could not be parsed.
    Parse(String),
    /// Text rejected by `inject_pointer` (multi-line, control chars, too long,
    /// bare slash command, empty).
    InvalidText(String),
    /// A request argument was invalid (empty argv, bad env key, bad session…).
    InvalidRequest(String),
}

impl TmuxError {
    /// True when the failure means "no tmux server is running on this socket".
    pub fn is_no_server(&self) -> bool {
        match self {
            TmuxError::Command { stderr, .. } => stderr_means_no_server(stderr),
            _ => false,
        }
    }
}

pub(crate) fn stderr_means_no_server(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no server running")
        || s.contains("error connecting to")
        || s.contains("failed to connect to server")
        || s.contains("no sessions")
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TmuxError::Io { what, source } => write!(f, "tmux io error ({what}): {source}"),
            TmuxError::Command { args, code, stderr } => {
                let code = code.map_or_else(|| "signal".to_string(), |c| c.to_string());
                write!(f, "tmux {args} failed (exit {code}): {}", stderr.trim())
            }
            TmuxError::Timeout { args, secs } => {
                write!(f, "tmux {args} timed out after {secs}s")
            }
            TmuxError::Parse(m) => write!(f, "tmux output parse error: {m}"),
            TmuxError::InvalidText(m) => write!(f, "invalid injection text: {m}"),
            TmuxError::InvalidRequest(m) => write!(f, "invalid tmux request: {m}"),
        }
    }
}

impl std::error::Error for TmuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TmuxError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
