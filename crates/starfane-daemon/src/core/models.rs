//! Available model & reasoning-effort catalog per agent (`GET /api/models`),
//! discovered from local CLI configs (`~/.claude/settings.json`, `~/.codex/config.toml`,
//! `agy models`, `~/.gemini/antigravity-cli/settings.json`, and read-only
//! `~/.cc-switch/cc-switch.db`) with graceful built-in fallbacks.
//!
//! For AGY, effort-suffixed variants (`gemini-4-neon-low/medium/high/xhigh`) are
//! aggregated into a single base model entry (`gemini-4-neon`) carrying its exact
//! supported `efforts` array, while fixed-configuration models (`Orchestrator (*)`)
//! report `efforts: []`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const EFFORT_ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const CLAUDE_EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];
const CODEX_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const AGY_DEFAULT_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub efforts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelCatalog {
    pub default: String,
    pub default_effort: String,
    pub efforts: Vec<String>,
    pub models: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub claude: AgentModelCatalog,
    pub codex: AgentModelCatalog,
    pub agy: AgentModelCatalog,
}

/// Discover the local model catalog for all three agents. Never fails; missing CLIs
/// or config files fall back to curated defaults.
pub fn discover_model_catalog(agy_bin: &str) -> ModelCatalog {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cc_models = home
        .as_deref()
        .map(read_recent_cc_switch_models)
        .unwrap_or_default();

    let claude = build_claude_catalog(home.as_deref(), &cc_models.claude);
    let codex = build_codex_catalog(home.as_deref(), &cc_models.codex);
    let agy = build_agy_catalog(home.as_deref(), agy_bin, &cc_models.agy);

    ModelCatalog { claude, codex, agy }
}

#[derive(Default)]
struct RecentCcModels {
    claude: Vec<String>,
    codex: Vec<String>,
    agy: Vec<String>,
}

fn read_recent_cc_switch_models(home: &Path) -> RecentCcModels {
    let db = home.join(".cc-switch/cc-switch.db");
    if !db.is_file() {
        return RecentCcModels::default();
    }
    let sql = "SELECT app_type, model, MAX(created_at) AS ts FROM proxy_request_logs WHERE model IS NOT NULL AND model != '' GROUP BY app_type, model ORDER BY ts DESC LIMIT 60;";
    let Ok(out) = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(&db)
        .arg(sql)
        .output()
    else {
        return RecentCcModels::default();
    };
    if !out.status.success() {
        return RecentCcModels::default();
    }
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&out.stdout) else {
        return RecentCcModels::default();
    };
    let mut res = RecentCcModels::default();
    for r in rows {
        let app = r.get("app_type").and_then(Value::as_str).unwrap_or("");
        let m = r
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if m.is_empty() || m == "<synthetic>" || m == "codex-auto-review" {
            continue;
        }
        match app {
            "claude" => res.claude.push(m.to_string()),
            "codex" => res.codex.push(m.to_string()),
            "agy" => res.agy.push(m.to_string()),
            _ => {}
        }
    }
    res
}

fn str_vec(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| (*s).to_string()).collect()
}

fn push_unique_model(
    list: &mut Vec<ModelOption>,
    seen: &mut HashSet<String>,
    id: &str,
    label: &str,
    efforts: &[&str],
) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    let key = id.to_ascii_lowercase();
    if seen.insert(key) {
        let lbl = label.trim();
        list.push(ModelOption {
            id: id.to_string(),
            label: if lbl.is_empty() {
                id.to_string()
            } else {
                lbl.to_string()
            },
            efforts: str_vec(efforts),
        });
    }
}

fn normalize_effort_choice(raw: Option<&str>, allowed: &[&str], fallback: &str) -> String {
    if let Some(r) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        let low = r.to_ascii_lowercase();
        if allowed.contains(&low.as_str()) {
            return low;
        }
    }
    fallback.to_string()
}

