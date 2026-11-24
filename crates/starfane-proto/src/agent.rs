//! Agent kinds, reasoning effort and native multi-agent modes.

use crate::error::ProtoError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Agy,
}

impl AgentKind {
    pub const ALL: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Agy];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Agy => "agy",
        }
    }

    /// Case-insensitive parse of `claude` / `codex` / `agy`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "agy" => Some(AgentKind::Agy),
            _ => None,
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentKind {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AgentKind::parse(s).ok_or(ProtoError::InvalidValue {
            field: "agent",
            value: s.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMode {
    Swarm,
    Boost,
    Teamwork,
    Subagents,
}

impl NativeMode {
    /// Swarm / Boost / Teamwork belong to agy; Subagents is generic (None).
    pub fn owner(self) -> Option<AgentKind> {
        match self {
            NativeMode::Swarm | NativeMode::Boost | NativeMode::Teamwork => Some(AgentKind::Agy),
            NativeMode::Subagents => None,
        }
    }

    /// TUI slash command, if this mode is triggered by one.
    pub fn slash(self) -> Option<&'static str> {
        match self {
            NativeMode::Swarm => Some("/swarm"),
            NativeMode::Boost => Some("/boost"),
            NativeMode::Teamwork => Some("/teamwork"),
            NativeMode::Subagents => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NativeMode::Swarm => "swarm",
            NativeMode::Boost => "boost",
            NativeMode::Teamwork => "teamwork",
            NativeMode::Subagents => "subagents",
        }
    }

    /// Accepts `swarm` or `/swarm` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().trim_start_matches('/').to_ascii_lowercase();
        match t.as_str() {
            "swarm" => Some(NativeMode::Swarm),
            "boost" => Some(NativeMode::Boost),
            "teamwork" => Some(NativeMode::Teamwork),
            "subagents" | "subagent" => Some(NativeMode::Subagents),
            _ => None,
        }
    }
}

impl fmt::Display for NativeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_parse_and_serde() {
        assert_eq!(AgentKind::parse("CLAUDE"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::parse("gpt"), None);
        assert_eq!(serde_json::to_string(&AgentKind::Agy).unwrap(), "\"agy\"");
        assert_eq!("codex".parse::<AgentKind>().unwrap(), AgentKind::Codex);
        for a in AgentKind::ALL {
            assert_eq!(AgentKind::parse(a.as_str()), Some(a));
        }
    }

    #[test]
    fn native_owner_and_slash() {
        assert_eq!(NativeMode::Swarm.owner(), Some(AgentKind::Agy));
        assert_eq!(NativeMode::Subagents.owner(), None);
        assert_eq!(NativeMode::Boost.slash(), Some("/boost"));
        assert_eq!(NativeMode::parse("/teamwork"), Some(NativeMode::Teamwork));
        assert_eq!(Effort::parse("XHIGH"), Some(Effort::Xhigh));
    }
}
