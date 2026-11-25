//! Sanctioned read-only folder-trust evaluation and per-session CLI ancestor-trust inheritance.
//!
//! Security invariants:
//! - NEVER mutates `~/.codex/config.toml`, `~/.claude.json`, or `~/.gemini/antigravity-cli/settings.json`.
//! - NEVER trusts a directory outside the user's already-trusted ancestor directories.
//! - NEVER overrides an explicit `untrusted` / `hasTrustDialogAccepted: false` entry on the target
//!   directory or on any closer ancestor than a trusted outer directory.
//! - Emits only official per-session CLI overrides:
//!   - **Codex**: `-c 'projects={"<canon>"={trust_level="trusted"}}'` (deep-merges with `config.toml` `[projects]`).
//!   - **AGY**: `--settings '{"trustedWorkspaces":[...]}'` (session-only, never saved to `settings.json`).
//!   - **Claude Code**: no CLI flag exists; keeps `~/.claude.json` read-only and relies on
//!     trust-screen detection (`SessionState::NeedsPermission`) in `gate.rs` + `discover.rs`.

use starfane_proto::AgentKind;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustMark {
    Trusted,
    Untrusted,
}

/// Result of evaluating whether `cwd` is covered by the user's existing agent trust config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderTrustDecision {
    /// Exact canonical directory is already explicitly marked trusted in the user's config.
    AlreadyTrusted,
    /// Directory is not exact-listed (or listed only under a symlink alias), is not explicitly
    /// untrusted (nor under a closer untrusted ancestor), and inherits from an explicitly trusted
    /// ancestor path.
    InheritFromAncestor {
        trusted_ancestor: PathBuf,
        canonical_cwd: PathBuf,
    },
    /// Directory itself or a closer ancestor than any trusted entry is explicitly untrusted.
    ExplicitlyUntrusted { untrusted_path: PathBuf },
    /// No trusted config entry covers this directory.
    Untrusted,
}

fn expand_home(raw: &str, home: Option<&Path>) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// Lexically normalize `.` / `..` / redundant separators without requiring the path to exist.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// Canonicalize when the path exists on disk; otherwise return the lexically normalized path.
fn canonical_or_normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

fn record_entry(entries: &mut HashMap<PathBuf, TrustMark>, key: PathBuf, mark: TrustMark) {
    // Fail-safe: if two aliases resolve to the same normalized key, explicit Untrusted wins.
    match entries.get(&key) {
        Some(TrustMark::Untrusted) => {}
        _ => {
            entries.insert(key, mark);
        }
    }
}

