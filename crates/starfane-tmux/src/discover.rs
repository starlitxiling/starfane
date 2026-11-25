//! Live tmux agent pane discovery & terminal tail capture.
//!
//! Discovers already-running `claude` (including versioned binaries like `2.1.299`),
//! `codex`, and `agy` sessions across all tmux panes by inspecting `tmux list-panes -a`
//! alongside a single `ps -eo pid=,ppid=,args=` snapshot.

use crate::error::TmuxError;
use crate::model::PaneInfo;
use crate::{valid_pane, Tmux};
use serde::{Deserialize, Serialize};
use starfane_proto::{AgentKind, PaneId};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Hard cap on captured preview text for standard tail requests (256 KiB).
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;
/// Hard cap on captured preview text for full/paginated history requests (4 MiB).
pub const MAX_FULL_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const MIN_CAPTURE_LINES: usize = 1;
/// Default cap on lines for standard tail requests.
pub const MAX_CAPTURE_LINES: usize = 2000;
/// Hard cap on lines for full/paginated history requests.
pub const MAX_FULL_CAPTURE_LINES: usize = 50_000;

/// Options controlling live tmux pane discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoverOptions {
    /// Also include non-agent panes (plain shells, training processes, monitors) as `AgentKind::Shell`.
    pub include_shell_panes: bool,
}

/// Request parameters for pane scrollback capture (tail, full `-S -`, or cursor-paginated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRequest {
    /// Number of lines to return.
    pub lines: usize,
    /// When `true`, fetch full scrollback (`-S -`, up to [`MAX_FULL_CAPTURE_LINES`] / [`MAX_FULL_CAPTURE_BYTES`]).
    pub all: bool,
    /// Optional 0-based line cursor (`0` = oldest scrollback line): return up to `lines` ending strictly before this line.
    pub before_line: Option<usize>,
}

impl CaptureRequest {
    pub fn tail(lines: usize) -> Self {
        Self {
            lines,
            all: false,
            before_line: None,
        }
    }

    pub fn full() -> Self {
        Self {
            lines: MAX_FULL_CAPTURE_LINES,
            all: true,
            before_line: None,
        }
    }

    pub fn page_before(before_line: usize, lines: usize) -> Self {
        Self {
            lines,
            all: false,
            before_line: Some(before_line),
        }
    }
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
    #[serde(default)]
    pub history_size: u32,
    #[serde(default)]
    pub cursor_x: u32,
    #[serde(default)]
    pub cursor_y: u32,
}

