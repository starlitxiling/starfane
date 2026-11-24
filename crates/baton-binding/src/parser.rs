//! Natural-language binding parser, defaults application, and validator (contracts §6).

use crate::types::{BindingContext, BindingWarning, ParsedBinding, PlaybookDefaults};
use crate::vocab::{
    detect_playbook_from_text, extract_executor, extract_stages, has_all_stage_keyword,
    has_binding_verb, has_rest_keyword, strip_playbook_prefix,
};
use baton_proto::{PlaybookKind, StageBinding, StageKind};
use std::collections::BTreeMap;

/// Parse a natural Chinese/English utterance into a [`ParsedBinding`] (contracts §6).
///
/// Pure, deterministic, zero-LLM, zero-regex. Call [`apply_defaults`] afterwards to merge
/// `team.toml` defaults and expand `same_as` chains.
pub fn parse_binding_utterance(
    text: &str,
    default_playbook: Option<PlaybookKind>,
    ctx: &BindingContext,
) -> ParsedBinding {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    let (playbook, playbook_explicit) = match default_playbook {
        Some(pb) => (pb, true),
        None => detect_playbook_from_text(&lower),
    };

    if trimmed.is_empty() {
        return ParsedBinding {
            playbook,
            playbook_explicit,
            goal: None,
            bindings: BTreeMap::new(),
            resolved: BTreeMap::new(),
            warnings: Vec::new(),
        };
    }

    let clauses = split_clauses(trimmed, ctx);

    let mut goal_parts: Vec<String> = Vec::new();
    let mut saw_binding = false;
    let mut bindings: BTreeMap<StageKind, StageBinding> = BTreeMap::new();
    let mut rest_binding: Option<StageBinding> = None;
    let mut all_binding: Option<StageBinding> = None;
    let mut last_bound_stage: Option<StageKind> = None;
    let mut parse_warnings: Vec<BindingWarning> = Vec::new();

    for (idx, raw_clause) in clauses.iter().enumerate() {
        let clause = raw_clause.trim();
        if clause.is_empty() {
            continue;
        }
        let clause_lower = clause.to_lowercase();

        // Check Chinese `same_as` shorthand phrases like `报告同计划`, `总结沿用规划`, `汇报同上`
        if let Some((stages, same_as_target)) =
            try_parse_cn_same_as(clause, &clause_lower, last_bound_stage)
        {
            saw_binding = true;
            for st in stages {
                bindings.insert(
                    st,
                    StageBinding {
                        same_as: Some(same_as_target),
                        ..Default::default()
                    },
                );
                last_bound_stage = Some(st);
            }
            continue;
        }

        let executor = extract_executor(clause, ctx);
        let stage_search_text = strip_same_as_tokens(&clause_lower);
        let stages = extract_stages(&stage_search_text);
        let is_rest = has_rest_keyword(&stage_search_text);
        let is_all = has_all_stage_keyword(&stage_search_text);

        match (stages.is_empty(), executor) {
            // S non-empty and X non-empty -> assign X to each stage in S
            (false, Some(exec)) => {
                saw_binding = true;
                for st in stages {
                    let mut b = exec.clone();
                    if st != StageKind::TestReview {
                        b.depth = None;
                    }
                    bindings.insert(st, b);
                    last_bound_stage = Some(st);
                }
            }
            // S empty, X non-empty -> check `其余` (rest) or `全部` (all)
            (true, Some(exec)) => {
                if is_rest {
                    saw_binding = true;
                    rest_binding = Some(exec);
                } else if is_all {
                    saw_binding = true;
                    all_binding = Some(exec);
                } else {
                    parse_warnings.push(BindingWarning::Unrecognized {
                        text: clause.to_string(),
                    });
                }
            }
            // X empty -> either leading goal text, or an unrecognized clause
            (_, None) => {
                let looks_like_failed_binding =
                    (!stages.is_empty() || is_rest || is_all) && has_binding_verb(&clause_lower);

                if !saw_binding && !looks_like_failed_binding {
                    let cleaned = if idx == 0 {
                        strip_playbook_prefix(clause)
                    } else {
                        clause
                    };
                    if !cleaned.is_empty() {
                        goal_parts.push(cleaned.to_string());
                    }
                } else {
                    parse_warnings.push(BindingWarning::Unrecognized {
                        text: clause.to_string(),
                    });
                }
            }
        }
    }

    // Expand `全部` (all) before `其余` (rest), never overwriting explicit stage bindings.
    if let Some(exec) = all_binding {
        for &st in playbook.stages() {
            bindings.entry(st).or_insert_with(|| {
                let mut b = exec.clone();
                if st != StageKind::TestReview {
                    b.depth = None;
                }
                b
            });
        }
    }

    if let Some(exec) = rest_binding {
        for &st in playbook.stages() {
            bindings.entry(st).or_insert_with(|| {
                let mut b = exec.clone();
                if st != StageKind::TestReview {
                    b.depth = None;
                }
                b
            });
        }
    }

    let goal = if goal_parts.is_empty() {
        None
    } else {
        Some(goal_parts.join("，"))
    };

    let mut resolved = BTreeMap::new();
    for &st in playbook.stages() {
        if let Some(b) = bindings.get(&st) {
            resolved.insert(st, b.clone());
        }
    }
    expand_same_as(&mut resolved);

    let mut parsed = ParsedBinding {
        playbook,
        playbook_explicit,
        goal,
        bindings,
        resolved,
        warnings: parse_warnings,
    };

    let validation_warnings = validate(&parsed, ctx);
    parsed.warnings.extend(validation_warnings);
    parsed
}