pub(crate) fn build_claude_catalog(home: Option<&Path>, recent: &[String]) -> AgentModelCatalog {
    let mut default_model: Option<String> = None;
    let mut default_effort_raw: Option<String> = None;
    let mut settings_models: Vec<String> = Vec::new();

    if let Some(h) = home {
        let p = h.join(".claude/settings.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(m) = v
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    default_model = Some(m.to_string());
                }
                if let Some(e) = v
                    .get("effortLevel")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    default_effort_raw = Some(e.to_string());
                }
                if let Some(obj) = v.get("modelSettings").and_then(Value::as_object) {
                    for k in obj.keys() {
                        settings_models.push(k.clone());
                    }
                }
            }
        }
    }

    let default = default_model.unwrap_or_else(|| "opus".to_string());
    let default_effort =
        normalize_effort_choice(default_effort_raw.as_deref(), &CLAUDE_EFFORTS, "high");
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    push_unique_model(
        &mut models,
        &mut seen,
        &default,
        &claude_label(&default),
        &CLAUDE_EFFORTS,
    );
    for (id, label) in [
        ("fable", "Fable"),
        ("opus", "Opus"),
        ("sonnet", "Sonnet"),
        ("haiku", "Haiku"),
        ("claude-fable-5-2", "Claude Fable 5.2"),
        ("claude-opus-5-5", "Claude Opus 5.5"),
        ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ] {
        push_unique_model(&mut models, &mut seen, id, label, &CLAUDE_EFFORTS);
    }
    for m in &settings_models {
        push_unique_model(&mut models, &mut seen, m, &claude_label(m), &CLAUDE_EFFORTS);
    }
    for m in recent {
        push_unique_model(&mut models, &mut seen, m, &claude_label(m), &CLAUDE_EFFORTS);
    }

    AgentModelCatalog {
        default,
        default_effort,
        efforts: str_vec(&CLAUDE_EFFORTS),
        models,
    }
}

fn claude_label(id: &str) -> String {
    match id {
        "fable" => "Fable".into(),
        "opus" => "Opus".into(),
        "sonnet" => "Sonnet".into(),
        "haiku" => "Haiku".into(),
        other => other.to_string(),
    }
}

pub(crate) fn build_codex_catalog(home: Option<&Path>, recent: &[String]) -> AgentModelCatalog {
    let mut default_model: Option<String> = None;
    let mut default_effort_raw: Option<String> = None;
    if let Some(h) = home {
        if let Ok(text) = std::fs::read_to_string(h.join(".codex/config.toml")) {
            default_model = parse_toml_string_field(&text, "model");
            default_effort_raw = parse_toml_string_field(&text, "model_reasoning_effort");
        }
    }
    let default = default_model.unwrap_or_else(|| "gpt-6.2-sol".to_string());
    let default_effort =
        normalize_effort_choice(default_effort_raw.as_deref(), &CODEX_EFFORTS, "high");
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    push_unique_model(&mut models, &mut seen, &default, &default, &CODEX_EFFORTS);
    for m in recent {
        push_unique_model(&mut models, &mut seen, m, m, &CODEX_EFFORTS);
    }
    for id in [
        "gpt-6.2-sol",
        "gpt-6.2-astra",
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5-codex",
    ] {
        push_unique_model(&mut models, &mut seen, id, id, &CODEX_EFFORTS);
    }

    AgentModelCatalog {
        default,
        default_effort,
        efforts: str_vec(&CODEX_EFFORTS),
        models,
    }
}

fn parse_toml_string_field(text: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                let cleaned = val
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches(['"', '\'']);
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
}

