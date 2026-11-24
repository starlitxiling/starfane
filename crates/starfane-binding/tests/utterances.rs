//! Table-driven utterance test suite for `starfane-binding` (contracts §6 & §10).

use serde::Deserialize;
use starfane_binding::{
    apply_defaults, format_shorthand, parse_binding_utterance, parse_shorthand, validate,
    BindingContext, BindingWarning, PlaybookDefaults,
};
use starfane_proto::{AgentKind, PlaybookKind, ReviewDepth, StageKind};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    text: String,
    playbook: String,
    #[serde(default)]
    default_playbook: Option<String>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    use_defaults: bool,
    #[serde(default)]
    expect: BTreeMap<String, String>,
    #[serde(default)]
    resolved: Option<BTreeMap<String, String>>,
    #[serde(default)]
    warnings: Vec<String>,
}

fn warning_tag(w: &BindingWarning) -> &'static str {
    match w {
        BindingWarning::SameModelReviewer { .. } => "same_model_reviewer",
        BindingWarning::NativeAgentMismatch { .. } => "native_agent_mismatch",
        BindingWarning::UnknownModel { .. } => "unknown_model",
        BindingWarning::StageNotInPlaybook { .. } => "stage_not_in_playbook",
        BindingWarning::Unrecognized { .. } => "unrecognized",
    }
}

fn test_context() -> BindingContext {
    let mut aliases = BTreeMap::new();
    aliases.insert("opus".into(), "claude:opus".into());
    aliases.insert("sonnet".into(), "claude:sonnet".into());
    aliases.insert("neon".into(), "agy:gemini-4-neon-high".into());

    let mut known_models = BTreeMap::new();
    known_models.insert(
        AgentKind::Claude,
        vec![
            "opus".into(),
            "sonnet".into(),
            "haiku".into(),
            "claude-opus-4".into(),
        ],
    );
    known_models.insert(
        AgentKind::Agy,
        vec!["gemini-4-neon-high".into(), "gemini-2.5-pro".into()],
    );

    BindingContext {
        aliases,
        known_models,
    }
}

#[test]
fn table_driven_utterances_corpus_all_pass() {
    let raw = include_str!("utterances.toml");
    let corpus: Corpus = toml::from_str(raw).expect("parse utterances.toml");
    assert!(
        corpus.cases.len() >= 35,
        "expected >= 35 utterance cases, got {}",
        corpus.cases.len()
    );

    let ctx = test_context();
    let defaults = PlaybookDefaults::builtin();

    for case in &corpus.cases {
        let default_pb = case
            .default_playbook
            .as_deref()
            .map(|s| PlaybookKind::parse(s).expect("valid default_playbook"));

        let mut parsed = parse_binding_utterance(&case.text, default_pb, &ctx);
        if case.use_defaults {
            apply_defaults(&mut parsed, &defaults);
            // Re-validate after applying defaults while preserving parse-time Unrecognized warnings.
            let mut combined: Vec<BindingWarning> = parsed
                .warnings
                .iter()
                .filter(|w| matches!(w, BindingWarning::Unrecognized { .. }))
                .cloned()
                .collect();
            combined.extend(validate(&parsed, &ctx));
            parsed.warnings = combined;
        }

        let expected_pb = PlaybookKind::parse(&case.playbook).expect("valid expected playbook");
        assert_eq!(
            parsed.playbook, expected_pb,
            "[{}] playbook mismatch for {:?}",
            case.name, case.text
        );

        assert_eq!(
            parsed.goal, case.goal,
            "[{}] goal mismatch for {:?}",
            case.name, case.text
        );

        let actual_expect: BTreeMap<String, String> = parsed
            .bindings
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_shorthand()))
            .collect();
        assert_eq!(
            actual_expect, case.expect,
            "[{}] explicit bindings mismatch for {:?}",
            case.name, case.text
        );

        if let Some(expected_resolved) = &case.resolved {
            let actual_resolved: BTreeMap<String, String> = parsed
                .resolved
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_shorthand()))
                .collect();
            assert_eq!(
                &actual_resolved, expected_resolved,
                "[{}] resolved bindings mismatch for {:?}",
                case.name, case.text
            );
        }

        let actual_warnings: Vec<&str> = parsed.warnings.iter().map(warning_tag).collect();
        let expected_warnings: Vec<&str> = case.warnings.iter().map(String::as_str).collect();
        assert_eq!(
            actual_warnings, expected_warnings,
            "[{}] warnings mismatch (full: {:?}) for {:?}",
            case.name, parsed.warnings, case.text
        );
    }
}

