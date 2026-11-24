//! Non-invasive, reversible hook + MCP install/uninstall planner and applier (contracts §3 & §3.1).
//!
//! Hard invariants:
//! - Never touch `~/.codex/config.toml` (preserves user `notify`, Computer Use, etc.).
//! - Preserve every non-starfane hook entry (e.g., `PostToolUse` `fix-after-edit.sh`).
//! - Backup every modified file before atomic write, re-parse after write, rollback on failure.

use crate::error::AdapterError;
use serde::Serialize;
use serde_json::{json, Map, Value};
use starfane_proto::{id::now_ms, AgentKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

const AGY_FLAT_EVENTS: &[&str] = &["SessionStart", "PreInvocation", "Stop"];
const AGY_TOOL_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];
const AGY_TOOL_MATCHER: &str = "run_command|write_file|edit_file";
const AGY_HOOK_GROUP_KEY: &str = "starfane";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    User,
    Project(PathBuf),
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub scope: Scope,
    pub dry_run: bool,
    /// Absolute path to the `starfane` binary written into hook commands.
    pub starfane_bin: PathBuf,
    /// Default `false`; when `true`, adds side-effecting `PreToolUse`/`PostToolUse` hooks for agy.
    pub agy_permission_heuristic: bool,
    /// Default `true`; adds official CLI commands to register `starfane` MCP server.
    pub register_mcp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallPlan {
    pub agent: AgentKind,
    pub file_changes: Vec<FileChange>,
    /// Official CLI commands for MCP registration / removal.
    pub commands: Vec<Vec<String>>,
    /// Operator notes (e.g., Codex hook trust hash guidance).
    pub notes: Vec<String>,
}

/// Return the hook configuration path for `(agent, home, scope)` per contracts §3.1.
pub fn hook_config_path(agent: AgentKind, home: &Path, scope: &Scope) -> PathBuf {
    match (agent, scope) {
        (AgentKind::Claude, Scope::User) => home.join(".claude").join("settings.json"),
        (AgentKind::Claude, Scope::Project(root)) => {
            root.join(".claude").join("settings.local.json")
        }
        (AgentKind::Codex, Scope::User) => home.join(".codex").join("hooks.json"),
        (AgentKind::Codex, Scope::Project(root)) => root.join(".codex").join("hooks.json"),
        (AgentKind::Agy, Scope::User) => home.join(".gemini").join("config").join("hooks.json"),
        (AgentKind::Agy, Scope::Project(root)) => root.join(".agents").join("hooks.json"),
    }
}

/// Compute a non-invasive install plan for `agent` without mutating disk.
pub fn install_plan(
    agent: AgentKind,
    home: &Path,
    opts: &InstallOptions,
) -> Result<InstallPlan, AdapterError> {
    let path = hook_config_path(agent, home, &opts.scope);
    let (before, mut root) = read_json_object(&path)?;

    let marker_prefix = starfane_command_prefix(&opts.starfane_bin);
    match agent {
        AgentKind::Claude => {
            upsert_grouped_hooks(
                &mut root,
                CLAUDE_EVENTS,
                agent,
                &opts.starfane_bin,
                &marker_prefix,
            )?;
        }
        AgentKind::Codex => {
            upsert_grouped_hooks(
                &mut root,
                CODEX_EVENTS,
                agent,
                &opts.starfane_bin,
                &marker_prefix,
            )?;
        }
        AgentKind::Agy => {
            upsert_agy_hooks(&mut root, &opts.starfane_bin, opts.agy_permission_heuristic);
        }
    }

    let after = Some(format_pretty_json(&Value::Object(root))?);
    let mut file_changes = Vec::new();
    if before != after {
        file_changes.push(FileChange {
            path,
            before,
            after,
        });
    }

    let commands = if opts.register_mcp {
        vec![mcp_add_command(agent, &opts.scope, &opts.starfane_bin)]
    } else {
        Vec::new()
    };

    let mut notes = Vec::new();
    if agent == AgentKind::Codex {
        notes.push(
            "Codex enforces SHA-256 hook trust ([hooks.state] in ~/.codex/config.toml). \
             Approve the new starfane hook entries on next Codex launch (starfane never edits config.toml `notify`)."
                .to_string(),
        );
    }
    if agent == AgentKind::Agy && opts.agy_permission_heuristic {
        notes.push(
            "Enabled opt-in agy PreToolUse/PostToolUse hooks (matcher: run_command|write_file|edit_file) \
             for permission-wait heuristics."
                .to_string(),
        );
    }

    Ok(InstallPlan {
        agent,
        file_changes,
        commands,
        notes,
    })
}

