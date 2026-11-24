//! Deterministic vocabulary tables and zero-regex matchers for `baton-binding` (contracts §6).

use crate::types::BindingContext;
use baton_proto::{
    AgentKind, Effort, NativeMode, PlaybookKind, ReviewDepth, StageBinding, StageKind,
};

/// Characters that belong inside an ASCII identifier/model token.
/// Including `-` ensures `high` inside `gemini-4-neon-high` does NOT match as effort `High`.
pub(crate) fn is_ascii_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Case-insensitive ASCII word-boundary match (no regex).
pub(crate) fn contains_ascii_word(haystack_lower: &str, needle_lower: &str) -> bool {
    find_ascii_word_pos(haystack_lower, needle_lower).is_some()
}

/// Returns the first byte offset of a word-boundary match for `needle_lower` in `haystack_lower`.
pub(crate) fn find_ascii_word_pos(haystack_lower: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let starts_word = needle_lower.chars().next().is_some_and(is_ascii_word_char);
    let ends_word = needle_lower.chars().last().is_some_and(is_ascii_word_char);

    for (start, matched) in haystack_lower.match_indices(needle_lower) {
        let left_ok = if starts_word {
            haystack_lower[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !is_ascii_word_char(c))
        } else {
            true
        };
        let end = start + matched.len();
        let right_ok = if ends_word {
            haystack_lower[end..]
                .chars()
                .next()
                .is_none_or(|c| !is_ascii_word_char(c))
        } else {
            true
        };
        if left_ok && right_ok {
            return Some(start);
        }
    }
    None
}

/// Match a keyword: ASCII keywords require word boundaries; non-ASCII (Chinese) uses substring.
pub(crate) fn matches_keyword(haystack_lower: &str, kw_lower: &str) -> bool {
    if kw_lower.is_ascii() {
        contains_ascii_word(haystack_lower, kw_lower)
    } else {
        haystack_lower.contains(kw_lower)
    }
}

/// Find the earliest match position of a keyword (ASCII word-boundary or Chinese substring).
pub(crate) fn find_keyword_pos(haystack_lower: &str, kw_lower: &str) -> Option<usize> {
    if kw_lower.is_ascii() {
        find_ascii_word_pos(haystack_lower, kw_lower)
    } else {
        haystack_lower.find(kw_lower)
    }
}

/// Extract ASCII tokens suitable for model/shorthand inspection (preserving `:`, `@`, `.`, `-`, `_`, `/`).
pub(crate) fn ascii_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        let is_tok = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '@' | '/');
        if is_tok {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            let trimmed = text[s..idx].trim_matches(|c: char| matches!(c, '.' | ':' | '@' | '-'));
            if !trimmed.is_empty() {
                tokens.push(trimmed);
            }
        }
    }
    if let Some(s) = start {
        let trimmed = text[s..].trim_matches(|c: char| matches!(c, '.' | ':' | '@' | '-'));
        if !trimmed.is_empty() {
            tokens.push(trimmed);
        }
    }
    tokens
}

pub(crate) const FEATURE_PLAYBOOK_WORDS: &[&str] = &["新功能", "功能", "需求", "feature", "feat"];

pub(crate) const DEBUG_PLAYBOOK_WORDS: &[&str] = &[
    "查错", "排查", "排障", "调试", "debug", "bug", "报错", "故障", "500", "崩溃",
];

pub(crate) const STAGE_WORDS: &[(StageKind, &[&str])] = &[
    (
        StageKind::Plan,
        &["计划", "规划", "设计", "方案", "plan", "planning", "design"],
    ),
    (
        StageKind::Implement,
        &[
            "实现",
            "写代码",
            "开发",
            "编码",
            "implement",
            "implementation",
            "build",
            "coding",
            "code",
            "dev",
        ],
    ),
    (
        StageKind::TestReview,
        &[
            "test_review",
            "测试",
            "评审",
            "审查",
            "review",
            "reviewer",
            "testing",
            "test",
            "qa",
            "cr",
        ],
    ),
    (
        StageKind::Report,
        &["总结", "汇报", "文档", "报告", "report", "summary"],
    ),
    (
        StageKind::Triage,
        &["快筛", "快速", "初查", "初步", "定位", "triage", "fast"],
    ),
    (
        StageKind::Deep,
        &["深挖", "深入", "根因", "修复", "root cause", "deep", "rca"],
    ),
];

pub(crate) const REST_WORDS: &[&str] = &[
    "其余",
    "其他",
    "剩下",
    "其余阶段",
    "rest",
    "others",
    "remaining",
];
pub(crate) const ALL_STAGE_WORDS: &[&str] = &["全部", "所有", "全流程", "all stages", "everything"];