#[test]
fn team_toml_parsing_and_defaults_application() {
    let toml_src = r#"
[playbooks.feature]
plan = "claude:opus@high"
implement = "codex"
test_review = "agy /swarm"
report = "same_as:plan"
review_depth = "D2"

[playbooks.debug]
triage = "codex"
deep = "claude:opus@high"
triage_timebox = "10m"
auto_deep = true

[models.aliases]
opus = "claude:opus"
sonnet = "claude:sonnet"
neon = "agy:gemini-4-neon-high"
"#;

    let (defaults, ctx) = PlaybookDefaults::from_team_toml(toml_src).expect("parse team.toml");
    assert_eq!(
        defaults.feature[&StageKind::TestReview].depth,
        Some(ReviewDepth::D2)
    );
    assert_eq!(
        ctx.aliases.get("neon").map(String::as_str),
        Some("agy:gemini-4-neon-high")
    );

    let mut parsed = parse_binding_utterance("计划用 neon，实现用 codex", None, &ctx);
    apply_defaults(&mut parsed, &defaults);
    assert_eq!(
        parsed.resolved[&StageKind::Plan].to_shorthand(),
        "agy:gemini-4-neon-high"
    );
    assert_eq!(
        parsed.resolved[&StageKind::Report].to_shorthand(),
        "agy:gemini-4-neon-high"
    );
    assert_eq!(
        parsed.resolved[&StageKind::TestReview].to_shorthand(),
        "agy /swarm"
    );
    assert!(validate(&parsed, &ctx).is_empty());
}

#[test]
fn shorthand_wrapper_and_review_depth_parsing() {
    let b = parse_shorthand("agy:gemini-4-neon-high@high /swarm").expect("valid shorthand");
    assert_eq!(format_shorthand(&b), "agy:gemini-4-neon-high@high /swarm");
    assert!(parse_shorthand("bad_agent:x").is_err());

    let parsed = parse_binding_utterance(
        "实现用 codex，审查用 agy /swarm D3",
        None,
        &BindingContext::default(),
    );
    assert_eq!(
        parsed.bindings[&StageKind::TestReview].depth,
        Some(ReviewDepth::D3)
    );
    assert_eq!(parsed.bindings[&StageKind::Implement].depth, None);
}

#[test]
fn empty_input_and_serde_roundtrip() {
    let ctx = BindingContext::default();
    let empty = parse_binding_utterance("   ", None, &ctx);
    assert_eq!(empty.playbook, PlaybookKind::Feature);
    assert!(!empty.playbook_explicit);
    assert!(empty.goal.is_none());
    assert!(empty.bindings.is_empty());
    assert!(empty.warnings.is_empty());

    let parsed = parse_binding_utterance("实现用 codex /swarm，审查用 codex", None, &ctx);
    let toml_str = toml::to_string(&parsed).expect("serialize ParsedBinding");
    let roundtrip: starfane_binding::ParsedBinding =
        toml::from_str(&toml_str).expect("deserialize ParsedBinding");
    assert_eq!(parsed, roundtrip);
    assert!(parsed.warnings.iter().any(|w| matches!(
        w,
        BindingWarning::NativeAgentMismatch {
            stage: StageKind::Implement,
            ..
        }
    )));
    assert!(parsed
        .warnings
        .iter()
        .any(|w| matches!(w, BindingWarning::SameModelReviewer { .. })));
}

#[test]
fn same_as_cycle_does_not_hang() {
    let mut defaults = PlaybookDefaults::default();
    defaults
        .feature
        .insert(StageKind::Plan, parse_shorthand("same_as:report").unwrap());
    defaults
        .feature
        .insert(StageKind::Report, parse_shorthand("same_as:plan").unwrap());
    let mut parsed = parse_binding_utterance("测试一下", None, &BindingContext::default());
    apply_defaults(&mut parsed, &defaults);
    assert_eq!(
        parsed.resolved[&StageKind::Plan].same_as,
        Some(StageKind::Report)
    );
    assert_eq!(
        parsed.resolved[&StageKind::Report].same_as,
        Some(StageKind::Plan)
    );
}
