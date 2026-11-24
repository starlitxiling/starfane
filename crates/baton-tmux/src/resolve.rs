//! Pane resolution (env → PID tree → cwd) and process ancestry.

use crate::model::PaneInfo;
use baton_proto::{AgentKind, PaneId, PaneResolution};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PaneHint {
    pub agent: AgentKind,
    pub env_pane: Option<PaneId>,
    /// Nearest-first ancestor pid chain of the agent / hook process.
    pub pid_chain: Vec<u32>,
    pub cwd: Option<String>,
}

/// Walk up the process tree with `ps -o ppid= -p <pid>`. The result starts with `pid`
/// itself and stops at pid 0/1, on a cycle, on `ps` failure, or after `max_depth` entries.
pub fn pid_ancestors(pid: u32, max_depth: usize) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut cur = pid;
    while chain.len() < max_depth && cur > 1 && !chain.contains(&cur) {
        chain.push(cur);
        let out = Command::new("ps")
            .args(["-o", "ppid=", "-p", &cur.to_string()])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let parent = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok(),
            _ => None,
        };
        match parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain
}

/// Does a pane's `pane_current_command` look like this agent?
/// Claude Code frequently reports its version (e.g. `2.1.45`) as the command on macOS.
fn command_matches_agent(cmd: &str, agent: AgentKind) -> bool {
    let c = cmd.to_ascii_lowercase();
    if c.contains(agent.as_str()) {
        return true;
    }
    agent == AgentKind::Claude
        && !c.is_empty()
        && c.starts_with(|ch: char| ch.is_ascii_digit())
        && c.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let t = s.trim_end_matches('/');
        if t.is_empty() { "/" } else { t }.to_string()
    };
    !a.is_empty() && Path::new(&norm(a)) == Path::new(&norm(b))
}

/// Priority: `env_pane` present in `panes` → Env; `pid_chain` intersects a `pane_pid` → PidTree
/// (nearest ancestor wins); `cwd` equal and exactly one live same-agent candidate → CwdMatch;
/// otherwise Unknown.
pub fn resolve_pane(panes: &[PaneInfo], hint: &PaneHint) -> (Option<PaneId>, PaneResolution) {
    if let Some(env) = &hint.env_pane {
        if panes.iter().any(|p| &p.pane_id == env) {
            return (Some(env.clone()), PaneResolution::Env);
        }
    }
    for pid in &hint.pid_chain {
        if let Some(p) = panes.iter().find(|p| p.pane_pid == *pid) {
            return (Some(p.pane_id.clone()), PaneResolution::PidTree);
        }
    }
    if let Some(cwd) = &hint.cwd {
        let mut cands = panes.iter().filter(|p| {
            !p.dead
                && same_dir(&p.current_path, cwd)
                && command_matches_agent(&p.current_command, hint.agent)
        });
        if let (Some(only), None) = (cands.next(), cands.next()) {
            return (Some(only.pane_id.clone()), PaneResolution::CwdMatch);
        }
    }
    (None, PaneResolution::Unknown)
}