fn evaluate_entries(
    raw_entries: &[(String, TrustMark)],
    cwd: &Path,
    home: Option<&Path>,
) -> FolderTrustDecision {
    let cwd_lex = lexical_normalize(&expand_home(&cwd.to_string_lossy(), home));
    let cwd_canon = canonical_or_normalized(&cwd_lex);

    let mut exact_trusted_keys: HashMap<PathBuf, ()> = HashMap::new();
    let mut norm_entries: HashMap<PathBuf, TrustMark> = HashMap::new();

    for (raw_key, mark) in raw_entries {
        if raw_key.trim().is_empty() {
            continue;
        }
        let expanded = expand_home(raw_key, home);
        let lex = lexical_normalize(&expanded);
        if *mark == TrustMark::Trusted {
            exact_trusted_keys.insert(lex.clone(), ());
        }
        record_entry(&mut norm_entries, lex.clone(), *mark);
        if let Ok(canon) = std::fs::canonicalize(&lex) {
            if canon != lex {
                record_entry(&mut norm_entries, canon, *mark);
            }
        }
    }

    // 1. Check the exact target directory first (both canonical and lexical forms).
    // Explicit Untrusted ALWAYS wins immediately.
    if norm_entries.get(&cwd_canon) == Some(&TrustMark::Untrusted) {
        return FolderTrustDecision::ExplicitlyUntrusted {
            untrusted_path: cwd_canon,
        };
    }
    if cwd_lex != cwd_canon && norm_entries.get(&cwd_lex) == Some(&TrustMark::Untrusted) {
        return FolderTrustDecision::ExplicitlyUntrusted {
            untrusted_path: cwd_lex,
        };
    }

    // Exact canonical key already present as Trusted -> agent will already trust it natively.
    if exact_trusted_keys.contains_key(&cwd_canon) {
        return FolderTrustDecision::AlreadyTrusted;
    }
    // Symlink-equivalent exact key (e.g. `/tmp/repo` vs `/private/tmp/repo`) -> pass canonical_cwd.
    if norm_entries.get(&cwd_canon) == Some(&TrustMark::Trusted) {
        return FolderTrustDecision::InheritFromAncestor {
            trusted_ancestor: cwd_lex,
            canonical_cwd: cwd_canon,
        };
    }
    if exact_trusted_keys.contains_key(&cwd_lex) {
        return FolderTrustDecision::InheritFromAncestor {
            trusted_ancestor: cwd_lex,
            canonical_cwd: cwd_canon,
        };
    }

    // 2. Walk strict ancestors from nearest parent upward to root.
    // Nearest listed ancestor wins: a closer explicit Untrusted blocks any outer Trusted ancestor.
    for ancestor in cwd_canon.ancestors().skip(1) {
        if let Some(mark) = norm_entries.get(ancestor) {
            return match mark {
                TrustMark::Trusted => FolderTrustDecision::InheritFromAncestor {
                    trusted_ancestor: ancestor.to_path_buf(),
                    canonical_cwd: cwd_canon,
                },
                TrustMark::Untrusted => FolderTrustDecision::ExplicitlyUntrusted {
                    untrusted_path: ancestor.to_path_buf(),
                },
            };
        }
    }

    // Also check lexical ancestors if canonicalization diverged (e.g. symlinked parent dir).
    if cwd_lex != cwd_canon {
        for ancestor in cwd_lex.ancestors().skip(1) {
            if let Some(mark) = norm_entries.get(ancestor) {
                return match mark {
                    TrustMark::Trusted => FolderTrustDecision::InheritFromAncestor {
                        trusted_ancestor: ancestor.to_path_buf(),
                        canonical_cwd: cwd_canon,
                    },
                    TrustMark::Untrusted => FolderTrustDecision::ExplicitlyUntrusted {
                        untrusted_path: ancestor.to_path_buf(),
                    },
                };
            }
        }
    }

    FolderTrustDecision::Untrusted
}