/// Extract all distinct stages mentioned in a clause, in order of first appearance.
pub(crate) fn extract_stages(clause_lower: &str) -> Vec<StageKind> {
    let mut hits: Vec<(usize, StageKind)> = Vec::new();
    for &(stage, words) in STAGE_WORDS {
        let mut earliest: Option<usize> = None;
        for &w in words {
            if let Some(pos) = find_keyword_pos(clause_lower, w) {
                earliest = Some(match earliest {
                    Some(cur) => cur.min(pos),
                    None => pos,
                });
            }
        }
        if let Some(pos) = earliest {
            hits.push((pos, stage));
        }
    }
    hits.sort_by_key(|&(pos, _)| pos);
    hits.into_iter().map(|(_, s)| s).collect()
}

pub(crate) fn has_rest_keyword(clause_lower: &str) -> bool {
    REST_WORDS.iter().any(|w| matches_keyword(clause_lower, w))
}

pub(crate) fn has_all_stage_keyword(clause_lower: &str) -> bool {
    ALL_STAGE_WORDS
        .iter()
        .any(|w| matches_keyword(clause_lower, w))
}

/// Detect whether a clause has a binding verb/marker suggesting the user intended a binding
/// (e.g. `用`, `使用`, `让`, `交给`, `走`, `跑`, `with`, `by`, `using`, `=`).
pub(crate) fn has_binding_verb(clause_lower: &str) -> bool {
    const CN_VERBS: &[&str] = &["用", "使用", "让", "交给", "分配给", "指定"];
    const EN_VERBS: &[&str] = &["with", "using", "via", "by"];
    if CN_VERBS.iter().any(|v| clause_lower.contains(v)) {
        return true;
    }
    if EN_VERBS
        .iter()
        .any(|v| contains_ascii_word(clause_lower, v))
    {
        return true;
    }
    clause_lower.contains('=')
}

/// Detect explicit playbook words from the utterance text.
pub(crate) fn detect_playbook_from_text(text_lower: &str) -> (PlaybookKind, bool) {
    // Check leading explicit prefix first (e.g. "debug: ...", "feature: ...", "新功能 ...", "查错 ...")
    let trimmed = text_lower.trim_start();
    for prefix in [
        "debug:", "debug：", "debug ", "查错", "排查", "排障", "调试",
    ] {
        if trimmed.starts_with(prefix) {
            return (PlaybookKind::Debug, true);
        }
    }
    for prefix in [
        "feature:",
        "feature：",
        "feature ",
        "feat:",
        "feat：",
        "feat ",
        "新功能",
    ] {
        if trimmed.starts_with(prefix) {
            return (PlaybookKind::Feature, true);
        }
    }

    let has_debug_word = DEBUG_PLAYBOOK_WORDS
        .iter()
        .any(|w| matches_keyword(text_lower, w));
    let has_feature_word = FEATURE_PLAYBOOK_WORDS
        .iter()
        .any(|w| matches_keyword(text_lower, w));

    let stages = extract_stages(text_lower);
    let has_debug_stage = stages
        .iter()
        .any(|s| matches!(s, StageKind::Triage | StageKind::Deep));
    let has_feature_stage = stages.iter().any(|s| {
        matches!(
            s,
            StageKind::Plan | StageKind::Implement | StageKind::TestReview | StageKind::Report
        )
    });

    if has_debug_word && !has_feature_word {
        // Exception: if user only mentioned feature stages (e.g. "修复500的功能，计划用claude，实现用codex")
        // and no debug stages, prefer Feature only when feature stages exist without debug stages AND
        // the debug word is just a symptom word like "500"/"报错"/"bug".
        let strong_debug = ["查错", "排查", "排障", "调试", "debug"]
            .iter()
            .any(|w| matches_keyword(text_lower, w));
        if strong_debug || !has_feature_stage || has_debug_stage {
            return (PlaybookKind::Debug, true);
        }
        return (PlaybookKind::Feature, false);
    }

    if has_feature_word && !has_debug_word {
        return (PlaybookKind::Feature, true);
    }

    if has_debug_stage && !has_feature_stage {
        return (PlaybookKind::Debug, true);
    }

    if has_debug_word && has_feature_word {
        if has_debug_stage && !has_feature_stage {
            return (PlaybookKind::Debug, true);
        }
        if has_feature_stage && !has_debug_stage {
            return (PlaybookKind::Feature, true);
        }
    }

    (PlaybookKind::Feature, false)
}

/// Strip an optional leading playbook prefix (`debug:`, `feature:`, `feat:`, `新功能：`) from goal text.
pub(crate) fn strip_playbook_prefix(s: &str) -> &str {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in [
        "debug:",
        "debug：",
        "feature:",
        "feature：",
        "feat:",
        "feat：",
        "新功能:",
        "新功能：",
        "查错:",
        "查错：",
    ] {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].trim();
        }
    }
    trimmed
}