/// Split an AGY model ID into `(base_id, Option<effort>)`.
/// Example: `"gemini-4-neon-xhigh"` -> `("gemini-4-neon", Some("xhigh"))`,
/// `"gemini-pro-orchestrator-neon"` -> `("gemini-pro-orchestrator-neon", None)`.
pub(crate) fn split_agy_id_effort(id: &str) -> (&str, Option<&'static str>) {
    let trimmed = id.trim();
    let lower = trimmed.to_ascii_lowercase();
    for eff in EFFORT_ORDER {
        let suffix = format!("-{eff}");
        if lower.ends_with(&suffix) && trimmed.len() > suffix.len() {
            return (&trimmed[..trimmed.len() - suffix.len()], Some(eff));
        }
    }
    (trimmed, None)
}

/// Split an AGY display label into `(base_label, Option<effort>)`.
/// Example: `"Gemini 4 Neon (High)"` -> `("Gemini 4 Neon", Some("high"))`,
/// `"Orchestrator (Neon)"` -> `("Orchestrator (Neon)", None)`.
pub(crate) fn split_agy_label_effort(label: &str) -> (&str, Option<&'static str>) {
    let trimmed = label.trim();
    if let Some(open) = trimmed.rfind('(') {
        if trimmed.ends_with(')') {
            let inner = trimmed[open + 1..trimmed.len() - 1]
                .trim()
                .to_ascii_lowercase();
            for eff in EFFORT_ORDER {
                if inner == eff {
                    return (trimmed[..open].trim_end(), Some(eff));
                }
            }
        }
    }
    (trimmed, None)
}

fn sort_efforts_canonical(efforts: &mut Vec<String>) {
    efforts.sort_by_key(|e| {
        EFFORT_ORDER
            .iter()
            .position(|&x| x == e.as_str())
            .unwrap_or(99)
    });
    efforts.dedup();
}

/// Parse `agy models` output and aggregate effort-suffixed variants into deduplicated
/// base models with per-model `efforts` lists (`Orchestrator (*)` gets `efforts: []`).
pub(crate) fn parse_agy_models_aggregated(stdout: &str) -> Vec<ModelOption> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, ModelOption> = HashMap::new();

    for line in stdout.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Fetching") {
            continue;
        }
        let Some((raw_id, raw_label)) = t.split_once('\t') else {
            continue;
        };
        let (base_id, id_eff) = split_agy_id_effort(raw_id);
        let (base_label, lbl_eff) = split_agy_label_effort(raw_label);
        if base_id.is_empty() {
            continue;
        }
        let eff = id_eff.or(lbl_eff);
        let key = base_id.to_ascii_lowercase();
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            ModelOption {
                id: base_id.to_string(),
                label: if base_label.is_empty() {
                    base_id.to_string()
                } else {
                    base_label.to_string()
                },
                efforts: Vec::new(),
            }
        });
        if let Some(e) = eff {
            if !entry.efforts.iter().any(|x| x == e) {
                entry.efforts.push(e.to_string());
            }
        }
    }

    order
        .into_iter()
        .filter_map(|k| {
            let mut opt = by_key.remove(&k)?;
            sort_efforts_canonical(&mut opt.efforts);
            Some(opt)
        })
        .collect()
}