/// Evaluate Codex folder trust strictly from a `~/.codex/config.toml` document (pure, read-only).
pub fn evaluate_codex_trust_from_toml(
    toml_str: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> FolderTrustDecision {
    let Ok(table) = toml_str.parse::<toml::Table>() else {
        return FolderTrustDecision::Untrusted;
    };
    let Some(projects) = table.get("projects").and_then(toml::Value::as_table) else {
        return FolderTrustDecision::Untrusted;
    };
    let mut entries = Vec::with_capacity(projects.len());
    for (path, val) in projects {
        let Some(level) = val
            .as_table()
            .and_then(|t| t.get("trust_level"))
            .and_then(toml::Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
        else {
            continue;
        };
        match level.as_str() {
            "trusted" => entries.push((path.clone(), TrustMark::Trusted)),
            "untrusted" => entries.push((path.clone(), TrustMark::Untrusted)),
            _ => {}
        }
    }
    evaluate_entries(&entries, cwd, home)
}

/// Read-only inspection of `<home>/.codex/config.toml` for Codex folder trust.
pub fn evaluate_codex_trust(home: &Path, cwd: &Path) -> FolderTrustDecision {
    let cfg_path = home.join(".codex").join("config.toml");
    let Ok(raw) = std::fs::read_to_string(cfg_path) else {
        return FolderTrustDecision::Untrusted;
    };
    evaluate_codex_trust_from_toml(&raw, cwd, Some(home))
}

/// Evaluate AGY folder trust strictly from `~/.gemini/antigravity-cli/settings.json` (pure, read-only).
/// Also returns the existing `trustedWorkspaces` list so session-only `--settings` overrides preserve them.
pub fn evaluate_agy_trust_from_json(
    json_str: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> (FolderTrustDecision, Vec<String>) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return (FolderTrustDecision::Untrusted, Vec::new());
    };
    let existing: Vec<String> = val
        .get("trustedWorkspaces")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let entries: Vec<(String, TrustMark)> = existing
        .iter()
        .map(|p| (p.clone(), TrustMark::Trusted))
        .collect();
    (evaluate_entries(&entries, cwd, home), existing)
}

/// Read-only inspection of `<home>/.gemini/antigravity-cli/settings.json` for AGY folder trust.
pub fn evaluate_agy_trust(home: &Path, cwd: &Path) -> (FolderTrustDecision, Vec<String>) {
    let cfg_path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");
    let Ok(raw) = std::fs::read_to_string(cfg_path) else {
        return (FolderTrustDecision::Untrusted, Vec::new());
    };
    evaluate_agy_trust_from_json(&raw, cwd, Some(home))
}

/// Evaluate Claude Code folder trust strictly from `~/.claude.json` (pure, read-only).
pub fn evaluate_claude_trust_from_json(
    json_str: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> FolderTrustDecision {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return FolderTrustDecision::Untrusted;
    };
    let Some(projects) = val.get("projects").and_then(serde_json::Value::as_object) else {
        return FolderTrustDecision::Untrusted;
    };
    let mut entries = Vec::with_capacity(projects.len());
    for (path, obj) in projects {
        let Some(accepted) = obj
            .get("hasTrustDialogAccepted")
            .and_then(serde_json::Value::as_bool)
        else {
            continue;
        };
        let mark = if accepted {
            TrustMark::Trusted
        } else {
            TrustMark::Untrusted
        };
        entries.push((path.clone(), mark));
    }
    evaluate_entries(&entries, cwd, home)
}

/// Read-only inspection of `<home>/.claude.json` for Claude Code folder trust.
pub fn evaluate_claude_trust(home: &Path, cwd: &Path) -> FolderTrustDecision {
    let cfg_path = home.join(".claude.json");
    let Ok(raw) = std::fs::read_to_string(cfg_path) else {
        return FolderTrustDecision::Untrusted;
    };
    evaluate_claude_trust_from_json(&raw, cwd, Some(home))
}

/// Build the official Codex CLI `-c` inline-table override for a canonical path:
/// `projects={"/abs/path"={trust_level="trusted"}}`.
///
/// Uses `toml::Value::String` formatting to guarantee valid TOML escaping for arbitrary paths
/// (quotes, backslashes, spaces, dots, etc.). Codex deep-merges this inline table with
/// `~/.codex/config.toml` `[projects]` for the spawned session only without writing to disk.
pub fn codex_trust_override_arg(canonical_cwd: &Path) -> String {
    let escaped = toml::Value::String(canonical_cwd.to_string_lossy().into_owned()).to_string();
    format!("projects={{{escaped}={{trust_level=\"trusted\"}}}}")
}

/// Build the official AGY CLI `--settings` JSON payload adding `canonical_cwd` to the session's
/// `trustedWorkspaces` list (preserving existing trusted workspaces; never saved to disk).
pub fn agy_trust_settings_json(existing_trusted: &[String], canonical_cwd: &Path) -> String {
    let canon_str = canonical_cwd.to_string_lossy().into_owned();
    let mut workspaces: Vec<String> = existing_trusted.to_vec();
    if !workspaces.iter().any(|w| w == &canon_str) {
        workspaces.push(canon_str);
    }
    serde_json::json!({ "trustedWorkspaces": workspaces }).to_string()
}

fn has_codex_projects_override(extra: &[String]) -> bool {
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        let a = arg.trim();
        if a == "-c" || a == "--config" {
            if let Some(val) = iter.next() {
                if val.trim_start().starts_with("projects") {
                    return true;
                }
            }
        } else if let Some(rest) = a.strip_prefix("--config=") {
            if rest.trim_start().starts_with("projects") {
                return true;
            }
        }
    }
    false
}

