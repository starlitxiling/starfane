//! L4 interactive spawn command builder (contracts §3; zero headless).

use crate::error::AdapterError;
use starfane_proto::{AgentKind, Effort};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub agent: AgentKind,
    pub bin: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub name: Option<String>,
    /// Single-line pointer prompt (may carry a `/swarm ` prefix).
    pub prompt: String,
    /// Extra user-configured args from `.starfane/team.toml` `[spawn]`.
    pub extra_args: Vec<String>,
}

/// True for AGY models that have a fixed reasoning configuration (e.g. Orchestrator variants)
/// and must NOT receive a `-<effort>` suffix or `--effort` flag.
pub fn is_fixed_effort_agy_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("orchestrator")
}

/// Resolve an AGY `(model, effort)` pair into the CLI `--model` identifier accepted by `agy`.
/// - Fixed-effort models (e.g. `gemini-pro-orchestrator-neon`) are returned verbatim.
/// - When `effort` is explicitly provided, any existing effort suffix (`-low`/`-medium`/`-high`/`-xhigh`/`-max`)
///   is replaced so explicit caller overrides (e.g. `gemini-4-neon-high` + `Xhigh` -> `gemini-4-neon-xhigh`)
///   are honored deterministically.
/// - When `effort` is `None`, models that already carry a suffix (or bare models) are returned verbatim.
pub fn resolve_agy_model_id(model: &str, effort: Option<Effort>) -> String {
    let m = model.trim();
    if is_fixed_effort_agy_model(m) {
        return m.to_string();
    }
    let Some(e) = effort else {
        return m.to_string();
    };
    let lower = m.to_ascii_lowercase();
    let mut base = m;
    for suffix in ["-low", "-medium", "-high", "-xhigh", "-max"] {
        if lower.ends_with(suffix) && m.len() > suffix.len() {
            base = &m[..m.len() - suffix.len()];
            break;
        }
    }
    format!("{base}-{}", e.as_str())
}

/// Default CLI flag granting full tool/command permissions in newly spawned interactive sessions.
pub const fn default_bypass_permission_flag(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "--dangerously-skip-permissions",
        AgentKind::Codex => "--dangerously-bypass-approvals-and-sandbox",
        AgentKind::Agy => "--dangerously-skip-permissions",
        AgentKind::Shell => "",
    }
}

/// Ensure `extra` includes the agent's default full-permission flag unless the caller or
/// `.starfane/team.toml` already specified an explicit permission/sandbox flag.
pub fn ensure_bypass_permission_arg(agent: AgentKind, extra: &mut Vec<String>) {
    if agent.is_shell() {
        return;
    }
    let has_explicit = extra.iter().any(|arg| {
        let a = arg.trim();
        match agent {
            AgentKind::Claude => {
                a == "--dangerously-skip-permissions"
                    || a == "--allow-dangerously-skip-permissions"
                    || a == "--permission-mode"
                    || a.starts_with("--permission-mode=")
            }
            AgentKind::Codex => {
                a == "--dangerously-bypass-approvals-and-sandbox"
                    || a == "--approve-for-me"
                    || a == "-s"
                    || a == "--sandbox"
                    || a.starts_with("--sandbox=")
                    || a == "-a"
                    || a == "--ask-for-approval"
                    || a.starts_with("--ask-for-approval=")
            }
            AgentKind::Agy => {
                a == "--dangerously-skip-permissions"
                    || a == "--mode"
                    || a.starts_with("--mode=")
                    || a == "--sandbox"
            }
            AgentKind::Shell => true,
        }
    });
    if !has_explicit {
        extra.insert(0, default_bypass_permission_flag(agent).to_string());
    }
}

