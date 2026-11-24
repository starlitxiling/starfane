//! Control plane daemon (`batond`): UDS ingest, session tree, playbook engine,
//! review gates, delivery ladder, Web REST/SSE, Feishu and MCP.
//!
//! Contracts: `docs/crate-contracts.md` §7.

pub mod client;
pub mod config;
pub mod core;
pub mod lark;
pub mod mcp;
pub mod notice;
pub mod store;
pub mod uds;
pub mod web;

pub use crate::config::{BatonConfig, TeamConfig};
pub use crate::core::Core;

use std::fmt;
use std::path::PathBuf;

pub const DAEMON_NAME: &str = "batond";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub config: BatonConfig,
    /// Dev hot reload: serve `web/` from disk instead of embedded assets.
    pub web_dir: Option<PathBuf>,
    pub foreground: bool,
}

#[derive(Debug)]
pub enum DaemonError {
    Io(std::io::Error),
    AlreadyRunning(String),
    Config(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::Io(e) => write!(f, "io error: {e}"),
            DaemonError::AlreadyRunning(m) => write!(f, "daemon already running: {m}"),
            DaemonError::Config(m) => write!(f, "config error: {m}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<std::io::Error> for DaemonError {
    fn from(e: std::io::Error) -> Self {
        DaemonError::Io(e)
    }
}

/// Run the daemon until `Shutdown` or SIGINT/SIGTERM.
pub async fn run(cfg: DaemonConfig) -> Result<(), DaemonError> {
    crate::core::run(cfg).await
}
