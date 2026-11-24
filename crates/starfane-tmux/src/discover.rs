//! Live tmux agent pane discovery & terminal tail capture.
//!
//! Discovers already-running `claude` (including versioned binaries like `2.1.299`),
//! `codex`, and `agy` sessions across all tmux panes by inspecting `tmux list-panes -a`
//! alongside a single `ps -eo pid=,ppid=,args=` snapshot.

use crate::error::TmuxError;
use crate::exec;
use crate::model::PaneInfo;
use crate::{valid_pane, Tmux};
use serde::Serialize;
use starfane_proto::{AgentKind, PaneId};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Hard cap on captured preview text (256 KiB).
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MIN_CAPTURE_LINES: usize = 1;
/// Hard cap on lines requested via `tmux capture-pane`.
pub const MAX_CAPTURE_LINES: usize = 2000;

/// Options controlling live tmux pane discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoverOptions {
    /// Also include non-agent panes (plain shells, training processes, monitors) as `AgentKind::Shell`.
    pub include_shell_panes: bool,
}

/// Metadata for an active coding-agent pane discovered in tmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredPane {
    pub pane_id: PaneId,
    /// tmux target string formatted as `session:window_index.pane_index`.
    pub tmux_target: String,
    /// Human-friendly label `<session_name>-<window_name>`.
    pub tmux_label: String,
    pub agent: AgentKind,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Explicit reasoning/thinking effort parsed from process flags, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// PID of the detected agent CLI process inside the pane, if matched via `ps`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pid: Option<u32>,
    /// All PIDs in the pane's process subtree (BFS up to depth 4) for exact session-file lookup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtree_pids: Vec<u32>,
    /// Explicit Codex resume session UUID parsed from `codex resume <uuid>`, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_resume_id: Option<String>,
    pub pane_in_mode: bool,
    pub pane_dead: bool,
    pub pane_current_command: String,
}

/// Captured tail of a pane returned by preview endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanePreview {
    pub pane_id: PaneId,
    pub lines: usize,
    pub text: String,
    pub in_mode: bool,
    pub dead: bool,
}

/// Standalone helper: discover all running agent panes on `socket` (`None` = default tmux server).
pub fn discover_agent_panes(socket: Option<&str>) -> Result<Vec<DiscoveredPane>, TmuxError> {
    let tmux = Tmux::new(PathBuf::from("tmux"), socket.map(str::to_string));
    tmux.discover_agent_panes()
}

/// Standalone helper: capture the last `lines` of `pane_id` (capped at [`MAX_CAPTURE_BYTES`], trailing blank lines stripped).
pub fn capture_pane_tail(
    socket: Option<&str>,
    pane_id: &PaneId,
    lines: usize,
) -> Result<String, TmuxError> {
    let tmux = Tmux::new(PathBuf::from("tmux"), socket.map(str::to_string));
    tmux.capture_pane_tail(pane_id, lines)
}

impl Tmux {
    /// Discover all running `claude`, `codex`, and `agy` panes on this tmux server.
    pub fn discover_agent_panes(&self) -> Result<Vec<DiscoveredPane>, TmuxError> {
        let panes = self.list_panes()?;
        Ok(self.discover_agent_panes_from(&panes))
    }

    /// Discover running panes from an already-fetched pane list under `opts`.
    pub fn discover_panes_from(
        &self,
        panes: &[PaneInfo],
        opts: DiscoverOptions,
    ) -> Vec<DiscoveredPane> {
        if panes.is_empty() {
            return Vec::new();
        }
        let ps_out = exec::run(
            Path::new("ps"),
            &["-eo".to_string(), "pid=,ppid=,args=".to_string()],
            None,
        )
        .map(|o| if o.success { o.stdout } else { String::new() })
        .unwrap_or_default();

        classify_panes_with(panes, &ps_out, opts)
    }

    /// Discover running `claude`, `codex`, and `agy` panes from an already-fetched pane list.
    pub fn discover_agent_panes_from(&self, panes: &[PaneInfo]) -> Vec<DiscoveredPane> {
        self.discover_panes_from(panes, DiscoverOptions::default())
    }

