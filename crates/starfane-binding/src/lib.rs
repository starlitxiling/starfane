//! Deterministic natural-language stage-to-agent/model binding resolver for `pipeline.v1`
//! (contracts §6).
//!
//! Zero LLM calls, zero `regex` crate.

pub mod error;
pub mod parser;
pub mod types;
mod vocab;

pub use error::{BindingError, Error};
pub use parser::{apply_defaults, parse_binding_utterance, validate};
pub use starfane_proto::{ProtoError, StageBinding};
pub use types::{BindingContext, BindingWarning, ParsedBinding, PlaybookDefaults};

/// Default playbook name (`"feature"`).
pub const DEFAULT_PLAYBOOK: &str = "feature";

/// Parse `<agent>[:<model>][@<effort>][ /<native>]` or `same_as:<stage>`.
pub fn parse_shorthand(s: &str) -> Result<StageBinding, ProtoError> {
    StageBinding::from_shorthand(s)
}

/// Format a [`StageBinding`] back into canonical shorthand syntax.
pub fn format_shorthand(binding: &StageBinding) -> String {
    binding.to_shorthand()
}