/// Fill missing playbook stages from `defaults` and expand all `same_as` chains in `parsed.resolved`
/// (contracts §6).
pub fn apply_defaults(parsed: &mut ParsedBinding, defaults: &PlaybookDefaults) {
    let default_map = match parsed.playbook {
        PlaybookKind::Feature => &defaults.feature,
        PlaybookKind::Debug => &defaults.debug,
    };

    let mut resolved = BTreeMap::new();
    for &stage in parsed.playbook.stages() {
        if let Some(explicit) = parsed.bindings.get(&stage) {
            resolved.insert(stage, explicit.clone());
        } else if let Some(def) = default_map.get(&stage) {
            resolved.insert(stage, def.clone());
        }
    }

    expand_same_as(&mut resolved);
    parsed.resolved = resolved;
}

/// Validate semantic consistency of `parsed` against `ctx` (contracts §6).
///
/// Checks:
/// - `StageNotInPlaybook`
/// - `NativeAgentMismatch`
/// - `UnknownModel`
/// - `SameModelReviewer`
pub fn validate(parsed: &ParsedBinding, ctx: &BindingContext) -> Vec<BindingWarning> {
    let mut warnings = Vec::new();
    let playbook_stages = parsed.playbook.stages();

    // 1. StageNotInPlaybook (checked on explicit bindings)
    for &stage in parsed.bindings.keys() {
        if !playbook_stages.contains(&stage) {
            warnings.push(BindingWarning::StageNotInPlaybook {
                stage,
                playbook: parsed.playbook,
            });
        }
    }

    // Build effective map for validation: resolved stages + any out-of-playbook explicit bindings.
    let mut effective: BTreeMap<StageKind, StageBinding> = parsed.resolved.clone();
    for (&stage, binding) in &parsed.bindings {
        effective.entry(stage).or_insert_with(|| binding.clone());
    }
    expand_same_as(&mut effective);

    // 2. NativeAgentMismatch & 3. UnknownModel
    for (&stage, b) in &effective {
        if let (Some(native), Some(agent)) = (b.native, b.agent) {
            if let Some(owner) = native.owner() {
                if owner != agent {
                    warnings.push(BindingWarning::NativeAgentMismatch {
                        stage,
                        native,
                        agent,
                    });
                }
            }
        }

        if let (Some(agent), Some(model)) = (b.agent, &b.model) {
            if let Some(known) = ctx.known_models.get(&agent) {
                if !known.is_empty() && !known.iter().any(|k| k.eq_ignore_ascii_case(model)) {
                    warnings.push(BindingWarning::UnknownModel {
                        stage,
                        agent,
                        model: model.clone(),
                    });
                }
            }
        }
    }

    // 4. SameModelReviewer (Feature playbook: Implement vs TestReview)
    if parsed.playbook == PlaybookKind::Feature {
        if let (Some(imp), Some(rev)) = (
            effective.get(&StageKind::Implement),
            effective.get(&StageKind::TestReview),
        ) {
            if let (Some(imp_agent), Some(rev_agent)) = (imp.agent, rev.agent) {
                let same_model = match (&imp.model, &rev.model) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => true,
                    _ => false,
                };
                if imp_agent == rev_agent && same_model {
                    let model_label = match &rev.model {
                        Some(m) => format!("{}:{}", rev_agent.as_str(), m),
                        None => rev_agent.as_str().to_string(),
                    };
                    warnings.push(BindingWarning::SameModelReviewer { model: model_label });
                }
            }
        }
    }

    warnings
}