    /// Capture the last `lines` lines of `pane_id`, trimming trailing empty lines and capping at [`MAX_CAPTURE_BYTES`].
    pub fn capture_pane_tail(&self, pane_id: &PaneId, lines: usize) -> Result<String, TmuxError> {
        valid_pane(pane_id)?;
        let clamped = lines.clamp(MIN_CAPTURE_LINES, MAX_CAPTURE_LINES);
        let start = format!("-{clamped}");
        let raw = self.run(
            &["capture-pane", "-p", "-t", pane_id.as_str(), "-S", &start],
            None,
        )?;
        Ok(trim_and_cap_tail(&raw, clamped, MAX_CAPTURE_BYTES))
    }

    /// Fetch pane preview together with `in_mode` and `dead` flags.
    pub fn preview_pane(&self, pane_id: &PaneId, lines: usize) -> Result<PanePreview, TmuxError> {
        valid_pane(pane_id)?;
        let clamped = lines.clamp(MIN_CAPTURE_LINES, MAX_CAPTURE_LINES);
        let panes = self.list_panes()?;
        let info = panes
            .into_iter()
            .find(|p| &p.pane_id == pane_id)
            .ok_or_else(|| TmuxError::Command {
                args: format!("capture-pane -t {pane_id}"),
                code: Some(1),
                stderr: format!("pane {pane_id} not found"),
            })?;
        let text = self.capture_pane_tail(pane_id, clamped)?;
        Ok(PanePreview {
            pane_id: info.pane_id,
            lines: clamped,
            text,
            in_mode: info.in_mode,
            dead: info.dead,
        })
    }
}

/// Pure classifier combining `list-panes` rows with `ps -eo pid=,ppid=,args=` output.
pub fn classify_panes(panes: &[PaneInfo], ps_output: &str) -> Vec<DiscoveredPane> {
    classify_panes_with(panes, ps_output, DiscoverOptions::default())
}

/// Pure classifier combining `list-panes` rows with `ps -eo pid=,ppid=,args=` output under `opts`.
pub fn classify_panes_with(
    panes: &[PaneInfo],
    ps_output: &str,
    opts: DiscoverOptions,
) -> Vec<DiscoveredPane> {
    let mut by_pid: HashMap<u32, String> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();

    for line in ps_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        // Reconstruct remaining args slice preserving original spacing after the 2nd token.
        let rest = after_two_tokens(trimmed);
        by_pid.insert(pid, rest.to_string());
        children.entry(ppid).or_default().push(pid);
    }

    let mut out = Vec::new();
    for pane in panes {
        let tmux_target = format!(
            "{}:{}.{}",
            pane.session_name, pane.window_index, pane.pane_index
        );
        if let Some(det) = detect_pane_agent(pane, &by_pid, &children) {
            let git_root = find_git_root(Path::new(&pane.current_path));
            let subtree_pids = collect_subtree_pids(pane.pane_pid, &children, 4);
            out.push(DiscoveredPane {
                pane_id: pane.pane_id.clone(),
                tmux_target,
                tmux_label: pane.tmux_label(),
                agent: det.agent,
                cwd: pane.current_path.clone(),
                git_root,
                model: det.model,
                effort: det.effort,
                agent_pid: det.pid,
                subtree_pids,
                codex_resume_id: det.codex_resume_id,
                pane_in_mode: pane.in_mode,
                pane_dead: pane.dead,
                pane_current_command: pane.current_command.clone(),
            });
        } else if opts.include_shell_panes && !pane.dead {
            let cmd = pane.current_command.trim();
            let model = (!cmd.is_empty()).then(|| cmd.to_string());
            let git_root = find_git_root(Path::new(&pane.current_path));
            let subtree_pids = collect_subtree_pids(pane.pane_pid, &children, 4);
            out.push(DiscoveredPane {
                pane_id: pane.pane_id.clone(),
                tmux_target,
                tmux_label: pane.tmux_label(),
                agent: AgentKind::Shell,
                cwd: pane.current_path.clone(),
                git_root,
                model,
                effort: None,
                agent_pid: Some(pane.pane_pid),
                subtree_pids,
                codex_resume_id: None,
                pane_in_mode: pane.in_mode,
                pane_dead: pane.dead,
                pane_current_command: pane.current_command.clone(),
            });
        }
    }
    out
}