/// Captured slice of a pane returned by preview endpoints (tail, paginated, or full history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePreview {
    pub pane_id: PaneId,
    pub lines: usize,
    pub text: String,
    pub in_mode: bool,
    pub dead: bool,
    /// Total lines currently held by the multiplexer (scrollback + visible screen).
    #[serde(default)]
    pub total_lines: usize,
    /// 0-based index of the first returned line (`0` = oldest scrollback line).
    #[serde(default)]
    pub start_line: usize,
    /// True when older lines exist before `start_line`.
    #[serde(default)]
    pub has_more: bool,
    /// Cursor to pass as `before_line` to load earlier lines without overlap or gaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_line: Option<usize>,
    /// True when the response hit the line or byte safety fuse.
    #[serde(default)]
    pub truncated: bool,
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
        let ps_out = self
            .transport
            .run(
                Path::new("ps"),
                &["-eo".to_string(), "pid=,ppid=,args=".to_string()],
                None,
            )
            .map(|o| if o.success { o.stdout } else { String::new() })
            .unwrap_or_default();

        classify_panes_with_git_lookup(panes, &ps_out, opts, self.transport.is_local())
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

    /// Capture multiple panes' tails in a single batched `tmux` invocation using `;` separators.
    /// Falls back to per-pane capture only if the batched command fails (e.g. a pane exited mid-call).
    pub fn capture_panes_tail_batch(
        &self,
        targets: &[(&PaneId, usize)],
    ) -> HashMap<PaneId, String> {
        let mut out = HashMap::new();
        if targets.is_empty() {
            return out;
        }
        if targets.len() == 1 {
            let (pid, lines) = targets[0];
            if let Ok(text) = self.capture_pane_tail(pid, lines) {
                out.insert(pid.clone(), text);
            }
            return out;
        }

        // Nonce marker contains no `%` or `#` so tmux `display-message -p` prints it verbatim.
        let marker = format!("__SF_CAP_END_{}__", std::process::id());
        let mut args: Vec<String> = Vec::with_capacity(targets.len() * 10);
        let mut valid_targets: Vec<(&PaneId, usize)> = Vec::with_capacity(targets.len());
        for &(pid, lines) in targets {
            if valid_pane(pid).is_err() {
                continue;
            }
            let clamped = lines.clamp(MIN_CAPTURE_LINES, MAX_CAPTURE_LINES);
            if !args.is_empty() {
                args.push(";".into());
            }
            args.extend([
                "capture-pane".into(),
                "-p".into(),
                "-t".into(),
                pid.as_str().to_string(),
                "-S".into(),
                format!("-{clamped}"),
                ";".into(),
                "display-message".into(),
                "-p".into(),
                marker.clone(),
            ]);
            valid_targets.push((pid, clamped));
        }
        if valid_targets.is_empty() {
            return out;
        }

        if let Ok(raw) = self.run_owned(&args, None) {
            let segments = split_batch_capture(&raw, &marker);
            if segments.len() == valid_targets.len() {
                for ((pid, clamped), seg) in valid_targets.into_iter().zip(segments) {
                    out.insert(
                        pid.clone(),
                        trim_and_cap_tail(&seg, clamped, MAX_CAPTURE_BYTES),
                    );
                }
                return out;
            }
        }

        // Resilient fallback if a pane died during the batched call.
        for &(pid, lines) in targets {
            if let Ok(text) = self.capture_pane_tail(pid, lines) {
                out.insert(pid.clone(), text);
            }
        }
        out
    }

    /// Fetch pane preview (tail, paginated scrollback, or full `-S -`) together with pane flags and cursor metadata.
    pub fn capture_pane(
        &self,
        pane_id: &PaneId,
        req: &CaptureRequest,
    ) -> Result<PanePreview, TmuxError> {
        valid_pane(pane_id)?;
        let meta_raw = self.run(
            &[
                "display-message",
                "-p",
                "-t",
                pane_id.as_str(),
                "#{history_size}\t#{pane_height}\t#{pane_in_mode}\t#{pane_dead}",
            ],
            None,
        )?;
        let meta_line = meta_raw.trim_end_matches(['\r', '\n']);
        let parts = crate::model::split_tmux_fields(meta_line);
        if parts.len() != 4 {
            return Err(TmuxError::Parse(format!(
                "unexpected capture meta for {pane_id}: {meta_line:?}"
            )));
        }
        let history_size = parts[0].trim().parse::<usize>().unwrap_or(0);
        let pane_height = parts[1].trim().parse::<usize>().unwrap_or(0);
        let in_mode = parts[2].trim() == "1";
        let dead = parts[3].trim() == "1";
        let total_lines = history_size.saturating_add(pane_height);

        if let Some(before) = req.before_line {
            let limit = req.lines.clamp(MIN_CAPTURE_LINES, MAX_FULL_CAPTURE_LINES);
            let end_excl = before.min(total_lines);
            if end_excl == 0 {
                return Ok(PanePreview {
                    pane_id: pane_id.clone(),
                    lines: limit,
                    text: String::new(),
                    in_mode,
                    dead,
                    total_lines,
                    start_line: 0,
                    has_more: false,
                    next_before_line: None,
                    truncated: false,
                });
            }
            let start_idx = end_excl.saturating_sub(limit);
            let tmux_start = (start_idx as isize) - (history_size as isize);
            let tmux_end = ((end_excl - 1) as isize) - (history_size as isize);
            let s_arg = tmux_start.to_string();
            let e_arg = tmux_end.to_string();
            let raw = self.run(
                &[
                    "capture-pane",
                    "-p",
                    "-t",
                    pane_id.as_str(),
                    "-S",
                    &s_arg,
                    "-E",
                    &e_arg,
                ],
                None,
            )?;
            let (text, dropped_top, byte_truncated) =
                cap_page_bytes(&raw, limit, MAX_FULL_CAPTURE_BYTES, false);
            let eff_start = start_idx.saturating_add(dropped_top).min(end_excl);
            let has_more = eff_start > 0;
            return Ok(PanePreview {
                pane_id: pane_id.clone(),
                lines: limit,
                text,
                in_mode,
                dead,
                total_lines,
                start_line: eff_start,
                has_more,
                next_before_line: has_more.then_some(eff_start),
                truncated: byte_truncated,
            });
        }

        let (limit, max_bytes) = if req.all {
            let lim = if req.lines == 0 {
                MAX_FULL_CAPTURE_LINES
            } else {
                req.lines.clamp(MIN_CAPTURE_LINES, MAX_FULL_CAPTURE_LINES)
            };
            (lim, MAX_FULL_CAPTURE_BYTES)
        } else {
            let lim = req.lines.clamp(MIN_CAPTURE_LINES, MAX_FULL_CAPTURE_LINES);
            let bytes = if lim > MAX_CAPTURE_LINES {
                MAX_FULL_CAPTURE_BYTES
            } else {
                MAX_CAPTURE_BYTES
            };
            (lim, bytes)
        };

        let (start_idx, start_arg) = if total_lines <= limit {
            (0usize, "-".to_string())
        } else {
            let s_idx = total_lines.saturating_sub(limit);
            let tmux_start = (s_idx as isize) - (history_size as isize);
            (s_idx, tmux_start.to_string())
        };

        let raw = self.run(
            &[
                "capture-pane",
                "-p",
                "-t",
                pane_id.as_str(),
                "-S",
                &start_arg,
            ],
            None,
        )?;
        let (text, dropped_top, byte_truncated) = cap_page_bytes(&raw, limit, max_bytes, true);
        let eff_start = start_idx.saturating_add(dropped_top).min(total_lines);
        let has_more = eff_start > 0;
        let truncated = byte_truncated || (req.all && has_more);
        Ok(PanePreview {
            pane_id: pane_id.clone(),
            lines: limit,
            text,
            in_mode,
            dead,
            total_lines,
            start_line: eff_start,
            has_more,
            next_before_line: has_more.then_some(eff_start),
            truncated,
        })
    }

    /// Fetch pane preview together with `in_mode` and `dead` flags.
    pub fn preview_pane(&self, pane_id: &PaneId, lines: usize) -> Result<PanePreview, TmuxError> {
        self.capture_pane(pane_id, &CaptureRequest::tail(lines))
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
    classify_panes_with_git_lookup(panes, ps_output, opts, true)
}