fn builtin_agy_models() -> Vec<ModelOption> {
    vec![
        ModelOption {
            id: "gemini-4-neon".into(),
            label: "Gemini 4 Neon".into(),
            efforts: str_vec(&["low", "medium", "high", "xhigh"]),
        },
        ModelOption {
            id: "gemini-4-carbon".into(),
            label: "Gemini 4 Carbon".into(),
            efforts: str_vec(&["low", "medium", "high"]),
        },
        ModelOption {
            id: "gemini-4-argon".into(),
            label: "Gemini 4 Argon".into(),
            efforts: str_vec(&["low", "medium", "high"]),
        },
        ModelOption {
            id: "gemini-4-hydrogen".into(),
            label: "Gemini 4 Hydrogen".into(),
            efforts: str_vec(&["low", "medium", "high", "xhigh"]),
        },
        ModelOption {
            id: "gemini-3.8-flash".into(),
            label: "Gemini 3.8 Flash".into(),
            efforts: str_vec(&["low", "medium", "high"]),
        },
        ModelOption {
            id: "claude-opus-5-5".into(),
            label: "Claude Opus 5.5".into(),
            efforts: str_vec(&["low", "medium", "high"]),
        },
        ModelOption {
            id: "claude-sonnet-5-5".into(),
            label: "Claude Sonnet 5.5".into(),
            efforts: str_vec(&["low", "medium", "high"]),
        },
        ModelOption {
            id: "gemini-pro-orchestrator-neon".into(),
            label: "Orchestrator (Neon)".into(),
            efforts: Vec::new(),
        },
        ModelOption {
            id: "gemini-pro-orchestrator".into(),
            label: "Orchestrator (Carbon)".into(),
            efforts: Vec::new(),
        },
        ModelOption {
            id: "gemini-pro-orchestrator-argon".into(),
            label: "Orchestrator (Argon)".into(),
            efforts: Vec::new(),
        },
        ModelOption {
            id: "gpt-oss-120b".into(),
            label: "GPT-OSS 120B".into(),
            efforts: str_vec(&["medium"]),
        },
    ]
}