/// Build the argv passed after `tmux new-window ... --` (contracts §3).
///
/// - Claude: `[bin, "--model", m, "--effort", e, "-n", name, ...extra, prompt]`
/// - Codex:  `[bin, "-m", m, "-c", "model_reasoning_effort=\"<e>\"", "-C", cwd, ...extra, prompt]`
/// - Agy:    `[bin, "--model", m, "--effort", e, ...extra, "-i", prompt]`
///
/// Strictly interactive (never `-p` / `exec`).
pub fn build_l4_spawn_cmd(req: &SpawnRequest) -> Result<Vec<String>, AdapterError> {
    if req.agent.is_shell() {
        return Err(AdapterError::InvalidInput(
            "cannot spawn plain shell via coding-agent launcher".into(),
        ));
    }
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(AdapterError::InvalidInput(
            "spawn prompt cannot be empty".into(),
        ));
    }
    if prompt.contains('\n') || prompt.contains('\r') {
        return Err(AdapterError::InvalidInput(
            "spawn prompt must be single-line (use L0 MCP inbox for long bodies)".into(),
        ));
    }

    let bin = req.bin.to_string_lossy().into_owned();
    if bin.trim().is_empty() {
        return Err(AdapterError::InvalidInput(
            "agent binary path cannot be empty".into(),
        ));
    }

    let mut argv = vec![bin];

    match req.agent {
        AgentKind::Claude => {
            if let Some(m) = req.model.as_deref().filter(|s| !s.trim().is_empty()) {
                argv.push("--model".into());
                argv.push(m.to_string());
            }
            if let Some(e) = req.effort {
                argv.push("--effort".into());
                argv.push(e.as_str().to_string());
            }
            if let Some(n) = req.name.as_deref().filter(|s| !s.trim().is_empty()) {
                argv.push("-n".into());
                argv.push(n.to_string());
            }
            argv.extend(req.extra_args.iter().cloned());
            argv.push(prompt.to_string());
        }
        AgentKind::Codex => {
            if let Some(m) = req.model.as_deref().filter(|s| !s.trim().is_empty()) {
                argv.push("-m".into());
                argv.push(m.to_string());
            }
            if let Some(e) = req.effort {
                let mapped = match e {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh | Effort::Max => "xhigh",
                };
                argv.push("-c".into());
                argv.push(format!("model_reasoning_effort=\"{mapped}\""));
            }
            argv.push("-C".into());
            argv.push(req.cwd.to_string_lossy().into_owned());
            argv.extend(req.extra_args.iter().cloned());
            argv.push(prompt.to_string());
        }
        AgentKind::Agy => {
            let raw_model = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let fixed = raw_model.is_some_and(is_fixed_effort_agy_model);
            if let Some(m) = raw_model {
                argv.push("--model".into());
                argv.push(resolve_agy_model_id(m, req.effort));
            }
            if let Some(e) = req.effort {
                if !fixed {
                    argv.push("--effort".into());
                    argv.push(e.as_str().to_string());
                }
            }
            argv.extend(req.extra_args.iter().cloned());
            argv.push("-i".into());
            argv.push(prompt.to_string());
        }
        AgentKind::Shell => {
            return Err(AdapterError::InvalidInput(
                "cannot spawn plain shell via coding-agent launcher".into(),
            ));
        }
    }

    Ok(argv)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    pub agent: AgentKind,
    pub bin: Option<PathBuf>,
    pub resume_id: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub bypass_permissions: bool,
}

/// POSIX shell single-quote escaping (`'` -> `'\''`).
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn normalize_resume_effort(raw: Option<&str>) -> Option<Effort> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// Build an interactive agent resume argv (`claude --resume`, `codex resume`, `agy --resume`).
pub fn build_resume_cmd(req: &ResumeRequest) -> Result<Vec<String>, AdapterError> {
    if req.agent.is_shell() {
        return Err(AdapterError::InvalidInput(
            "cannot resume plain shell via coding-agent launcher".into(),
        ));
    }
    let rid = req.resume_id.trim();
    if rid.is_empty() || rid.chars().any(char::is_control) {
        return Err(AdapterError::InvalidInput(
            "resume_id must be non-empty and free of control characters".into(),
        ));
    }
    let bin = req
        .bin
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| req.agent.as_str().to_string());
    let parsed_effort = normalize_resume_effort(req.effort.as_deref());
    let mut argv = vec![bin];

    match req.agent {
        AgentKind::Claude => {
            argv.push("--resume".into());
            argv.push(rid.to_string());
            if let Some(m) = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                argv.push("--model".into());
                argv.push(m.to_string());
            }
            if let Some(e) = parsed_effort {
                argv.push("--effort".into());
                argv.push(e.as_str().to_string());
            }
            if req.bypass_permissions {
                argv.push("--dangerously-skip-permissions".into());
            }
        }
        AgentKind::Codex => {
            // Options must precede positional `[SESSION_ID]` for `codex resume`.
            argv.push("resume".into());
            if let Some(m) = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                argv.push("-m".into());
                argv.push(m.to_string());
            }
            if let Some(e) = parsed_effort {
                let mapped = match e {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh | Effort::Max => "xhigh",
                };
                argv.push("-c".into());
                argv.push(format!("model_reasoning_effort=\"{mapped}\""));
            }
            if let Some(dir) = req.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                argv.push("-C".into());
                argv.push(dir.to_string());
            }
            if req.bypass_permissions {
                argv.push("--dangerously-bypass-approvals-and-sandbox".into());
            }
            argv.push(rid.to_string());
        }
        AgentKind::Agy => {
            argv.push("--resume".into());
            argv.push(rid.to_string());
            let raw_model = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let fixed = raw_model.is_some_and(is_fixed_effort_agy_model);
            if let Some(m) = raw_model {
                argv.push("--model".into());
                argv.push(resolve_agy_model_id(m, parsed_effort));
            }
            if let Some(e) = parsed_effort {
                if !fixed {
                    argv.push("--effort".into());
                    argv.push(e.as_str().to_string());
                }
            }
            if req.bypass_permissions {
                argv.push("--dangerously-skip-permissions".into());
            }
        }
        AgentKind::Shell => unreachable!("checked above"),
    }

    Ok(argv)
}