fn collect_subtree_pids(
    root: u32,
    children: &HashMap<u32, Vec<u32>>,
    max_depth: usize,
) -> Vec<u32> {
    let mut out = Vec::new();
    let mut q = VecDeque::new();
    q.push_back((root, 0));
    while let Some((pid, depth)) = q.pop_front() {
        out.push(pid);
        if depth < max_depth {
            if let Some(kids) = children.get(&pid) {
                for &k in kids {
                    q.push_back((k, depth + 1));
                }
            }
        }
    }
    out
}

fn after_two_tokens(s: &str) -> &str {
    let mut idx = 0;
    let bytes = s.as_bytes();
    for _ in 0..2 {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        while idx < bytes.len() && !bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
    }
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    &s[idx..]
}

struct DetectedAgent {
    agent: AgentKind,
    model: Option<String>,
    effort: Option<String>,
    pid: Option<u32>,
    codex_resume_id: Option<String>,
}

fn detect_pane_agent(
    pane: &PaneInfo,
    by_pid: &HashMap<u32, String>,
    children: &HashMap<u32, Vec<u32>>,
) -> Option<DetectedAgent> {
    // BFS over pane_pid and descendants up to depth 3 so shells wrapping agent CLIs are detected,
    // while avoiding deep grandchildren such as `starfane mcp --agent agy`.
    let mut queue: VecDeque<(u32, usize)> = VecDeque::new();
    queue.push_back((pane.pane_pid, 0));

    while let Some((pid, depth)) = queue.pop_front() {
        if let Some(args) = by_pid.get(&pid) {
            if let Some(agent) = classify_cmdline(args) {
                let model = extract_model_flag(args);
                let effort = extract_effort_flag(agent, args);
                let codex_resume_id = if agent == AgentKind::Codex {
                    extract_codex_resume_id(args)
                } else {
                    None
                };
                return Some(DetectedAgent {
                    agent,
                    model,
                    effort,
                    pid: Some(pid),
                    codex_resume_id,
                });
            }
        }
        if depth < 3 {
            if let Some(kids) = children.get(&pid) {
                for &k in kids {
                    queue.push_back((k, depth + 1));
                }
            }
        }
    }

    // Fallback: inspect `pane_current_command` directly (covers versioned Claude binaries like `2.1.299`).
    let cmd = pane.current_command.trim();
    if let Some(agent) = classify_command_name(cmd) {
        return Some(DetectedAgent {
            agent,
            model: None,
            effort: None,
            pid: None,
            codex_resume_id: None,
        });
    }
    if looks_like_claude_version(cmd) {
        return Some(DetectedAgent {
            agent: AgentKind::Claude,
            model: None,
            effort: None,
            pid: None,
            codex_resume_id: None,
        });
    }
    None
}

fn extract_codex_resume_id(args: &str) -> Option<String> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    for w in tokens.windows(2) {
        if w[0] == "resume" {
            let cand = w[1].trim_matches(['"', '\'']);
            if cand.len() >= 16 && cand.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return Some(cand.to_string());
            }
        }
    }
    None
}

fn classify_command_name(cmd: &str) -> Option<AgentKind> {
    let base = cmd.rsplit('/').next().unwrap_or(cmd).to_ascii_lowercase();
    match base.as_str() {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "agy" => Some(AgentKind::Agy),
        _ => None,
    }
}