/// Compute an uninstall plan removing only starfane-owned hook entries and MCP registration.
pub fn uninstall_plan(
    agent: AgentKind,
    home: &Path,
    scope: &Scope,
    starfane_bin: &Path,
) -> Result<InstallPlan, AdapterError> {
    let path = hook_config_path(agent, home, scope);
    let (before, mut root) = read_json_object(&path)?;

    let mut file_changes = Vec::new();
    if before.is_some() {
        let marker_prefix = starfane_command_prefix(starfane_bin);
        match agent {
            AgentKind::Claude | AgentKind::Codex => {
                remove_grouped_hooks(&mut root, &marker_prefix);
            }
            AgentKind::Agy => {
                root.remove(AGY_HOOK_GROUP_KEY);
            }
        }

        let after = if root.is_empty() {
            None
        } else {
            Some(format_pretty_json(&Value::Object(root))?)
        };

        if before != after {
            file_changes.push(FileChange {
                path,
                before,
                after,
            });
        }
    }

    let commands = vec![mcp_remove_command(agent, scope)];

    Ok(InstallPlan {
        agent,
        file_changes,
        commands,
        notes: Vec::new(),
    })
}

/// Apply an [`InstallPlan`] with full backup, atomic writes, JSON verification, and rollback on failure.
pub fn apply_plan(plan: &InstallPlan, backups_root: &Path) -> Result<PathBuf, AdapterError> {
    let backup_dir = backups_root.join(now_ms().to_string());
    fs::create_dir_all(&backup_dir).map_err(|e| AdapterError::io(backup_dir.clone(), e))?;

    // Track applied changes so we can roll back if write/re-parse fails.
    let mut backups: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

    for change in &plan.file_changes {
        let backup_copy = if change.path.exists() {
            let rel = change.path.strip_prefix("/").unwrap_or(&change.path);
            let dst = backup_dir.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| AdapterError::io(parent.to_path_buf(), e))?;
            }
            fs::copy(&change.path, &dst).map_err(|e| AdapterError::io(change.path.clone(), e))?;
            Some(dst)
        } else {
            None
        };
        backups.push((change.path.clone(), backup_copy));

        if let Err(err) = apply_single_change(change) {
            rollback_changes(&backups);
            return Err(err);
        }
    }

    for argv in &plan.commands {
        let Some((prog, args)) = argv.split_first() else {
            continue;
        };
        let output = Command::new(prog)
            .args(args)
            .output()
            .map_err(|e| AdapterError::io(None, e))?;
        if !output.status.success() {
            return Err(AdapterError::CommandFailed {
                cmd: argv.join(" "),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
    }

    Ok(backup_dir)
}

fn apply_single_change(change: &FileChange) -> Result<(), AdapterError> {
    match &change.after {
        Some(content) => {
            if let Some(parent) = change.path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| AdapterError::io(parent.to_path_buf(), e))?;
            }
            let tmp_name = format!(
                ".{}.starfane.tmp.{}",
                change
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("config"),
                std::process::id()
            );
            let tmp_path = change
                .path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(tmp_name);

            fs::write(&tmp_path, content.as_bytes())
                .map_err(|e| AdapterError::io(tmp_path.clone(), e))?;
            fs::rename(&tmp_path, &change.path)
                .map_err(|e| AdapterError::io(change.path.clone(), e))?;

            // Read back and re-parse JSON to guarantee file integrity (contracts §3).
            let reread = fs::read_to_string(&change.path)
                .map_err(|e| AdapterError::io(change.path.clone(), e))?;
            let _: Value = serde_json::from_str(&reread)
                .map_err(|e| AdapterError::json(change.path.clone(), e))?;
            Ok(())
        }
        None => {
            if change.path.exists() {
                fs::remove_file(&change.path)
                    .map_err(|e| AdapterError::io(change.path.clone(), e))?;
            }
            Ok(())
        }
    }
}

fn rollback_changes(backups: &[(PathBuf, Option<PathBuf>)]) {
    for (target, backup) in backups.iter().rev() {
        match backup {
            Some(src) => {
                let _ = fs::copy(src, target);
            }
            None => {
                let _ = fs::remove_file(target);
            }
        }
    }
}

fn read_json_object(path: &Path) -> Result<(Option<String>, Map<String, Value>), AdapterError> {
    if !path.exists() {
        return Ok((None, Map::new()));
    }
    let raw = fs::read_to_string(path).map_err(|e| AdapterError::io(path.to_path_buf(), e))?;
    if raw.trim().is_empty() {
        return Ok((Some(raw), Map::new()));
    }
    let val: Value =
        serde_json::from_str(&raw).map_err(|e| AdapterError::json(path.to_path_buf(), e))?;
    let obj = val
        .as_object()
        .cloned()
        .ok_or_else(|| AdapterError::json(path.to_path_buf(), "expected top-level JSON object"))?;
    Ok((Some(raw), obj))
}

fn format_pretty_json(v: &Value) -> Result<String, AdapterError> {
    let mut s = serde_json::to_string_pretty(v).map_err(|e| AdapterError::json(None, e))?;
    s.push('\n');
    Ok(s)
}