/// Parse an executor (`StageBinding`) from a single clause, returning `None` if no executor cue exists.
pub(crate) fn extract_executor(clause: &str, ctx: &BindingContext) -> Option<StageBinding> {
    let lower = clause.to_lowercase();
    let mut binding = StageBinding::default();
    let mut matched_any = false;

    // 1. Direct shorthand tokens (e.g. `same_as:plan`, `claude:opus@high`, `agy:gemini-4-neon-high`, `codex@xhigh`)
    for tok in ascii_tokens(clause) {
        let tok_lower = tok.to_ascii_lowercase();

        if let Some(stage_str) = tok_lower.strip_prefix("same_as:") {
            if let Some(st) = StageKind::parse(stage_str) {
                return Some(StageBinding {
                    same_as: Some(st),
                    ..Default::default()
                });
            }
        }

        // Token with `:` or `@` -> try parsing as `<agent>[:<model>][@<effort>]`
        if tok.contains(':') || tok.contains('@') {
            if let Some(parsed) = parse_inline_shorthand_token(tok) {
                merge_binding(&mut binding, &parsed);
                matched_any = true;
                continue;
            }
        }
    }

    // 2. Context aliases (expand configured aliases before generic vocab)
    // Sort by key length descending so longer aliases win over prefixes.
    let mut alias_entries: Vec<(&String, &String)> = ctx.aliases.iter().collect();
    alias_entries.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
    for (alias, shorthand) in alias_entries {
        let alias_lower = alias.to_lowercase();
        if matches_keyword(&lower, &alias_lower) {
            if let Ok(expanded) = StageBinding::from_shorthand(shorthand) {
                merge_binding(&mut binding, &expanded);
                matched_any = true;
            }
        }
    }

    // 3. Explicit known models from `ctx.known_models`
    for (&agent, models) in &ctx.known_models {
        for m in models {
            let m_lower = m.to_ascii_lowercase();
            if matches_keyword(&lower, &m_lower) {
                if binding.agent.is_none() {
                    binding.agent = Some(agent);
                }
                if binding.model.is_none() {
                    binding.model = Some(m.clone());
                }
                matched_any = true;
            }
        }
    }

    // 4. Token-based model families (`gpt*`, `gemini*`, `claude-*`)
    for tok in ascii_tokens(clause) {
        if tok.contains(':') || tok.contains('@') || tok.starts_with('/') {
            continue;
        }
        let tl = tok.to_ascii_lowercase();
        if tl.starts_with("gpt") && tl.len() >= 3 {
            if binding.agent.is_none() {
                binding.agent = Some(AgentKind::Codex);
            }
            if binding.model.is_none() {
                binding.model = Some(tl);
            }
            matched_any = true;
        } else if tl.starts_with("gemini") {
            if binding.agent.is_none() {
                binding.agent = Some(AgentKind::Agy);
            }
            if binding.model.is_none() {
                binding.model = Some(tl);
            }
            matched_any = true;
        } else if tl.starts_with("claude-") && tl.len() > "claude-".len() {
            if binding.agent.is_none() {
                binding.agent = Some(AgentKind::Claude);
            }
            if binding.model.is_none() {
                binding.model = Some(tl);
            }
            matched_any = true;
        }
    }

    // 5. Built-in agent & model keywords (contracts §6)
    // Claude model shorthands (opus / sonnet / haiku)
    for model_kw in ["opus", "sonnet", "haiku"] {
        if contains_ascii_word(&lower, model_kw) {
            if binding.agent.is_none() {
                binding.agent = Some(AgentKind::Claude);
            }
            if binding.model.is_none() {
                binding.model = Some(model_kw.to_string());
            }
            matched_any = true;
        }
    }

    // Explicit agent names
    if binding.agent.is_none() {
        if contains_ascii_word(&lower, "claude") || contains_ascii_word(&lower, "cc") {
            binding.agent = Some(AgentKind::Claude);
            matched_any = true;
        } else if contains_ascii_word(&lower, "codex") {
            binding.agent = Some(AgentKind::Codex);
            matched_any = true;
        } else if contains_ascii_word(&lower, "agy") || contains_ascii_word(&lower, "antigravity") {
            binding.agent = Some(AgentKind::Agy);
            matched_any = true;
        }
    }

    // 6. Native modes (`/swarm`, `swarm`, `/boost`, `boost`, `/teamwork`, `teamwork`, `subagent`, `子代理`, `并行`)
    if binding.native.is_none() {
        if let Some(native) = extract_native_mode(&lower) {
            binding.native = Some(native);
            matched_any = true;
            // If no agent was explicitly stated, infer from native mode owner (e.g. `/swarm` -> `agy`).
            if binding.agent.is_none() {
                if let Some(owner) = native.owner() {
                    binding.agent = Some(owner);
                }
            }
        }
    }

    // If neither agent, model, nor native was matched, do not treat stray effort/depth words as an executor.
    if !matched_any {
        return None;
    }

    // 7. Effort keywords (contracts §6)
    if binding.effort.is_none() {
        binding.effort = extract_effort(&lower);
    }

    // 8. Optional ReviewDepth hint (`D0`..`D3`)
    if binding.depth.is_none() {
        binding.depth = extract_review_depth(&lower);
    }

    Some(binding)
}

