//! Shared protocol types and wire schemas for `starfane`.
//!
//! This crate is the single source of truth for every cross-crate type, the
//! UDS JSON-line wire protocol, the REST/SSE payloads and the MCP tool
//! schemas. See `docs/crate-contracts.md` §0–§1 (FROZEN v0.1).
//!
//! Dependency rule: only `serde` + `serde_json`. Never add tokio/axum/etc.

pub mod agent;
pub mod api;
pub mod artifact;
pub mod error;
pub mod event;
pub mod id;
pub mod mcp;
pub mod paths;
pub mod pipeline;
pub mod session;
pub mod sse;
pub mod wire;

pub use agent::*;
pub use api::*;
pub use artifact::handoff::*;
pub use artifact::plan::*;
pub use artifact::rca::*;
pub use artifact::report::*;
pub use artifact::review::*;
pub use artifact::triage::*;
pub use artifact::*;
pub use error::*;
pub use event::*;
pub use id::{
    ArtifactId, BindToken, EventId, FindingId, HandoffId, PaneId, PipelineId, ReviewId, SessionId,
};
pub use mcp::mcp_tool_schemas;
pub use pipeline::*;
pub use session::*;
pub use sse::*;
pub use wire::*;

/// Version carried in every top-level wire / artifact structure (`v` field).
pub const PROTO_VERSION: u32 = 1;

/// Legacy alias used by the M0 skeleton; prefer [`PROTO_VERSION`].
pub const PROTOCOL_VERSION: u32 = PROTO_VERSION;

/// Crate version string (used by `Ping` / `/api/health`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