/// Pure classifier combining `list-panes` rows with `ps` output, optionally skipping local `.git` traversal
/// (e.g. for remote SSH panes where paths belong to a remote filesystem).
pub fn classify_panes_with_git_lookup(
    panes: &[PaneInfo],
    ps_output: &str,
    opts: DiscoverOptions,
    resolve_local_git: bool,
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
            let git_root = if resolve_local_git && !pane.current_path.is_empty() {
                find_git_root(Path::new(&pane.current_path))
            } else {
                None
            };
            let subtree_pids = if pane.pane_pid > 0 {
                collect_subtree_pids(pane.pane_pid, &children, 4)
            } else {
                Vec::new()
            };
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
                history_size: pane.history_size,
                cursor_x: pane.cursor_x,
                cursor_y: pane.cursor_y,
            });
        } else if opts.include_shell_panes && !pane.dead {
            let cmd = pane.current_command.trim();
            let model = (!cmd.is_empty()).then(|| cmd.to_string());
            let git_root = if resolve_local_git && !pane.current_path.is_empty() {
                find_git_root(Path::new(&pane.current_path))
            } else {
                None
            };
            let subtree_pids = if pane.pane_pid > 0 {
                collect_subtree_pids(pane.pane_pid, &children, 4)
            } else {
                Vec::new()
            };
            out.push(DiscoveredPane {
                pane_id: pane.pane_id.clone(),
                tmux_target,
                tmux_label: pane.tmux_label(),
                agent: AgentKind::Shell,
                cwd: pane.current_path.clone(),
                git_root,
                model,
                effort: None,
                agent_pid: (pane.pane_pid > 0).then_some(pane.pane_pid),
                subtree_pids,
                codex_resume_id: None,
                pane_in_mode: pane.in_mode,
                pane_dead: pane.dead,
                pane_current_command: pane.current_command.clone(),
                history_size: pane.history_size,
                cursor_x: pane.cursor_x,
                cursor_y: pane.cursor_y,
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

    // Fallback: inspect `pane_current_command` directly (covers versioned Claude binaries like `2.1.299`
    // and Zellij panes whose `current_command` carries the full command line).
    let cmd = pane.current_command.trim();
    if let Some(agent) = classify_cmdline(cmd) {
        let model = extract_model_flag(cmd);
        let effort = extract_effort_flag(agent, cmd);
        let codex_resume_id = if agent == AgentKind::Codex {
            extract_codex_resume_id(cmd)
        } else {
            None
        };
        return Some(DetectedAgent {
            agent,
            model,
            effort,
            pid: None,
            codex_resume_id,
        });
    }
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

/// Split batched `capture-pane ... ; display-message -p <marker>` stdout into per-pane segments.
pub(crate) fn split_batch_capture(raw: &str, marker: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line == marker {
            segments.push(current.join("\n"));
            current.clear();
        } else {
            current.push(line);
        }
    }
    segments
}

/// Cap captured output to at most `max_lines` newest lines and `max_bytes`, returning
/// `(text, dropped_leading_lines, truncated_by_bytes)`.
pub fn cap_page_bytes(
    raw: &str,
    max_lines: usize,
    max_bytes: usize,
    trim_trailing_blanks: bool,
) -> (String, usize, bool) {
    let mut lines: Vec<&str> = raw.lines().collect();
    if trim_trailing_blanks {
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
    }
    let mut dropped_top = 0usize;
    if lines.len() > max_lines {
        let drop_n = lines.len() - max_lines;
        lines.drain(0..drop_n);
        dropped_top += drop_n;
    }
    let joined = lines.join("\n");
    if joined.len() <= max_bytes {
        return (joined, dropped_top, false);
    }
    let mut start = joined.len() - max_bytes;
    while start < joined.len() && !joined.is_char_boundary(start) {
        start += 1;
    }
    // Prefer snapping to the next newline boundary when possible so pagination stays line-aligned.
    if let Some(nl) = joined[start..].find('\n') {
        if start + nl + 1 < joined.len() {
            let cut = start + nl + 1;
            let extra_dropped = joined[..cut].bytes().filter(|&b| b == b'\n').count();
            return (joined[cut..].to_string(), dropped_top + extra_dropped, true);
        }
    }
    let extra_dropped = joined[..start].bytes().filter(|&b| b == b'\n').count();
    (
        joined[start..].to_string(),
        dropped_top + extra_dropped,
        true,
    )
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
            session_group: String::new(),
            history_size: 0,
            pane_height: 24,
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