fn has_agy_settings_override(extra: &[String]) -> bool {
    extra.iter().any(|arg| {
        let a = arg.trim();
        a == "--settings" || a.starts_with("--settings=")
    })
}

/// Multiplexer & bind environment variables that Codex should forward into the `starfane` MCP
/// subprocess so multi-pane sessions sharing one repository resolve caller identity immediately.
pub const CODEX_MCP_ENV_VARS: [&str; 5] = [
    "TMUX_PANE",
    "TMUX",
    "ZELLIJ_PANE_ID",
    "ZELLIJ_SESSION_NAME",
    "STARFANE_BIND",
];

fn has_codex_mcp_env_vars_override(extra: &[String]) -> bool {
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        let a = arg.trim();
        if a == "-c" || a == "--config" {
            if let Some(val) = iter.next() {
                if val
                    .trim_start()
                    .starts_with("mcp_servers.starfane.env_vars")
                {
                    return true;
                }
            }
        } else if let Some(rest) = a.strip_prefix("--config=") {
            if rest
                .trim_start()
                .starts_with("mcp_servers.starfane.env_vars")
            {
                return true;
            }
        }
    }
    false
}

/// Pure computation of a Codex `-c mcp_servers.starfane.env_vars=[...]` override from
/// `~/.codex/config.toml` (read-only).
///
/// Returns `Some(override_str)` ONLY when `[mcp_servers.starfane]` already exists in `toml_str`
/// (with a `command`) AND is missing one or more of [`CODEX_MCP_ENV_VARS`]. Existing user
/// `env_vars` entries are preserved in order. Never creates a partial `[mcp_servers.starfane]`
/// table if the user has not registered the `starfane` MCP server.
pub fn codex_mcp_env_vars_override_from_toml(toml_str: &str) -> Option<String> {
    let table = toml_str.parse::<toml::Table>().ok()?;
    let starfane = table
        .get("mcp_servers")
        .and_then(toml::Value::as_table)?
        .get("starfane")
        .and_then(toml::Value::as_table)?;
    let has_command = starfane
        .get("command")
        .and_then(toml::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !has_command {
        return None;
    }
    let mut vars: Vec<String> = starfane
        .get("env_vars")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut added = false;
    for key in CODEX_MCP_ENV_VARS {
        if !vars.iter().any(|v| v == key) {
            vars.push(key.to_string());
            added = true;
        }
    }
    if !added {
        return None;
    }
    let array = toml::Value::Array(vars.into_iter().map(toml::Value::String).collect());
    Some(format!("mcp_servers.starfane.env_vars={array}"))
}

/// Append a per-session `-c mcp_servers.starfane.env_vars=[...]` override when spawning Codex if
/// `[mcp_servers.starfane]` is configured in `<home>/.codex/config.toml` without mux env vars.
/// Strictly read-only (never modifies `~/.codex/config.toml`).
pub fn ensure_codex_mcp_env_vars_arg(
    agent: AgentKind,
    home: Option<&Path>,
    extra: &mut Vec<String>,
) {
    if agent != AgentKind::Codex || has_codex_mcp_env_vars_override(extra) {
        return;
    }
    let Some(home) = home else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(home.join(".codex").join("config.toml")) else {
        return;
    };
    if let Some(ov) = codex_mcp_env_vars_override_from_toml(&raw) {
        extra.push("-c".into());
        extra.push(ov);
    }
}

/// Append sanctioned per-session ancestor-trust CLI flags to `extra` when spawning `agent` in `cwd`.
///
/// - **Codex**: reads `<home>/.codex/config.toml` (read-only). ONLY when `cwd` inherits trust from
///   an already-trusted ancestor (and is neither already exact-trusted nor explicitly untrusted /
///   under a closer untrusted ancestor), appends `["-c", "projects={\"<canon>\"={trust_level=\"trusted\"}}]`.
/// - **AGY**: reads `<home>/.gemini/antigravity-cli/settings.json` (read-only). ONLY when `cwd`
///   inherits trust from an already-trusted ancestor in `trustedWorkspaces`, appends
///   `["--settings", "{\"trustedWorkspaces\":[...]}"]` (session-only, never saved to disk).
/// - **Claude Code**: no CLI trust flag exists; strictly read-only no-op (never writes `~/.claude.json`).
/// - **Shell**: no-op.
pub fn ensure_ancestor_trust_args(
    agent: AgentKind,
    cwd: &Path,
    home: Option<&Path>,
    extra: &mut Vec<String>,
) {
    let Some(home) = home else {
        return;
    };
    match agent {
        AgentKind::Codex => {
            if has_codex_projects_override(extra) {
                return;
            }
            if let FolderTrustDecision::InheritFromAncestor { canonical_cwd, .. } =
                evaluate_codex_trust(home, cwd)
            {
                extra.push("-c".into());
                extra.push(codex_trust_override_arg(&canonical_cwd));
            }
        }
        AgentKind::Agy => {
            if has_agy_settings_override(extra) {
                return;
            }
            let (decision, existing) = evaluate_agy_trust(home, cwd);
            if let FolderTrustDecision::InheritFromAncestor { canonical_cwd, .. } = decision {
                extra.push("--settings".into());
                extra.push(agy_trust_settings_json(&existing, &canonical_cwd));
            }
        }
        AgentKind::Claude | AgentKind::Shell => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CODEX_TOML: &str = r#"
model = "gpt-6.2-sol"

[projects."/Users/wujie"]
trust_level = "trusted"

[projects."/Users/wujie/Work/workflow"]
trust_level = "trusted"

[projects."/Users/wujie/untrusted_zone"]
trust_level = "untrusted"
"#;

    #[test]
    fn codex_trust_exact_ancestor_untrusted_and_outside_cases() {
        // 1. Exact trusted dir -> AlreadyTrusted (no -c needed)
        assert_eq!(
            evaluate_codex_trust_from_toml(
                SAMPLE_CODEX_TOML,
                Path::new("/Users/wujie/Work/workflow"),
                Some(Path::new("/Users/wujie")),
            ),
            FolderTrustDecision::AlreadyTrusted
        );

        // 2. Subdir under trusted ancestor -> InheritFromAncestor
        assert_eq!(
            evaluate_codex_trust_from_toml(
                SAMPLE_CODEX_TOML,
                Path::new("/Users/wujie/Work/new_repo"),
                Some(Path::new("/Users/wujie")),
            ),
            FolderTrustDecision::InheritFromAncestor {
                trusted_ancestor: PathBuf::from("/Users/wujie"),
                canonical_cwd: PathBuf::from("/Users/wujie/Work/new_repo"),
            }
        );

        // 3a. Explicitly untrusted subdir under trusted ancestor -> MUST NOT inherit!
        assert_eq!(
            evaluate_codex_trust_from_toml(
                SAMPLE_CODEX_TOML,
                Path::new("/Users/wujie/untrusted_zone"),
                Some(Path::new("/Users/wujie")),
            ),
            FolderTrustDecision::ExplicitlyUntrusted {
                untrusted_path: PathBuf::from("/Users/wujie/untrusted_zone"),
            }
        );

        // 3b. Deeper child of explicitly untrusted subdir under trusted ancestor -> MUST NOT inherit!
        assert_eq!(
            evaluate_codex_trust_from_toml(
                SAMPLE_CODEX_TOML,
                Path::new("/Users/wujie/untrusted_zone/nested/repo"),
                Some(Path::new("/Users/wujie")),
            ),
            FolderTrustDecision::ExplicitlyUntrusted {
                untrusted_path: PathBuf::from("/Users/wujie/untrusted_zone"),
            }
        );

        // 4. Completely untrusted path outside trusted ancestors -> MUST NOT inherit!
        assert_eq!(
            evaluate_codex_trust_from_toml(
                SAMPLE_CODEX_TOML,
                Path::new("/tmp/foo"),
                Some(Path::new("/Users/wujie")),
            ),
            FolderTrustDecision::Untrusted
        );
    }

    #[test]
    fn claude_trust_nearest_ancestor_wins_and_honors_false() {
        let json = r#"{
            "projects": {
                "/Users/wujie": { "hasTrustDialogAccepted": false },
                "/Users/wujie/Work": { "hasTrustDialogAccepted": true },
                "/Users/wujie/Work/workflow": { "hasTrustDialogAccepted": true },
                "/Users/wujie/Work/do-as-i-do": { "hasTrustDialogAccepted": false }
            }
        }"#;

        assert_eq!(
            evaluate_claude_trust_from_json(json, Path::new("/Users/wujie/Work/workflow"), None),
            FolderTrustDecision::AlreadyTrusted
        );
        assert_eq!(
            evaluate_claude_trust_from_json(json, Path::new("/Users/wujie/Work/new_proj"), None),
            FolderTrustDecision::InheritFromAncestor {
                trusted_ancestor: PathBuf::from("/Users/wujie/Work"),
                canonical_cwd: PathBuf::from("/Users/wujie/Work/new_proj"),
            }
        );
        // Explicitly false subdir under trusted `/Users/wujie/Work` -> ExplicitlyUntrusted
        assert_eq!(
            evaluate_claude_trust_from_json(
                json,
                Path::new("/Users/wujie/Work/do-as-i-do/sub"),
                None
            ),
            FolderTrustDecision::ExplicitlyUntrusted {
                untrusted_path: PathBuf::from("/Users/wujie/Work/do-as-i-do"),
            }
        );
        // Sibling under untrusted `/Users/wujie` -> ExplicitlyUntrusted
        assert_eq!(
            evaluate_claude_trust_from_json(json, Path::new("/Users/wujie/random"), None),
            FolderTrustDecision::ExplicitlyUntrusted {
                untrusted_path: PathBuf::from("/Users/wujie"),
            }
        );
        // Outside all entries -> Untrusted
        assert_eq!(
            evaluate_claude_trust_from_json(json, Path::new("/tmp/foo"), None),
            FolderTrustDecision::Untrusted
        );
    }

    #[test]
    fn agy_trust_exact_ancestor_and_untrusted() {
        let json = r#"{
            "model": "Gemini 4 Neon (XHigh)",
            "trustedWorkspaces": [
                "/Users/wujie",
                "/Users/wujie/Work/workflow"
            ]
        }"#;

        let (d_exact, _) =
            evaluate_agy_trust_from_json(json, Path::new("/Users/wujie/Work/workflow"), None);
        assert_eq!(d_exact, FolderTrustDecision::AlreadyTrusted);

        let (d_anc, existing) =
            evaluate_agy_trust_from_json(json, Path::new("/Users/wujie/Work/other"), None);
        assert_eq!(
            d_anc,
            FolderTrustDecision::InheritFromAncestor {
                trusted_ancestor: PathBuf::from("/Users/wujie"),
                canonical_cwd: PathBuf::from("/Users/wujie/Work/other"),
            }
        );
        let payload: serde_json::Value = serde_json::from_str(&agy_trust_settings_json(
            &existing,
            Path::new("/Users/wujie/Work/other"),
        ))
        .unwrap();
        assert_eq!(
            payload["trustedWorkspaces"],
            serde_json::json!([
                "/Users/wujie",
                "/Users/wujie/Work/workflow",
                "/Users/wujie/Work/other"
            ])
        );

        let (d_out, _) = evaluate_agy_trust_from_json(json, Path::new("/tmp/foo"), None);
        assert_eq!(d_out, FolderTrustDecision::Untrusted);
    }

    #[test]
    fn ensure_ancestor_trust_args_emits_only_sanctioned_flags_and_never_touches_disk() {
        let root =
            std::env::temp_dir().join(format!("sf-trust-unit-{}", starfane_proto::id::new_id("T")));
        let home = root.join("home");
        let work_child = home.join("Work").join("new_repo");
        let untrusted_child = home.join("untrusted_zone").join("sub");
        let outside_dir = root.join("outside");
        std::fs::create_dir_all(&work_child).unwrap();
        std::fs::create_dir_all(&untrusted_child).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let canon_home = std::fs::canonicalize(&home).unwrap();
        let canon_child = std::fs::canonicalize(&work_child).unwrap();
        let canon_untrusted = std::fs::canonicalize(home.join("untrusted_zone")).unwrap();

        // Seed Codex config.toml
        let codex_cfg = home.join(".codex").join("config.toml");
        std::fs::create_dir_all(codex_cfg.parent().unwrap()).unwrap();
        let codex_toml = format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n\n[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
            canon_home.display(),
            canon_untrusted.display()
        );
        std::fs::write(&codex_cfg, &codex_toml).unwrap();

        // Seed AGY settings.json
        let agy_cfg = home.join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(agy_cfg.parent().unwrap()).unwrap();
        let agy_json = serde_json::json!({
            "trustedWorkspaces": [canon_home.to_string_lossy()]
        })
        .to_string();
        std::fs::write(&agy_cfg, &agy_json).unwrap();

        // Seed Claude .claude.json (must stay read-only!)
        let claude_cfg = home.join(".claude.json");
        let claude_json = serde_json::json!({
            "projects": {
                canon_home.to_string_lossy().to_string(): { "hasTrustDialogAccepted": true }
            }
        })
        .to_string();
        std::fs::write(&claude_cfg, &claude_json).unwrap();

        // Codex on trusted-ancestor subdir -> gets `-c projects={"<canon>"={trust_level="trusted"}}`
        let mut cx_args = Vec::new();
        ensure_ancestor_trust_args(AgentKind::Codex, &work_child, Some(&home), &mut cx_args);
        assert_eq!(
            cx_args,
            vec!["-c".to_string(), codex_trust_override_arg(&canon_child),]
        );
        // Verify the generated `-c` payload parses as valid TOML!
        let parsed: toml::Table = cx_args[1].parse().expect("valid TOML inline override");
        assert_eq!(
            parsed["projects"][canon_child.to_string_lossy().as_ref()]["trust_level"].as_str(),
            Some("trusted")
        );

        // Codex on explicitly untrusted subdir -> NO `-c`
        let mut cx_untrusted = Vec::new();
        ensure_ancestor_trust_args(
            AgentKind::Codex,
            &untrusted_child,
            Some(&home),
            &mut cx_untrusted,
        );
        assert!(cx_untrusted.is_empty());

        // Codex on completely untrusted outside dir -> NO `-c`
        let mut cx_outside = Vec::new();
        ensure_ancestor_trust_args(AgentKind::Codex, &outside_dir, Some(&home), &mut cx_outside);
        assert!(cx_outside.is_empty());

        // Codex with existing `-c projects=...` -> does not duplicate
        let mut cx_existing = vec!["-c".into(), "projects={}".into()];
        ensure_ancestor_trust_args(AgentKind::Codex, &work_child, Some(&home), &mut cx_existing);
        assert_eq!(cx_existing.len(), 2);

        // AGY on trusted-ancestor subdir -> gets `--settings {"trustedWorkspaces":[...]}`
        let mut ag_args = Vec::new();
        ensure_ancestor_trust_args(AgentKind::Agy, &work_child, Some(&home), &mut ag_args);
        assert_eq!(ag_args.len(), 2);
        assert_eq!(ag_args[0], "--settings");
        let parsed_agy: serde_json::Value = serde_json::from_str(&ag_args[1]).unwrap();
        let tw = parsed_agy["trustedWorkspaces"].as_array().unwrap();
        assert!(tw
            .iter()
            .any(|v| v.as_str() == Some(canon_child.to_string_lossy().as_ref())));

        // AGY on outside dir -> NO `--settings`
        let mut ag_outside = Vec::new();
        ensure_ancestor_trust_args(AgentKind::Agy, &outside_dir, Some(&home), &mut ag_outside);
        assert!(ag_outside.is_empty());

        // Claude on trusted-ancestor subdir -> strictly NO CLI flag and ~/.claude.json untouched!
        let mut cl_args = Vec::new();
        ensure_ancestor_trust_args(AgentKind::Claude, &work_child, Some(&home), &mut cl_args);
        assert!(cl_args.is_empty());

        // Verify ALL three config files are byte-for-byte untouched!
        assert_eq!(std::fs::read_to_string(&codex_cfg).unwrap(), codex_toml);
        assert_eq!(std::fs::read_to_string(&agy_cfg).unwrap(), agy_json);
        assert_eq!(std::fs::read_to_string(&claude_cfg).unwrap(), claude_json);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_override_escapes_special_chars_in_path() {
        let arg = codex_trust_override_arg(Path::new(
            r#"/tmp/dir with "quotes" & \slash/.agent_session"#,
        ));
        let parsed: toml::Table = arg.parse().expect("escaped TOML parses cleanly");
        assert_eq!(
            parsed["projects"][r#"/tmp/dir with "quotes" & \slash/.agent_session"#]["trust_level"]
                .as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn codex_mcp_env_vars_override_merges_only_when_starfane_server_registered() {
        // Without [mcp_servers.starfane] -> None (must not fabricate a broken server entry).
        assert_eq!(
            codex_mcp_env_vars_override_from_toml("model = \"gpt-6.2-sol\"\n"),
            None
        );

        // With [mcp_servers.starfane] lacking env_vars -> adds all 5 mux/bind vars.
        let bare = r#"
[mcp_servers.starfane]
command = "/usr/local/bin/starfane"
args = ["mcp", "--agent", "codex"]
"#;
        let ov = codex_mcp_env_vars_override_from_toml(bare).expect("expected override");
        let parsed: toml::Table = ov.parse().expect("valid TOML");
        let arr = parsed["mcp_servers"]["starfane"]["env_vars"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = arr.iter().filter_map(toml::Value::as_str).collect();
        assert_eq!(names, CODEX_MCP_ENV_VARS);

        // Preserves existing custom env_vars and avoids duplicates.
        let partial = r#"
[mcp_servers.starfane]
command = "/usr/local/bin/starfane"
env_vars = ["CUSTOM_VAR", "TMUX_PANE"]
"#;
        let ov2 = codex_mcp_env_vars_override_from_toml(partial).expect("expected merged override");
        let parsed2: toml::Table = ov2.parse().unwrap();
        let names2: Vec<&str> = parsed2["mcp_servers"]["starfane"]["env_vars"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(
            names2,
            vec![
                "CUSTOM_VAR",
                "TMUX_PANE",
                "TMUX",
                "ZELLIJ_PANE_ID",
                "ZELLIJ_SESSION_NAME",
                "STARFANE_BIND",
            ]
        );

        // Already complete -> None (idempotent).
        let complete = r#"
[mcp_servers.starfane]
command = "/usr/local/bin/starfane"
env_vars = ["TMUX_PANE", "TMUX", "ZELLIJ_PANE_ID", "ZELLIJ_SESSION_NAME", "STARFANE_BIND"]
"#;
        assert_eq!(codex_mcp_env_vars_override_from_toml(complete), None);
    }
}