pub(crate) fn build_agy_catalog(
    home: Option<&Path>,
    agy_bin: &str,
    recent: &[String],
) -> AgentModelCatalog {
    let mut models = Command::new(agy_bin)
        .arg("models")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_agy_models_aggregated(&s))
        .unwrap_or_default();

    if models.is_empty() {
        models = builtin_agy_models();
    }

    // Merge any recent models (normalizing out effort suffixes so no duplicates appear).
    let mut seen: HashSet<String> = models.iter().map(|m| m.id.to_ascii_lowercase()).collect();
    for raw in recent {
        if raw.contains("-tiered-") {
            continue;
        }
        let (base, eff) = split_agy_id_effort(raw);
        let key = base.to_ascii_lowercase();
        if seen.insert(key) {
            let mut effs = Vec::new();
            if let Some(e) = eff {
                effs.push(e.to_string());
            }
            models.push(ModelOption {
                id: base.to_string(),
                label: base.to_string(),
                efforts: effs,
            });
        }
    }

    let settings_val = home.and_then(|h| {
        let text = std::fs::read_to_string(h.join(".gemini/antigravity-cli/settings.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v.get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });

    let (default, default_effort) = resolve_agy_default(&models, settings_val.as_deref());

    let mut all_efforts: Vec<String> = models
        .iter()
        .flat_map(|m| m.efforts.iter().cloned())
        .collect();
    if all_efforts.is_empty() {
        all_efforts = str_vec(&AGY_DEFAULT_EFFORTS);
    } else {
        sort_efforts_canonical(&mut all_efforts);
    }

    AgentModelCatalog {
        default,
        default_effort,
        efforts: all_efforts,
        models,
    }
}

fn resolve_agy_default(models: &[ModelOption], settings_val: Option<&str>) -> (String, String) {
    if let Some(sv) = settings_val {
        let (lbl_base, lbl_eff) = split_agy_label_effort(sv);
        let (id_base, id_eff) = split_agy_id_effort(sv);
        if let Some(found) = models.iter().find(|m| {
            m.id.eq_ignore_ascii_case(sv)
                || m.id.eq_ignore_ascii_case(id_base)
                || m.label.eq_ignore_ascii_case(sv)
                || m.label.eq_ignore_ascii_case(lbl_base)
        }) {
            let eff = lbl_eff
                .or(id_eff)
                .map(str::to_string)
                .filter(|e| found.efforts.iter().any(|x| x == e))
                .or_else(|| {
                    if found.efforts.iter().any(|x| x == "high") {
                        Some("high".to_string())
                    } else {
                        found.efforts.first().cloned()
                    }
                })
                .unwrap_or_default();
            return (found.id.clone(), eff);
        }
    }
    let first = models
        .first()
        .map(|m| m.id.clone())
        .unwrap_or_else(|| "gemini-4-neon".into());
    (first, "high".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_agy_models_by_base_id_and_preserves_fixed_orchestrators() {
        let out = "\
Fetching available models...
gemini-4-neon-high\tGemini 4 Neon (High)
gemini-4-neon-xhigh\tGemini 4 Neon (XHigh)
gemini-4-neon-medium\tGemini 4 Neon (Medium)
gemini-4-neon-low\tGemini 4 Neon (Low)
gemini-4-carbon-medium\tGemini 4 Carbon (Medium)
gemini-4-carbon-high\tGemini 4 Carbon (High)
gemini-4-carbon-low\tGemini 4 Carbon (Low)
gemini-pro-orchestrator-neon\tOrchestrator (Neon)
gemini-pro-orchestrator\tOrchestrator (Carbon)
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)
";
        let parsed = parse_agy_models_aggregated(out);
        assert_eq!(parsed.len(), 5, "{parsed:?}");

        assert_eq!(parsed[0].id, "gemini-4-neon");
        assert_eq!(parsed[0].label, "Gemini 4 Neon");
        assert_eq!(parsed[0].efforts, vec!["low", "medium", "high", "xhigh"]);

        assert_eq!(parsed[1].id, "gemini-4-carbon");
        assert_eq!(parsed[1].efforts, vec!["low", "medium", "high"]);

        assert_eq!(parsed[2].id, "gemini-pro-orchestrator-neon");
        assert_eq!(parsed[2].label, "Orchestrator (Neon)");
        assert!(parsed[2].efforts.is_empty());

        assert_eq!(parsed[4].id, "gpt-oss-120b");
        assert_eq!(parsed[4].efforts, vec!["medium"]);

        // Settings `"Gemini 4 Neon (High)"` splits into `("gemini-4-neon", "high")`.
        let (def, def_eff) = resolve_agy_default(&parsed, Some("Gemini 4 Neon (High)"));
        assert_eq!(def, "gemini-4-neon");
        assert_eq!(def_eff, "high");
    }

    #[test]
    fn builds_claude_codex_agy_catalogs_with_default_efforts_and_fallback() {
        let dir =
            std::env::temp_dir().join(format!("starfane-mod-{}", starfane_proto::id::new_id("T")));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::create_dir_all(dir.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"model":"fable","effortLevel":"max","modelSettings":{"claude-fable-5-2":{"effortLevel":"medium"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".codex/config.toml"),
            "model = \"gpt-6.2-sol\"\nmodel_reasoning_effort = \"xhigh\"\n[other]\nmodel = \"ignored\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(".gemini/antigravity-cli/settings.json"),
            r#"{"model":"Gemini 4 Neon (Medium)"}"#,
        )
        .unwrap();

        let cl = build_claude_catalog(Some(&dir), &["claude-opus-5-5".into()]);
        assert_eq!(cl.default, "fable");
        assert_eq!(cl.default_effort, "max");
        assert_eq!(cl.efforts, vec!["low", "medium", "high", "max"]);
        assert_eq!(cl.models[0].efforts, vec!["low", "medium", "high", "max"]);

        let cx = build_codex_catalog(Some(&dir), &["gpt-6.2-astra".into()]);
        assert_eq!(cx.default, "gpt-6.2-sol");
        assert_eq!(cx.default_effort, "xhigh");
        assert_eq!(cx.efforts, vec!["low", "medium", "high", "xhigh"]);

        // Non-existent agy binary falls back cleanly to built-in aggregated catalog.
        let ag = build_agy_catalog(Some(&dir), "/nonexistent/agy", &[]);
        assert_eq!(ag.default, "gemini-4-neon");
        assert_eq!(ag.default_effort, "medium");
        let orch = ag
            .models
            .iter()
            .find(|m| m.label == "Orchestrator (Neon)")
            .unwrap();
        assert!(orch.efforts.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