/// Build a POSIX `/bin/sh -lc` waiter script used inside `tmux respawn-pane -k`.
/// Displays a compact suspended banner and blocks until `[Enter]` (wake original session via
/// `exec`) or `[s + Enter]` (drop to interactive login shell).
pub fn build_tmux_suspend_waiter_script(
    agent: AgentKind,
    resume_id: &str,
    resume_argv: &[String],
) -> String {
    let quoted_cmd = resume_argv
        .iter()
        .map(|arg| shell_single_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let short_id: String = resume_id.trim().chars().take(16).collect();
    let agent_label = match agent {
        AgentKind::Claude => "Claude",
        AgentKind::Codex => "Codex",
        AgentKind::Agy => "AGY",
        AgentKind::Shell => "Shell",
    };
    format!(
        "printf '\\n[starfane] %s 会话已休眠 (ID: %s) — 进程已释放\\n[Enter] 唤醒原会话 | [s + Enter] 进入 Shell\\n> ' {agent_q} {id_q}; \
while IFS= read -r _sf_key; do case \"$_sf_key\" in s|S) exec \"${{SHELL:-/bin/sh}}\" -l ;; *) exec {quoted_cmd} ;; esac; done",
        agent_q = shell_single_quote(agent_label),
        id_q = shell_single_quote(&short_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_claude_spawn_argv() {
        let argv = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Claude,
            bin: PathBuf::from("/Users/demo/.local/bin/claude"),
            cwd: PathBuf::from("/repo"),
            model: Some("opus".into()),
            effort: Some(Effort::High),
            name: Some("P1-impl".into()),
            prompt: "starfane: read_inbox".into(),
            extra_args: vec!["--dangerously-skip-permissions".into()],
        })
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "/Users/demo/.local/bin/claude",
                "--model",
                "opus",
                "--effort",
                "high",
                "-n",
                "P1-impl",
                "--dangerously-skip-permissions",
                "starfane: read_inbox",
            ]
        );
    }

    #[test]
    fn builds_codex_spawn_argv_with_reasoning_effort() {
        let argv = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Codex,
            bin: PathBuf::from("/usr/local/bin/codex"),
            cwd: PathBuf::from("/repo"),
            model: Some("gpt-6.2-sol".into()),
            effort: Some(Effort::Max),
            name: Some("ignored-by-codex".into()),
            prompt: "starfane: read_inbox".into(),
            extra_args: vec![],
        })
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/codex",
                "-m",
                "gpt-6.2-sol",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-C",
                "/repo",
                "starfane: read_inbox",
            ]
        );
    }

    #[test]
    fn builds_agy_spawn_argv_interactive() {
        let argv = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Agy,
            bin: PathBuf::from("/Users/demo/.local/bin/agy"),
            cwd: PathBuf::from("/repo"),
            model: Some("gemini-4-neon-high".into()),
            effort: Some(Effort::High),
            name: None,
            prompt: "starfane: read_inbox".into(),
            extra_args: vec!["--mode".into(), "auto".into()],
        })
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "/Users/demo/.local/bin/agy",
                "--model",
                "gemini-4-neon-high",
                "--effort",
                "high",
                "--mode",
                "auto",
                "-i",
                "starfane: read_inbox",
            ]
        );
    }

    #[test]
    fn rejects_multiline_or_empty_prompt() {
        let err = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Claude,
            bin: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/repo"),
            model: None,
            effort: None,
            name: None,
            prompt: "line1\nline2".into(),
            extra_args: vec![],
        })
        .unwrap_err();
        assert!(matches!(err, AdapterError::InvalidInput(_)));
    }

    #[test]
    fn resolves_agy_base_model_with_effort_and_omits_effort_for_orchestrator() {
        let base_argv = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Agy,
            bin: PathBuf::from("agy"),
            cwd: PathBuf::from("/repo"),
            model: Some("gemini-4-neon".into()),
            effort: Some(Effort::Xhigh),
            name: None,
            prompt: "hi".into(),
            extra_args: vec![],
        })
        .unwrap();
        assert_eq!(
            base_argv,
            vec![
                "agy",
                "--model",
                "gemini-4-neon-xhigh",
                "--effort",
                "xhigh",
                "-i",
                "hi"
            ]
        );

        let orch_argv = build_l4_spawn_cmd(&SpawnRequest {
            agent: AgentKind::Agy,
            bin: PathBuf::from("agy"),
            cwd: PathBuf::from("/repo"),
            model: Some("gemini-pro-orchestrator-neon".into()),
            effort: Some(Effort::High),
            name: None,
            prompt: "hi".into(),
            extra_args: vec![],
        })
        .unwrap();
        assert_eq!(
            orch_argv,
            vec!["agy", "--model", "gemini-pro-orchestrator-neon", "-i", "hi"]
        );
        assert_eq!(
            resolve_agy_model_id("gemini-4-neon-high", Some(Effort::Xhigh)),
            "gemini-4-neon-xhigh"
        );
        assert_eq!(
            resolve_agy_model_id("gemini-4-neon-xhigh", None),
            "gemini-4-neon-xhigh"
        );
    }

    #[test]
    fn ensures_default_bypass_permission_flags_idempotently_and_honors_overrides() {
        let mut cl = Vec::new();
        ensure_bypass_permission_arg(AgentKind::Claude, &mut cl);
        ensure_bypass_permission_arg(AgentKind::Claude, &mut cl);
        assert_eq!(cl, vec!["--dangerously-skip-permissions"]);

        let mut cl_custom = vec!["--permission-mode=acceptEdits".into()];
        ensure_bypass_permission_arg(AgentKind::Claude, &mut cl_custom);
        assert_eq!(cl_custom, vec!["--permission-mode=acceptEdits"]);

        let mut cx = Vec::new();
        ensure_bypass_permission_arg(AgentKind::Codex, &mut cx);
        ensure_bypass_permission_arg(AgentKind::Codex, &mut cx);
        assert_eq!(cx, vec!["--dangerously-bypass-approvals-and-sandbox"]);

        let mut cx_custom = vec!["-s".into(), "workspace-write".into()];
        ensure_bypass_permission_arg(AgentKind::Codex, &mut cx_custom);
        assert_eq!(cx_custom, vec!["-s", "workspace-write"]);

        let mut ag = Vec::new();
        ensure_bypass_permission_arg(AgentKind::Agy, &mut ag);
        ensure_bypass_permission_arg(AgentKind::Agy, &mut ag);
        assert_eq!(ag, vec!["--dangerously-skip-permissions"]);

        let mut ag_custom = vec!["--mode".into(), "auto".into()];
        ensure_bypass_permission_arg(AgentKind::Agy, &mut ag_custom);
        assert_eq!(ag_custom, vec!["--mode", "auto"]);
    }

    #[test]
    fn builds_resume_argv_and_waiter_script_for_all_agents() {
        let cl = build_resume_cmd(&ResumeRequest {
            agent: AgentKind::Claude,
            bin: None,
            resume_id: "sess-123".into(),
            cwd: Some("/repo with space".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
            bypass_permissions: true,
        })
        .unwrap();
        assert_eq!(
            cl,
            vec![
                "claude",
                "--resume",
                "sess-123",
                "--model",
                "opus",
                "--effort",
                "high",
                "--dangerously-skip-permissions",
            ]
        );

        let cx = build_resume_cmd(&ResumeRequest {
            agent: AgentKind::Codex,
            bin: Some(PathBuf::from("/usr/local/bin/codex")),
            resume_id: "019d-thread".into(),
            cwd: Some("/repo's path".into()),
            model: Some("gpt-6.2-sol".into()),
            effort: Some("max".into()),
            bypass_permissions: true,
        })
        .unwrap();
        assert_eq!(
            cx,
            vec![
                "/usr/local/bin/codex",
                "resume",
                "-m",
                "gpt-6.2-sol",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-C",
                "/repo's path",
                "--dangerously-bypass-approvals-and-sandbox",
                "019d-thread",
            ]
        );

        let agy_fixed = build_resume_cmd(&ResumeRequest {
            agent: AgentKind::Agy,
            bin: None,
            resume_id: "conv-99".into(),
            cwd: None,
            model: Some("gemini-pro-orchestrator-neon".into()),
            effort: Some("high".into()),
            bypass_permissions: false,
        })
        .unwrap();
        assert_eq!(
            agy_fixed,
            vec![
                "agy",
                "--resume",
                "conv-99",
                "--model",
                "gemini-pro-orchestrator-neon",
            ]
        );

        assert!(build_resume_cmd(&ResumeRequest {
            agent: AgentKind::Shell,
            bin: None,
            resume_id: "x".into(),
            cwd: None,
            model: None,
            effort: None,
            bypass_permissions: false,
        })
        .is_err());

        let waiter = build_tmux_suspend_waiter_script(AgentKind::Codex, "019d-thread", &cx);
        assert!(waiter.contains("Codex"));
        assert!(waiter.contains("'/repo'\\''s path'"));
        assert!(waiter.contains("'019d-thread'"));
        assert!(waiter.contains("IFS= read -r _sf_key"));
    }
}