/// Expand `same_as` entries inside `map` (following chains up to 6 hops to guard against cycles).
fn expand_same_as(map: &mut BTreeMap<StageKind, StageBinding>) {
    let snapshot = map.clone();
    for (stage, binding) in map.iter_mut() {
        let Some(mut target) = binding.same_as else {
            continue;
        };
        let mut hops = 0;
        let mut resolved_binding: Option<StageBinding> = None;
        while hops < 6 {
            hops += 1;
            if target == *stage {
                break;
            }
            let Some(next) = snapshot.get(&target) else {
                break;
            };
            if let Some(chained) = next.same_as {
                target = chained;
            } else {
                resolved_binding = Some(next.clone());
                break;
            }
        }
        if let Some(mut resolved) = resolved_binding {
            // Preserve any stage-specific depth override if present on the referencing stage.
            if binding.depth.is_some() {
                resolved.depth = binding.depth;
            }
            resolved.same_as = None;
            *binding = resolved;
        }
    }
}

/// Split utterance into clauses on `，,；;。\n`, optional goal-separator colons,
/// and sequence connectors (`然后` / `再` / `最后` / English `then`) when separating bindings.
fn split_clauses(text: &str, ctx: &BindingContext) -> Vec<String> {
    // Pass 1: primary punctuation delimiters.
    let mut primary: Vec<String> = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if matches!(ch, '，' | ',' | '；' | ';' | '。' | '\n') {
            let s = buf.trim();
            if !s.is_empty() {
                primary.push(s.to_string());
            }
            buf.clear();
        } else {
            buf.push(ch);
        }
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        primary.push(tail.to_string());
    }

    // Pass 2: split goal-colon separators like `"Add caching: plan with claude"` or `"加缓存：计划用 claude"`
    // ONLY when the right side contains a binding (never splitting `claude:opus` or `debug: CI timeout`).
    let mut after_colon: Vec<String> = Vec::new();
    for chunk in primary {
        if let Some((left, right)) = split_goal_colon(&chunk, ctx) {
            if !left.is_empty() {
                after_colon.push(left);
            }
            if !right.is_empty() {
                after_colon.push(right);
            }
        } else {
            after_colon.push(chunk);
        }
    }

    // Pass 3: split on sequence connectors (`然后`, `最后`, `再`, English `then`) when the split
    // actually separates binding fragments (avoids mangling goals like `"重试后再上报"`).
    let mut result: Vec<String> = Vec::new();
    for chunk in after_colon {
        split_on_connectors(&chunk, ctx, &mut result);
    }
    result
}

fn split_goal_colon(chunk: &str, ctx: &BindingContext) -> Option<(String, String)> {
    // Prefer full-width Chinese colon `：` or ASCII `: ` (colon followed by whitespace).
    for (idx, ch) in chunk.char_indices() {
        let is_sep = ch == '：'
            || (ch == ':'
                && chunk[idx + 1..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace));
        if !is_sep {
            continue;
        }
        let left = chunk[..idx].trim();
        let right = chunk[idx + ch.len_utf8()..].trim();
        if left.is_empty() || right.is_empty() {
            continue;
        }
        // Do not split leading playbook prefix like `"debug: CI 偶发超时"` if right has no executor.
        let right_has_binding = extract_executor(right, ctx).is_some()
            && (!extract_stages(&right.to_lowercase()).is_empty()
                || has_rest_keyword(&right.to_lowercase())
                || has_all_stage_keyword(&right.to_lowercase()));
        if right_has_binding {
            // If `left` is just a bare playbook prefix like `"feature"` or `"debug"`, drop it.
            let left_lower = left.to_ascii_lowercase();
            if matches!(
                left_lower.as_str(),
                "feature" | "feat" | "debug" | "新功能" | "查错"
            ) {
                return Some((String::new(), right.to_string()));
            }
            return Some((left.to_string(), right.to_string()));
        }
    }
    None
}