/// True for Claude Code native versioned binary names like `"2.1.299"`.
fn looks_like_claude_version(cmd: &str) -> bool {
    let parts: Vec<&str> = cmd.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn classify_cmdline(args: &str) -> Option<AgentKind> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let first = *tokens.first()?;
    let base = first.rsplit('/').next().unwrap_or(first);

    // Ignore starfane itself (and leftover baton MCP children during transition).
    if base.eq_ignore_ascii_case("starfane") || base.eq_ignore_ascii_case("baton") {
        return None;
    }

    if let Some(agent) = classify_command_name(base) {
        // Skip Codex background app-server / exec-server helpers if they ever appear.
        if agent == AgentKind::Codex
            && tokens
                .get(1)
                .is_some_and(|sub| matches!(*sub, "app-server" | "exec-server" | "mcp"))
        {
            return None;
        }
        return Some(agent);
    }

    if looks_like_claude_version(base) && first.contains("claude") {
        return Some(AgentKind::Claude);
    }

    // Node wrapper form: `node .../codex.js` or `node .../claude/cli.js`.
    if base.eq_ignore_ascii_case("node") {
        if let Some(script) = tokens.get(1) {
            let sbase = script
                .rsplit('/')
                .next()
                .unwrap_or(script)
                .to_ascii_lowercase();
            if sbase == "codex.js" || sbase == "codex" || script.contains("@openai/codex") {
                return Some(AgentKind::Codex);
            }
            if sbase == "claude.js" || script.contains("@anthropic-ai/claude-code") {
                return Some(AgentKind::Claude);
            }
        }
    }

    None
}

