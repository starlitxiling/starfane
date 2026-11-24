//! Public data structures for `baton-binding` (contracts §6).

use crate::error::BindingError;
use baton_proto::{AgentKind, NativeMode, PlaybookKind, ReviewDepth, StageBinding, StageKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// External context supplied to the binding resolver (aliases and known models).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingContext {
    /// Model / shorthand aliases, e.g. `"opus" => "claude:opus"`, `"neon" => "agy:gemini-4-neon-high"`.
    pub aliases: BTreeMap<String, String>,
    /// Cached model catalog per agent (e.g. from `baton doctor` / `agy models`).
    pub known_models: BTreeMap<AgentKind, Vec<String>>,
}

/// Default per-stage bindings for Feature and Debug playbooks (loaded from `team.toml`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaybookDefaults {
    pub feature: BTreeMap<StageKind, StageBinding>,
    pub debug: BTreeMap<StageKind, StageBinding>,
}

impl PlaybookDefaults {
    /// Standard built-in defaults matching `architecture.md` §3.4.
    pub fn builtin() -> Self {
        let mut feature = BTreeMap::new();
        feature.insert(
            StageKind::Plan,
            StageBinding::from_shorthand("claude:opus@high").expect("valid builtin"),
        );
        feature.insert(
            StageKind::Implement,
            StageBinding::from_shorthand("codex").expect("valid builtin"),
        );
        let mut review = StageBinding::from_shorthand("agy /swarm").expect("valid builtin");
        review.depth = Some(ReviewDepth::D1);
        feature.insert(StageKind::TestReview, review);
        feature.insert(
            StageKind::Report,
            StageBinding::from_shorthand("same_as:plan").expect("valid builtin"),
        );

        let mut debug = BTreeMap::new();
        debug.insert(
            StageKind::Triage,
            StageBinding::from_shorthand("codex").expect("valid builtin"),
        );
        debug.insert(
            StageKind::Deep,
            StageBinding::from_shorthand("claude:opus@high").expect("valid builtin"),
        );

        PlaybookDefaults { feature, debug }
    }

    /// Parse `[playbooks.feature]`, `[playbooks.debug]`, and `[models.aliases]`
    /// from a project's `.baton/team.toml` (architecture §3.4).
    pub fn from_team_toml(src: &str) -> Result<(PlaybookDefaults, BindingContext), BindingError> {
        let doc: TeamToml =
            toml::from_str(src).map_err(|e| BindingError::TomlParse(e.to_string()))?;

        let mut defaults = PlaybookDefaults::default();

        if let Some(playbooks) = doc.playbooks {
            let mut feature_depth: Option<ReviewDepth> = None;
            for (k, v) in playbooks.feature {
                if k == "review_depth" {
                    if let Some(s) = v.as_str() {
                        feature_depth = ReviewDepth::parse(s);
                    }
                    continue;
                }
                let Some(s) = v.as_str() else {
                    continue;
                };
                let stage = StageKind::parse(&k).ok_or_else(|| BindingError::UnknownStage {
                    playbook: "feature",
                    stage: k.clone(),
                })?;
                let binding = StageBinding::from_shorthand(s).map_err(|source| {
                    BindingError::InvalidShorthand {
                        key: format!("playbooks.feature.{k}"),
                        source,
                    }
                })?;
                defaults.feature.insert(stage, binding);
            }
            if let (Some(depth), Some(rev)) = (
                feature_depth,
                defaults.feature.get_mut(&StageKind::TestReview),
            ) {
                rev.depth = Some(depth);
            }

            for (k, v) in playbooks.debug {
                if k == "triage_timebox" || k == "auto_deep" {
                    continue;
                }
                let Some(s) = v.as_str() else {
                    continue;
                };
                let stage = StageKind::parse(&k).ok_or_else(|| BindingError::UnknownStage {
                    playbook: "debug",
                    stage: k.clone(),
                })?;
                let binding = StageBinding::from_shorthand(s).map_err(|source| {
                    BindingError::InvalidShorthand {
                        key: format!("playbooks.debug.{k}"),
                        source,
                    }
                })?;
                defaults.debug.insert(stage, binding);
            }
        }

        let mut ctx = BindingContext::default();
        if let Some(models) = doc.models {
            for (alias, shorthand) in models.aliases {
                StageBinding::from_shorthand(&shorthand).map_err(|source| {
                    BindingError::InvalidShorthand {
                        key: format!("models.aliases.{alias}"),
                        source,
                    }
                })?;
                ctx.aliases.insert(alias, shorthand);
            }
        }

        Ok((defaults, ctx))
    }
}

#[derive(Debug, Deserialize)]
struct TeamToml {
    playbooks: Option<TeamPlaybooksToml>,
    models: Option<TeamModelsToml>,
}

#[derive(Debug, Deserialize)]
struct TeamPlaybooksToml {
    #[serde(default)]
    feature: BTreeMap<String, toml::Value>,
    #[serde(default)]
    debug: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct TeamModelsToml {
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

/// Warnings emitted during binding parsing or validation (contracts §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "warning", rename_all = "snake_case")]
pub enum BindingWarning {
    /// `TestReview` and `Implement` resolve to the same agent and model.
    SameModelReviewer { model: String },
    /// Native mode does not belong to the assigned agent (e.g. `codex /swarm`).
    NativeAgentMismatch {
        stage: StageKind,
        native: NativeMode,
        agent: AgentKind,
    },
    /// Model is not in `ctx.known_models[agent]` (when the catalog is non-empty).
    UnknownModel {
        stage: StageKind,
        agent: AgentKind,
        model: String,
    },
    /// Stage mentioned in the utterance does not belong to the chosen playbook.
    StageNotInPlaybook {
        stage: StageKind,
        playbook: PlaybookKind,
    },
    /// Clause could not be paired into a stage binding or goal.
    Unrecognized { text: String },
}

/// Output of parsing a natural-language binding utterance (contracts §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedBinding {
    pub playbook: PlaybookKind,
    pub playbook_explicit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Stages explicitly bound by the utterance (including expanded rest/all clauses).
    #[serde(default)]
    pub bindings: BTreeMap<StageKind, StageBinding>,
    /// Complete playbook bindings after applying defaults and expanding `same_as`.
    #[serde(default)]
    pub resolved: BTreeMap<StageKind, StageBinding>,
    #[serde(default)]
    pub warnings: Vec<BindingWarning>,
}