fn split_on_connectors(chunk: &str, ctx: &BindingContext, out: &mut Vec<String>) {
    // Try Chinese connectors (`然后`, `最后`, `再`) and English `then`.
    let connectors: &[(&str, bool)] = &[
        ("然后", false),
        ("最后", false),
        ("再", false),
        ("then", true),
    ];

    for &(conn, ascii_word) in connectors {
        if let Some((left, right)) = split_once_connector(chunk, conn, ascii_word) {
            let left_t = left.trim();
            let right_t = right.trim();
            if left_t.is_empty() || right_t.is_empty() {
                continue;
            }
            // Only split if at least one side has an executor AND splitting separates clauses cleanly
            // (so goals like "重试后再上报" or "最后一条日志丢失" stay intact).
            let left_exec = extract_executor(left_t, ctx).is_some();
            let right_exec = extract_executor(right_t, ctx).is_some();
            if right_exec && (left_exec || conn != "再") {
                split_on_connectors(left_t, ctx, out);
                split_on_connectors(right_t, ctx, out);
                return;
            }
        }
    }

    // Trim leading connector words like `"然后 review 用 agy"` or `"最后总结用 claude"`
    let cleaned = trim_leading_connector(chunk);
    if !cleaned.is_empty() {
        out.push(cleaned.to_string());
    }
}

fn split_once_connector<'a>(
    s: &'a str,
    conn: &str,
    ascii_word: bool,
) -> Option<(&'a str, &'a str)> {
    if ascii_word {
        let lower = s.to_ascii_lowercase();
        let pos = crate::vocab::find_ascii_word_pos(&lower, conn)?;
        Some((&s[..pos], &s[pos + conn.len()..]))
    } else {
        let pos = s.find(conn)?;
        Some((&s[..pos], &s[pos + conn.len()..]))
    }
}

fn trim_leading_connector(s: &str) -> &str {
    let mut t = s.trim();
    for prefix in ["然后", "最后", "接着", "并且", "并", "再"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            let rest_t = rest.trim();
            if !rest_t.is_empty() {
                t = rest_t;
                break;
            }
        }
    }
    let lower = t.to_ascii_lowercase();
    for prefix in ["then ", "and then ", "finally ", "and "] {
        if lower.starts_with(prefix) {
            t = t[prefix.len()..].trim();
            break;
        }
    }
    t
}

/// Strip `same_as:<stage>` tokens so the referenced target stage is not misread as a bound stage.
fn strip_same_as_tokens(clause_lower: &str) -> String {
    let mut out = String::with_capacity(clause_lower.len());
    let mut rest = clause_lower;
    while let Some(pos) = rest.find("same_as:") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "same_as:".len()..];
        let skip = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        out.push(' ');
        rest = &after[skip..];
    }
    out.push_str(rest);
    out
}

/// Handle Chinese `same_as` phrases like `"汇报同计划"`, `"总结沿用规划"`, `"报告同上"`.
fn try_parse_cn_same_as(
    clause: &str,
    clause_lower: &str,
    last_bound: Option<StageKind>,
) -> Option<(Vec<StageKind>, StageKind)> {
    // Explicit "同上" / "同前"
    if clause.contains("同上") || clause.contains("同前") {
        let target = last_bound?;
        let stages = extract_stages(clause_lower);
        if !stages.is_empty() {
            return Some((stages, target));
        }
    }

    // "<stage1>同<stage2>" or "<stage1>沿用<stage2>"
    for sep in ["沿用", "复用", "同"] {
        if let Some((left, right)) = clause_lower.split_once(sep) {
            let left_stages = extract_stages(left);
            let right_stages = extract_stages(right);
            if !left_stages.is_empty() && right_stages.len() == 1 {
                let target = right_stages[0];
                if !left_stages.contains(&target) {
                    return Some((left_stages, target));
                }
            }
        }
    }

    None
}