pub(crate) fn starfane_command_prefix(starfane_bin: &Path) -> String {
    format!("{} emit --agent ", starfane_bin.display())
}

fn upsert_grouped_hooks(
    root: &mut Map<String, Value>,
    events: &[&str],
    agent: AgentKind,
    starfane_bin: &Path,
    marker_prefix: &str,
) -> Result<(), AdapterError> {
    let hooks_val = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks_val
        .as_object_mut()
        .ok_or_else(|| AdapterError::json(None, "`hooks` field must be a JSON object"))?;

    for &event in events {
        let cmd = format!(
            "{} emit --agent {} --event {event}",
            starfane_bin.display(),
            agent.as_str()
        );
        let starfane_entry = json!({
            "hooks": [
                {
                    "type": "command",
                    "command": cmd,
                    "async": true,
                    "timeout": 5
                }
            ]
        });

        let arr_val = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = arr_val.as_array_mut().ok_or_else(|| {
            AdapterError::json(None, format!("`hooks.{event}` must be a JSON array"))
        })?;

        arr.retain(|entry| !is_starfane_grouped_entry(entry, marker_prefix));
        arr.push(starfane_entry);
    }
    Ok(())
}

fn remove_grouped_hooks(root: &mut Map<String, Value>, marker_prefix: &str) {
    let Some(hooks_obj) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };

    let mut empty_events = Vec::new();
    for (event, val) in hooks_obj.iter_mut() {
        if let Some(arr) = val.as_array_mut() {
            arr.retain(|entry| !is_starfane_grouped_entry(entry, marker_prefix));
            if arr.is_empty() {
                empty_events.push(event.clone());
            }
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }

    if hooks_obj.is_empty() {
        root.remove("hooks");
    }
}

pub(crate) fn is_starfane_grouped_entry(entry: &Value, marker_prefix: &str) -> bool {
    let Some(handlers) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    handlers.iter().any(|h| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with(marker_prefix))
    })
}

fn upsert_agy_hooks(
    root: &mut Map<String, Value>,
    starfane_bin: &Path,
    permission_heuristic: bool,
) {
    let mut group = Map::new();
    for &event in AGY_FLAT_EVENTS {
        let cmd = format!(
            "{} emit --agent agy --event {event}",
            starfane_bin.display()
        );
        group.insert(
            event.to_string(),
            json!([
                {
                    "type": "command",
                    "command": cmd,
                    "timeout": 5
                }
            ]),
        );
    }

    if permission_heuristic {
        for &event in AGY_TOOL_EVENTS {
            let cmd = format!(
                "{} emit --agent agy --event {event}",
                starfane_bin.display()
            );
            group.insert(
                event.to_string(),
                json!([
                    {
                        "matcher": AGY_TOOL_MATCHER,
                        "hooks": [
                            {
                                "type": "command",
                                "command": cmd,
                                "timeout": 5
                            }
                        ]
                    }
                ]),
            );
        }
    }

    root.insert(AGY_HOOK_GROUP_KEY.to_string(), Value::Object(group));
}

fn mcp_add_command(agent: AgentKind, scope: &Scope, starfane_bin: &Path) -> Vec<String> {
    let bin = starfane_bin.display().to_string();
    match agent {
        AgentKind::Claude => {
            let scope_flag = match scope {
                Scope::User => "user",
                Scope::Project(_) => "project",
            };
            vec![
                "claude".into(),
                "mcp".into(),
                "add".into(),
                "-s".into(),
                scope_flag.into(),
                "starfane".into(),
                "--".into(),
                bin,
                "mcp".into(),
                "--agent".into(),
                "claude".into(),
            ]
        }
        AgentKind::Codex => vec![
            "codex".into(),
            "mcp".into(),
            "add".into(),
            "starfane".into(),
            "--".into(),
            bin,
            "mcp".into(),
            "--agent".into(),
            "codex".into(),
        ],
        AgentKind::Agy => vec![
            "agy".into(),
            "mcp".into(),
            "add".into(),
            "starfane".into(),
            "--".into(),
            bin,
            "mcp".into(),
            "--agent".into(),
            "agy".into(),
        ],
    }
}

fn mcp_remove_command(agent: AgentKind, scope: &Scope) -> Vec<String> {
    match agent {
        AgentKind::Claude => {
            let scope_flag = match scope {
                Scope::User => "user",
                Scope::Project(_) => "project",
            };
            vec![
                "claude".into(),
                "mcp".into(),
                "remove".into(),
                "-s".into(),
                scope_flag.into(),
                "starfane".into(),
            ]
        }
        AgentKind::Codex => vec![
            "codex".into(),
            "mcp".into(),
            "remove".into(),
            "starfane".into(),
        ],
        AgentKind::Agy => vec![
            "agy".into(),
            "mcp".into(),
            "remove".into(),
            "starfane".into(),
        ],
    }
}