fn parse_inline_shorthand_token(tok: &str) -> Option<StageBinding> {
    let (head, effort) = match tok.rsplit_once('@') {
        Some((h, e)) => {
            let eff = Effort::parse(e)?;
            (h, Some(eff))
        }
        None => (tok, None),
    };

    let (agent_str, model) = match head.split_once(':') {
        Some((a, m)) if !m.is_empty() => (a, Some(m.to_string())),
        Some(_) => return None,
        None => (head, None),
    };

    let agent = parse_agent_word(agent_str)?;
    Some(StageBinding {
        agent: Some(agent),
        model,
        effort,
        ..Default::default()
    })
}

fn parse_agent_word(s: &str) -> Option<AgentKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude" | "cc" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "agy" | "antigravity" => Some(AgentKind::Agy),
        _ => None,
    }
}

fn merge_binding(dst: &mut StageBinding, src: &StageBinding) {
    if dst.agent.is_none() {
        dst.agent = src.agent;
    }
    if dst.model.is_none() {
        dst.model = src.model.clone();
    }
    if dst.effort.is_none() {
        dst.effort = src.effort;
    }
    if dst.native.is_none() {
        dst.native = src.native;
    }
    if dst.depth.is_none() {
        dst.depth = src.depth;
    }
    if dst.same_as.is_none() {
        dst.same_as = src.same_as;
    }
}

fn extract_native_mode(lower: &str) -> Option<NativeMode> {
    if lower.contains("/swarm") || contains_ascii_word(lower, "swarm") {
        Some(NativeMode::Swarm)
    } else if lower.contains("/boost") || contains_ascii_word(lower, "boost") {
        Some(NativeMode::Boost)
    } else if lower.contains("/teamwork") || contains_ascii_word(lower, "teamwork") {
        Some(NativeMode::Teamwork)
    } else if lower.contains("/subagents")
        || lower.contains("/subagent")
        || contains_ascii_word(lower, "subagents")
        || contains_ascii_word(lower, "subagent")
        || lower.contains("子代理")
        || lower.contains("并行")
    {
        Some(NativeMode::Subagents)
    } else {
        None
    }
}

fn extract_effort(lower: &str) -> Option<Effort> {
    // Check specific multi-char and ASCII keywords first.
    if contains_ascii_word(lower, "xhigh") || lower.contains("超高") {
        return Some(Effort::Xhigh);
    }
    if contains_ascii_word(lower, "max") || lower.contains("最高") {
        return Some(Effort::Max);
    }
    if contains_ascii_word(lower, "high")
        || lower.contains("高强度")
        || lower.contains("强度高")
        || lower.contains("高档")
    {
        return Some(Effort::High);
    }
    if contains_ascii_word(lower, "medium")
        || lower.contains("中等")
        || lower.contains("中强度")
        || lower.contains("强度中")
        || lower.contains("中档")
    {
        return Some(Effort::Medium);
    }
    if contains_ascii_word(lower, "low")
        || lower.contains("低强度")
        || lower.contains("强度低")
        || lower.contains("低档")
    {
        return Some(Effort::Low);
    }

    // Standalone single-character Chinese effort markers (`高`, `中`, `低`) bounded by
    // whitespace, punctuation, ASCII characters, or end-of-string (avoids matching inside
    // compound words like `高并发`, `中文`, `降低`).
    for (ch, eff) in [
        ('高', Effort::High),
        ('中', Effort::Medium),
        ('低', Effort::Low),
    ] {
        if has_standalone_cn_effort(lower, ch) {
            return Some(eff);
        }
    }

    None
}

fn has_standalone_cn_effort(s: &str, target: char) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != target {
            continue;
        }
        let prev_ok =
            i == 0 || !is_cjk(chars[i - 1]) || matches!(chars[i - 1], '用' | '开' | '选' | '为');
        let next_ok =
            i + 1 == chars.len() || !is_cjk(chars[i + 1]) || matches!(chars[i + 1], '的' | '档');
        if prev_ok && next_ok {
            return true;
        }
    }
    false
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF)
}

fn extract_review_depth(lower: &str) -> Option<ReviewDepth> {
    for (kw, d) in [
        ("d0", ReviewDepth::D0),
        ("d1", ReviewDepth::D1),
        ("d2", ReviewDepth::D2),
        ("d3", ReviewDepth::D3),
    ] {
        if contains_ascii_word(lower, kw) {
            return Some(d);
        }
    }
    None
}