fn extract_model_flag(args: &str) -> Option<String> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut i = 1;
    while i < tokens.len() {
        let tok = tokens[i];
        if let Some(val) = tok
            .strip_prefix("--model=")
            .or_else(|| tok.strip_prefix("-m="))
        {
            let v = val.trim_matches(['"', '\'']);
            if !v.is_empty() {
                return Some(v.to_string());
            }
        } else if matches!(tok, "--model" | "-m") {
            if let Some(next) = tokens.get(i + 1) {
                let v = next.trim_matches(['"', '\'']);
                if !v.is_empty() && !v.starts_with('-') {
                    return Some(v.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

fn extract_effort_flag(agent: AgentKind, args: &str) -> Option<String> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut i = 1;
    while i < tokens.len() {
        let tok = tokens[i];
        match agent {
            AgentKind::Claude => {
                if let Some(val) = tok.strip_prefix("--effort=") {
                    let v = val.trim_matches(['"', '\'']).trim();
                    if !v.is_empty() {
                        return Some(v.to_ascii_lowercase());
                    }
                } else if tok == "--effort" {
                    if let Some(next) = tokens.get(i + 1) {
                        let v = next.trim_matches(['"', '\'']).trim();
                        if !v.is_empty() && !v.starts_with('-') {
                            return Some(v.to_ascii_lowercase());
                        }
                    }
                }
            }
            AgentKind::Codex => {
                if let Some(kv) = tok
                    .strip_prefix("-c=")
                    .or_else(|| tok.strip_prefix("--config="))
                {
                    if let Some(v) = kv.strip_prefix("model_reasoning_effort=") {
                        let clean = v.trim_matches(['"', '\'']).trim();
                        if !clean.is_empty() {
                            return Some(clean.to_ascii_lowercase());
                        }
                    }
                } else if matches!(tok, "-c" | "--config") {
                    if let Some(next) = tokens.get(i + 1) {
                        if let Some(v) = next.strip_prefix("model_reasoning_effort=") {
                            let clean = v.trim_matches(['"', '\'']).trim();
                            if !clean.is_empty() {
                                return Some(clean.to_ascii_lowercase());
                            }
                        }
                    }
                }
            }
            AgentKind::Agy | AgentKind::Shell => {}
        }
        i += 1;
    }
    None
}

/// Fast filesystem walk for `.git` (works for both normal repos and worktrees without spawning `git`).
fn find_git_root(dir: &Path) -> Option<String> {
    if !dir.is_absolute() || !dir.exists() {
        return None;
    }
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.join(".git").exists() {
            return Some(p.to_string_lossy().into_owned());
        }
        cur = p.parent();
    }
    None
}

/// Strip trailing empty lines, keep at most `max_lines` newest lines, and cap UTF-8 byte length at `max_bytes`.
pub fn trim_and_cap_tail(raw: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut lines: Vec<&str> = raw.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() > max_lines {
        let drop_n = lines.len() - max_lines;
        lines.drain(0..drop_n);
    }
    let joined = lines.join("\n");
    if joined.len() <= max_bytes {
        return joined;
    }
    let mut start = joined.len() - max_bytes;
    while start < joined.len() && !joined.is_char_boundary(start) {
        start += 1;
    }
    joined[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_pane(
        id: &str,
        session: &str,
        win: u32,
        idx: u32,
        pid: u32,
        cmd: &str,
        path: &str,
    ) -> PaneInfo {
        PaneInfo {
            pane_id: PaneId::new(id),
            session_name: session.into(),
            window_id: format!("@{win}"),
            window_index: win,
            pane_index: idx,
            pane_pid: pid,
            current_command: cmd.into(),
            current_path: path.into(),
            pane_active: true,
            window_active: true,
            session_attached: 1,
            in_mode: false,
            dead: false,
            synchronized: false,
            alternate_on: false,
            cursor_x: 2,
            cursor_y: 10,
            window_name: format!("win{win}"),
        }
    }

    #[test]
    fn classifies_claude_version_codex_agy_and_skips_zsh_and_starfane() {
        let panes = vec![
            mk_pane("%9", "learning", 2, 1, 9797, "2.1.299", "/tmp"),
            mk_pane("%24", "teleop", 1, 0, 13391, "codex", "/tmp"),
            mk_pane("%41", "learning", 6, 0, 7922, "agy", "/tmp"),
            mk_pane("%6", "learning", 0, 0, 9403, "zsh", "/tmp"),
        ];
        let ps = "\
 9797  1 /bin/zsh
21677 9797 claude --model opus --allow-dangerously-skip-permissions
13391  1 /bin/zsh
24705 13391 /Users/demo/.nvm/bin/codex -m gpt-6.2-sol resume 01a1
 7922  1 -zsh
 8759 7922 agy --model=gemini-4-neon-high --dangerously-skip-permissions
98375 8759 /Users/demo/work/starfane/target/release/starfane mcp --agent agy
 9403  1 /bin/zsh
99000 9403 /Users/demo/work/starfane/target/release/starfane ls
";
        let found = classify_panes(&panes, ps);
        assert_eq!(found.len(), 3);

        assert_eq!(found[0].pane_id.as_str(), "%9");
        assert_eq!(found[0].tmux_target, "learning:2.1");
        assert_eq!(found[0].agent, AgentKind::Claude);
        assert_eq!(found[0].model.as_deref(), Some("opus"));

        assert_eq!(found[1].pane_id.as_str(), "%24");
        assert_eq!(found[1].tmux_target, "teleop:1.0");
        assert_eq!(found[1].agent, AgentKind::Codex);
        assert_eq!(found[1].model.as_deref(), Some("gpt-6.2-sol"));

        assert_eq!(found[2].pane_id.as_str(), "%41");
        assert_eq!(found[2].tmux_target, "learning:6.0");
        assert_eq!(found[2].agent, AgentKind::Agy);
        assert_eq!(found[2].model.as_deref(), Some("gemini-4-neon-high"));

        let with_shells = classify_panes_with(
            &panes,
            ps,
            DiscoverOptions {
                include_shell_panes: true,
            },
        );
        assert_eq!(with_shells.len(), 4);
        assert_eq!(with_shells[3].pane_id.as_str(), "%6");
        assert_eq!(with_shells[3].tmux_target, "learning:0.0");
        assert_eq!(with_shells[3].agent, AgentKind::Shell);
        assert_eq!(with_shells[3].model.as_deref(), Some("zsh"));
    }

    #[test]
    fn trims_trailing_blank_lines_and_caps_bytes() {
        assert_eq!(trim_and_cap_tail("a\nb\n\n  \n", 80, 100), "a\nb");
        assert_eq!(trim_and_cap_tail("1\n2\n3\n4\n5\n\n", 3, 100), "3\n4\n5");
        let big = "中".repeat(100_000); // 300,000 bytes > 256 KiB
        let capped = trim_and_cap_tail(&big, 80, MAX_CAPTURE_BYTES);
        assert!(capped.len() <= MAX_CAPTURE_BYTES);
        assert!(capped.len() > MAX_CAPTURE_BYTES - 16);
        assert!(capped.ends_with('中'));
    }
}
