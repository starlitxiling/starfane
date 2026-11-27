//! Pure functions: user-watching detection, the injection gate, and pointer-text validation.

use crate::error::TmuxError;
use crate::model::{ClientInfo, PaneInfo};
use starfane_proto::{AgentKind, GateCheck, GateReport, SessionState};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Minimum seconds since the last client input before we may inject (default 15).
    pub user_inactive_secs: u64,
    /// Pause between `paste-buffer` and `C-m` (default 250 ms): clears Codex
    /// `PasteBurst` / Claude `isPasting` so Enter is a submit, not a literal newline.
    pub paste_settle_ms: u64,
    /// ACK wait (default 4000 ms). Owned by the daemon, carried here for one config surface.
    pub ack_timeout_ms: u64,
    /// Max pointer length in chars (default 300).
    pub max_pointer_len: usize,
    /// Idle prompt prefixes of the cursor line, per agent.
    pub idle_prompts: BTreeMap<AgentKind, Vec<String>>,
}

impl Default for GateConfig {
    fn default() -> Self {
        let mut idle_prompts = BTreeMap::new();
        idle_prompts.insert(
            AgentKind::Claude,
            vec![
                "❯\u{a0}".to_string(),
                "❯ ".to_string(),
                "> ".to_string(),
                "│ > ".to_string(),
            ],
        );
        idle_prompts.insert(AgentKind::Codex, vec!["› ".to_string()]);
        idle_prompts.insert(AgentKind::Agy, vec!["> ".to_string()]);
        GateConfig {
            user_inactive_secs: 15,
            paste_settle_ms: 250,
            ack_timeout_ms: 4000,
            max_pointer_len: 300,
            idle_prompts,
        }
    }
}

/// True when `client` is attached to `pane`'s session (or shared `session_group`) AND
/// currently viewing `pane`'s window. Falls back to `pane.window_active` when `client.window_id`
/// is empty (legacy 4-field client records).
fn client_viewing_pane_window(client: &ClientInfo, pane: &PaneInfo) -> bool {
    if !pane.same_session_or_group(client) {
        return false;
    }
    if !client.window_id.is_empty() {
        client.window_id == pane.window_id
    } else {
        pane.window_active
    }
}

/// Newest `activity_s` among clients viewing `pane`'s window (same session or session group),
/// or `None` if no client is currently viewing that window.
fn newest_activity_for_pane(clients: &[ClientInfo], pane: &PaneInfo) -> Option<u64> {
    clients
        .iter()
        .filter(|c| client_viewing_pane_window(c, pane))
        .map(|c| c.activity_s)
        .max()
}

/// Seconds since `activity_s`; clock skew (activity in the future) counts as "just now".
fn idle_for(now_s: u64, activity_s: u64) -> u64 {
    now_s.saturating_sub(activity_s)
}

/// Is a human plausibly looking at this pane right now?
///
/// Requires the pane to be visible (`session_attached > 0 && pane_active` and either
/// `window_active` or a grouped-session client displaying `pane.window_id`) AND the *newest*
/// `client_activity` among clients viewing that window to be younger than `watch_window_secs`.
/// Background windows in an attached session are never falsely treated as watched.
/// If the session is attached and visible (`window_active && pane_active`) but no client
/// records were supplied at all (`clients.is_empty()`), we cannot prove the user is away,
/// so we answer `true` (conservative).
pub fn is_user_watching(
    pane: &PaneInfo,
    clients: &[ClientInfo],
    now_s: u64,
    watch_window_secs: u64,
) -> bool {
    if pane.session_attached == 0 || !pane.pane_active {
        return false;
    }
    let has_group_window_viewer = clients.iter().any(|c| {
        pane.same_session_or_group(c) && !c.window_id.is_empty() && c.window_id == pane.window_id
    });
    if !pane.window_active && !has_group_window_viewer {
        return false;
    }
    match newest_activity_for_pane(clients, pane) {
        Some(act) => idle_for(now_s, act) < watch_window_secs,
        None => clients.is_empty(),
    }
}

/// True when the bottom-most lines of a multi-line capture show active TUI chat chrome
/// (normal conversation prompt/footer), suppressing false positives when normal chat
/// merely quotes trust/MCP/update prompt text higher up in the scrollback.
///
/// Restricted to the bottom 5 non-empty lines so stale chat chrome left above a newly
/// launched startup modal (e.g. exiting an old CLI and immediately restarting it in the
/// same tmux pane) never suppresses detection of the active modal at the bottom.
fn has_bottom_active_chat_chrome(non_empty: &[&str]) -> bool {
    if non_empty.len() <= 1 {
        return false;
    }
    let bottom = &non_empty[non_empty.len().saturating_sub(5)..];
    bottom.iter().any(|l| {
        let ll = l.to_ascii_lowercase();
        ll.contains("? for shortcuts")
            || ll.contains("esc to interrupt")
            || ll.contains("ctrl+c to interrupt")
            || ll.contains("context left")
            || ll.contains("ask codex to do anything")
            || ll.contains("shift+tab to cycle")
    })
}

/// Detect whether terminal screen text (visible screen capture, tail capture, or a single cursor
/// line) displays an interactive folder/workspace trust or Claude project MCP server confirmation
/// prompt for `agent`.
///
/// Returns a clear human-readable reason (e.g. `"codex waiting for folder trust approval"` or
/// `"claude waiting for mcp server approval"`) when a confirmation dialog is active, or `None`
/// otherwise. In multi-line captures, active TUI chat chrome at the bottom of the screen
/// suppresses false positives when a normal conversation merely mentions prompt text.
pub fn detect_folder_trust_prompt(agent: AgentKind, text: &str) -> Option<&'static str> {
    if agent == AgentKind::Shell || text.trim().is_empty() {
        return None;
    }
    let non_empty: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }
    let single_line = non_empty.len() == 1;
    let lower = text.to_ascii_lowercase();

    if has_bottom_active_chat_chrome(&non_empty) {
        return None;
    }

    match agent {
        AgentKind::Codex => {
            let has_question = lower.contains("trust this folder?")
                || lower.contains("do you trust the contents of this directory?")
                || lower.contains("do you trust the contents of this folder?");
            let has_header = lower.contains("folder access");
            let has_action = lower.contains("trust and continue")
                || lower.contains("back to agent command center")
                || (lower.contains("yes, continue") && lower.contains("trust"))
                || non_empty.iter().any(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.contains("trust and continue")
                        || (ll.starts_with("› 1.") && ll.contains("trust"))
                });
            let hit = if single_line {
                has_question
                    || has_action
                    || (has_header && lower.contains("trust"))
                    || non_empty[0].eq_ignore_ascii_case("folder access")
            } else {
                (has_question && (has_action || has_header || lower.contains("enter continue")))
                    || (has_header && has_action)
                    || (has_question && lower.contains("trust decision"))
            };
            hit.then_some("codex waiting for folder trust approval")
        }
        AgentKind::Claude => {
            let has_question = lower.contains("do you trust the files in this folder?")
                || lower.contains("is this a project you created or one you trust?")
                || lower.contains("quick safety check");
            let has_action = lower.contains("yes, i trust this folder")
                || lower.contains("yes, proceed")
                || (lower.contains("no, exit") && lower.contains("trust"))
                || non_empty.iter().any(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.contains("yes, i trust this folder")
                        || (ll.starts_with('❯') && ll.contains("no, exit"))
                });
            let has_context = lower.contains("accessing workspace")
                || lower.contains("claude code'll be able to read, edit, and execute files here")
                || lower.contains("enter to confirm");
            let trust_hit = if single_line {
                has_question
                    || lower.contains("yes, i trust this folder")
                    || (non_empty[0].starts_with('❯')
                        && non_empty[0].to_ascii_lowercase().contains("no, exit"))
            } else {
                (has_question && (has_action || has_context))
                    || (lower.contains("yes, i trust this folder") && has_context)
            };
            if trust_hit {
                return Some("claude waiting for folder trust approval");
            }

            let has_mcp_header = lower.contains("new mcp server found in this project")
                || lower.contains("new mcp servers found in this project");
            let has_mcp_action = lower.contains("continue without using this mcp server")
                || lower.contains("continue without using these mcp servers")
                || lower.contains("use this and all future mcp servers")
                || lower.contains("use these and all future mcp servers")
                || non_empty.iter().any(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.starts_with('❯')
                        && (ll.contains("use this mcp server")
                            || ll.contains("use these mcp servers")
                            || ll.contains("continue without using this mcp")
                            || ll.contains("continue without using these mcp"))
                });
            let has_mcp_context = lower.contains("enter to confirm")
                || lower.contains("esc to cancel")
                || lower.contains("mcp servers may execute code")
                || lower.contains("use this mcp server")
                || lower.contains("use these mcp servers");
            let mcp_hit = if single_line {
                has_mcp_header || has_mcp_action
            } else {
                (has_mcp_header && (has_mcp_action || has_mcp_context))
                    || (has_mcp_action && has_mcp_context)
            };
            mcp_hit.then_some("claude waiting for mcp server approval")
        }
        AgentKind::Agy => {
            let has_question = lower.contains("do you trust the contents of this project?")
                || lower.contains("trust this workspace")
                || lower.contains("trust this folder")
                || lower.contains("trust folder");
            let has_action = lower.contains("yes, i trust this folder")
                || lower.contains("yes, trust")
                || non_empty.iter().any(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.starts_with('>') && ll.contains("trust")
                });
            let has_context = lower.contains("accessing workspace")
                || lower.contains(
                    "antigravity cli requires permission to read, edit, and execute files here",
                )
                || (lower.contains("no, exit") && lower.contains("navigate"))
                || lower.contains("enter confirm");
            let hit = if single_line {
                has_question || has_action
            } else {
                (has_question && (has_action || has_context)) || (has_action && has_context)
            };
            hit.then_some("agy waiting for folder trust approval")
        }
        AgentKind::Shell => None,
    }
}

/// Detect Codex's interactive `Update available` startup modal (`Update available · ... /
/// › 1. Update now / 2. Skip / enter continue · esc skip`) so `starfane` never treats its `›`
/// menu marker as an empty idle prompt or injects keystrokes (which could trigger an unwanted
/// npm update).
pub fn detect_codex_update_prompt(agent: AgentKind, text: &str) -> Option<&'static str> {
    if agent != AgentKind::Codex || text.trim().is_empty() {
        return None;
    }
    let non_empty: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }
    let single_line = non_empty.len() == 1;
    let lower = text.to_ascii_lowercase();

    if has_bottom_active_chat_chrome(&non_empty) {
        return None;
    }

    let has_header = lower.contains("update available");
    let has_action = lower.contains("update now")
        || lower.contains("skip until next version")
        || lower.contains("enter continue · esc skip")
        || non_empty.iter().any(|l| {
            let ll = l.to_ascii_lowercase();
            ll.starts_with("› 1. update now") || ll.starts_with("1. update now")
        });
    let hit = if single_line {
        (has_header && (lower.contains('·') || lower.contains('→') || lower.contains("->")))
            || has_action
    } else {
        has_header && (has_action || lower.contains("release notes:"))
    };
    hit.then_some("codex waiting on update prompt")
}

/// Detect any interactive blocking startup modal (folder-trust confirmation across Codex/Claude/AGY,
/// or Codex's `Update available` startup chooser) where text injection must be blocked.
pub fn detect_blocking_startup_prompt(agent: AgentKind, text: &str) -> Option<&'static str> {
    detect_folder_trust_prompt(agent, text).or_else(|| detect_codex_update_prompt(agent, text))
}

/// Strip an idle prompt prefix from the cursor line and report whether the remaining input
/// area is blank. `None` = the line does not look like this agent's idle prompt.
fn input_area_is_empty(
    agent: AgentKind,
    pane: &PaneInfo,
    line: &str,
    prompts: &[String],
) -> Option<bool> {
    if detect_blocking_startup_prompt(agent, line).is_some() {
        return Some(false);
    }
    let is_filler =
        |c: char| c.is_whitespace() || c == '\u{a0}' || c == '│' || c == '┃' || c == '║';
    for p in prompts {
        // capture-pane trims trailing blanks, so an idle "> " comes back as ">".
        let rest = if let Some(r) = line.strip_prefix(p.as_str()) {
            r
        } else if line.trim_end() == p.trim_end() && !p.trim().is_empty() {
            ""
        } else {
            continue;
        };
        if agent == AgentKind::Codex
            && rest.trim() == "Ask Codex to do anything"
            && pane.cursor_x <= 4
        {
            return Some(true);
        }
        if agent == AgentKind::Claude && pane.cursor_x <= 4 {
            let trimmed = rest.trim_start();
            if trimmed.starts_with("Try \"") || trimmed.starts_with("Try '") {
                return Some(true);
            }
        }
        return Some(rest.chars().all(is_filler));
    }
    None
}

/// Compute the settle pause between bracketed paste and Enter (`C-m`), scaling up
/// modestly for multi-line or long payloads so TUI paste-burst detectors (Claude
/// `isPasting`, Codex `PasteBurst`, AGY Ink paste handler) finish settling before
/// submit arrives.
pub fn adaptive_paste_settle_ms(base_ms: u64, text: &str) -> u64 {
    if base_ms == 0 {
        return 0;
    }
    let chars = text.chars().count() as u64;
    let newlines = text.bytes().filter(|&b| b == b'\n').count() as u64;
    let mut extra = 0u64;
    if newlines > 0 {
        extra += 150 + newlines.saturating_sub(1).min(10) * 20;
    }
    if chars > 160 {
        extra += ((chars - 160) / 200).min(6) * 50;
    }
    let cap = base_ms.max(900);
    base_ms.saturating_add(extra).min(cap)
}

/// True when `line` is a horizontal TUI divider rule (e.g. `──────` bordering an input box).
fn is_tui_divider_line(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 8
        && t.chars().all(|c| {
            matches!(
                c,
                '─' | '━'
                    | '═'
                    | '-'
                    | '_'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '╭'
                    | '╮'
                    | '╰'
                    | '╯'
            )
        })
}

/// True when `line` starts with a recognized prompt marker for `agent`.
fn line_starts_with_agent_prompt(agent: AgentKind, raw: &str) -> bool {
    let prefixes: &[&str] = match agent {
        AgentKind::Claude => &["❯\u{a0}", "❯ ", "❯", "> ", "│ > "],
        AgentKind::Codex => &["› ", "›"],
        AgentKind::Agy => &["> ", ">"],
        AgentKind::Shell => return false,
    };
    let line = raw.trim_start_matches(['│', '┃', '║', ' ']);
    prefixes
        .iter()
        .any(|p| line.starts_with(p) || (line.trim_end() == p.trim_end() && !p.trim().is_empty()))
}

/// Extract the active TUI input-box lines containing `cursor_y` from a visible screen capture.
/// Returns `None` if `cursor_y` does not sit inside an active agent prompt box (e.g. during
/// streaming/spinner output or in a non-TUI shell), preventing false-positive duplicate Enters.
fn extract_active_input_box_at_cursor(
    agent: AgentKind,
    screen: &str,
    cursor_y: u32,
) -> Option<String> {
    if agent == AgentKind::Shell {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let cy = (cursor_y as usize).min(lines.len().saturating_sub(1));
    // Walk up from cursor_y (at most 8 lines) without crossing a TUI horizontal divider rule.
    let min_up = cy.saturating_sub(8);
    let mut prompt_idx: Option<usize> = None;
    for i in (min_up..=cy).rev() {
        let line = lines[i];
        if is_tui_divider_line(line) {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("esc to interrupt") || lower.contains("ctrl+c to interrupt") {
            return None;
        }
        if line_starts_with_agent_prompt(agent, line) {
            prompt_idx = Some(i);
            break;
        }
    }
    let start = prompt_idx?;
    let max_down = (cy + 2).min(lines.len().saturating_sub(1));
    let mut box_lines = Vec::new();
    for &raw in &lines[start..=max_down] {
        if is_tui_divider_line(raw) {
            break;
        }
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        if !box_lines.is_empty()
            && (lower.starts_with("? for shortcuts")
                || trimmed.starts_with("● Swarm(")
                || lower.contains("context left")
                || lower.contains("esc to interrupt"))
        {
            break;
        }
        box_lines.push(raw);
    }
    Some(box_lines.join("\n"))
}

/// Extract distinctive fragments (>= 6 chars) from `pasted_text` for stuck-in-prompt checks.
fn paste_needles(pasted_text: &str) -> Vec<String> {
    let mut needles = Vec::new();
    let mut push_slice = |s: &str| {
        let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let chars: Vec<char> = collapsed.chars().collect();
        if chars.len() >= 6 {
            let head: String = chars.iter().take(24).collect();
            if !needles.contains(&head) {
                needles.push(head);
            }
            if chars.len() > 24 {
                let tail: String = chars[chars.len().saturating_sub(24)..].iter().collect();
                if !needles.contains(&tail) {
                    needles.push(tail);
                }
            }
        }
    };
    let non_empty: Vec<&str> = pasted_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(first) = non_empty.first() {
        push_slice(first);
    }
    if let Some(last) = non_empty.last() {
        push_slice(last);
    }
    needles
}

/// Detect whether `pasted_text` is still sitting unsubmitted in the TUI prompt input box
/// after `submit_enter` (e.g. because `C-m` was absorbed by a TUI paste-burst handler).
///
/// Conservative by design to avoid false-positive duplicate Enters:
/// - Always `false` for `Shell` or empty captures.
/// - Always `false` if `cursor_y` is not inside an active prompt box or the box is empty.
/// - Returns `true` ONLY when the active input box at `cursor_y` contains a distinctive
///   fragment of `pasted_text` OR a newly-appeared TUI paste badge (`[Pasted text` /
///   `[Pasted Content`) that was NOT already present in `pre_screen`.
pub fn is_paste_stuck_in_prompt(
    agent: AgentKind,
    pre_screen: &str,
    post_screen: &str,
    post_cursor_y: u32,
    pasted_text: &str,
) -> bool {
    if agent == AgentKind::Shell || post_screen.trim().is_empty() {
        return false;
    }
    let Some(post_box) = extract_active_input_box_at_cursor(agent, post_screen, post_cursor_y)
    else {
        return false;
    };
    let post_first = post_box.lines().next().unwrap_or("");
    let cfg = GateConfig::default();
    if let Some(prompts) = cfg.idle_prompts.get(&agent) {
        let probe_pane = PaneInfo {
            pane_id: starfane_proto::PaneId::new("%0"),
            session_name: String::new(),
            window_id: String::new(),
            window_index: 0,
            pane_index: 0,
            pane_pid: 0,
            current_command: String::new(),
            current_path: String::new(),
            pane_active: true,
            window_active: true,
            session_attached: 0,
            in_mode: false,
            dead: false,
            synchronized: false,
            alternate_on: false,
            cursor_x: 2,
            cursor_y: post_cursor_y,
            window_name: String::new(),
            session_group: String::new(),
            history_size: 0,
            pane_height: 24,
        };
        let rest_blank = post_box.lines().skip(1).all(|l| l.trim().is_empty());
        if rest_blank && input_area_is_empty(agent, &probe_pane, post_first, prompts) == Some(true)
        {
            return false;
        }
    }

    let post_norm: String = post_box.split_whitespace().collect::<Vec<_>>().join(" ");
    let pre_norm: String = pre_screen.split_whitespace().collect::<Vec<_>>().join(" ");

    const PASTE_BADGES: &[&str] = &["[Pasted text", "[Pasted Content", "[pasted text"];
    for badge in PASTE_BADGES {
        if post_norm.contains(badge) && !pre_norm.contains(badge) {
            return true;
        }
    }

    for needle in paste_needles(pasted_text) {
        if post_norm.contains(&needle) && !pre_norm.contains(&needle) {
            return true;
        }
    }
    false
}

/// Pure injection gate (§4). Checks, in order: PaneAlive, NotInMode, NotSynchronized,
/// AgentIdle, UserInactive, InputEmpty. `Ack` is evaluated by the daemon, not here.
/// The seventh gate point (bare slash command) concerns the injected text and is enforced by
/// [`validate_pointer_text`], which `inject_pointer` always runs.
pub fn check_injection_gate(
    pane: &PaneInfo,
    clients: &[ClientInfo],
    state: SessionState,
    agent: AgentKind,
    cursor_line: &str,
    cfg: &GateConfig,
    now_s: u64,
) -> GateReport {
    check_injection_gate_with_screen(pane, clients, state, agent, cursor_line, None, cfg, now_s)
}

/// Variant of [`check_injection_gate`] that also inspects an optional visible-screen capture so
/// folder-trust dialogs (whose cursor may sit on a blank line or footer row) always fail
/// `InputEmpty` with an explicit trust-approval reason.
#[allow(clippy::too_many_arguments)]
pub fn check_injection_gate_with_screen(
    pane: &PaneInfo,
    clients: &[ClientInfo],
    state: SessionState,
    agent: AgentKind,
    cursor_line: &str,
    visible_screen: Option<&str>,
    cfg: &GateConfig,
    now_s: u64,
) -> GateReport {
    let mut notes: Vec<String> = Vec::new();

    let alive = !pane.dead;
    let not_in_mode = !pane.in_mode;
    let not_sync = !pane.synchronized;
    let idle = state == SessionState::Idle;
    if !idle {
        notes.push(format!("agent state is {state:?}, not Idle"));
    }

    let user_inactive = match newest_activity_for_pane(clients, pane) {
        Some(act) => {
            let quiet = idle_for(now_s, act);
            if quiet <= cfg.user_inactive_secs {
                notes.push(format!(
                    "client active {quiet}s ago (need > {}s)",
                    cfg.user_inactive_secs
                ));
            }
            quiet > cfg.user_inactive_secs
        }
        None => true,
    };

    let modal_block = visible_screen
        .and_then(|screen| detect_blocking_startup_prompt(agent, screen))
        .or_else(|| detect_blocking_startup_prompt(agent, cursor_line));

    let input_empty = if let Some(reason) = modal_block {
        notes.push(reason.to_string());
        false
    } else {
        match cfg.idle_prompts.get(&agent) {
            Some(prompts) => match input_area_is_empty(agent, pane, cursor_line, prompts) {
                Some(empty) => {
                    if !empty {
                        notes.push("input box holds a draft".to_string());
                    }
                    empty
                }
                None => {
                    notes.push("cursor line is not a recognised idle prompt".to_string());
                    false
                }
            },
            None => {
                notes.push(format!("no idle prompts configured for {agent}"));
                false
            }
        }
    };

    let checks = vec![
        (GateCheck::PaneAlive, alive),
        (GateCheck::NotInMode, not_in_mode),
        (GateCheck::NotSynchronized, not_sync),
        (GateCheck::AgentIdle, idle),
        (GateCheck::UserInactive, user_inactive),
        (GateCheck::InputEmpty, input_empty),
    ];
    for (c, ok) in &checks {
        if !ok
            && !matches!(
                c,
                GateCheck::AgentIdle | GateCheck::UserInactive | GateCheck::InputEmpty
            )
        {
            notes.push(format!("{c:?} failed"));
        }
    }
    let detail = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    GateReport::from_checks(checks, detail)
}

/// Relaxed gate for explicit interactive sends from the Web UI: enforces ONLY the 3 hard
/// tmux safety checks (`PaneAlive`, `NotInMode`, `NotSynchronized`) and skips `AgentIdle`,
/// `UserInactive`, and `InputEmpty`.
pub fn check_interactive_gate(pane: &PaneInfo) -> GateReport {
    let alive = !pane.dead;
    let not_in_mode = !pane.in_mode;
    let not_sync = !pane.synchronized;
    let checks = vec![
        (GateCheck::PaneAlive, alive),
        (GateCheck::NotInMode, not_in_mode),
        (GateCheck::NotSynchronized, not_sync),
    ];
    let mut notes = Vec::new();
    if !alive {
        notes.push("pane is dead".to_string());
    }
    if !not_in_mode {
        notes.push("pane is in copy-mode".to_string());
    }
    if !not_sync {
        notes.push("window has synchronize-panes enabled".to_string());
    }
    let detail = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    GateReport::from_checks(checks, detail)
}

/// True when `text` is a slash command with no argument (e.g. `/swarm`, `/help `).
/// Injecting a bare slash command would open a TUI picker / run it without a task.
pub fn is_bare_slash_command(text: &str) -> bool {
    let t = text.trim();
    t.starts_with('/') && !t.contains(char::is_whitespace)
}

/// Validate pointer text for `inject_pointer`: non-empty, single line, no control characters
/// (an embedded ESC could terminate the bracketed paste early), within `max_pointer_len`,
/// and not a bare slash command.
pub fn validate_pointer_text(text: &str, cfg: &GateConfig) -> Result<(), TmuxError> {
    let bad = |m: String| Err(TmuxError::InvalidText(m));
    if text.trim().is_empty() {
        return bad("text is empty".into());
    }
    if text.contains(['\n', '\r']) {
        return bad("text must be a single line".into());
    }
    if text.chars().any(char::is_control) {
        return bad("text contains control characters".into());
    }
    let n = text.chars().count();
    if n > cfg.max_pointer_len {
        return bad(format!("text is {n} chars, max is {}", cfg.max_pointer_len));
    }
    if is_bare_slash_command(text) {
        return bad("bare slash command without arguments".into());
    }
    Ok(())
}

/// Validate explicit interactive user input before bracketed-paste injection: non-empty,
/// no dangerous control chars (like ESC that can break bracketed paste), capped at 4000 chars.
pub fn validate_interactive_text(text: &str) -> Result<(), TmuxError> {
    let bad = |m: String| Err(TmuxError::InvalidText(m));
    if text.trim().is_empty() {
        return bad("text is empty".into());
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return bad("text contains control characters".into());
    }
    let n = text.chars().count();
    if n > 4000 {
        return bad(format!("text is {n} chars, max interactive length is 4000"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starfane_proto::PaneId;

    fn sample_pane() -> PaneInfo {
        PaneInfo {
            pane_id: PaneId::new("%5"),
            session_name: "prome".into(),
            window_id: "@1".into(),
            window_index: 0,
            pane_index: 0,
            pane_pid: 1000,
            current_command: "2.1.299".into(),
            current_path: "/tmp".into(),
            pane_active: true,
            window_active: true,
            session_attached: 1,
            in_mode: false,
            dead: false,
            synchronized: false,
            alternate_on: false,
            cursor_x: 0,
            cursor_y: 0,
            window_name: "code".into(),
            session_group: String::new(),
            history_size: 0,
            pane_height: 24,
        }
    }

    #[test]
    fn interactive_gate_checks_only_hard_safety_rules() {
        let p = sample_pane();
        let ok = check_interactive_gate(&p);
        assert!(ok.passed);
        assert_eq!(ok.checks.len(), 3);
        assert_eq!(
            ok.checks.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
            vec![
                GateCheck::PaneAlive,
                GateCheck::NotInMode,
                GateCheck::NotSynchronized
            ]
        );

        let mut dead = sample_pane();
        dead.dead = true;
        assert_eq!(
            check_interactive_gate(&dead).failed_checks(),
            vec![GateCheck::PaneAlive]
        );

        let mut in_mode = sample_pane();
        in_mode.in_mode = true;
        assert_eq!(
            check_interactive_gate(&in_mode).failed_checks(),
            vec![GateCheck::NotInMode]
        );

        let mut sync = sample_pane();
        sync.synchronized = true;
        assert_eq!(
            check_interactive_gate(&sync).failed_checks(),
            vec![GateCheck::NotSynchronized]
        );
    }

    #[test]
    fn interactive_text_validation_allows_multiline_and_slash_rejects_esc() {
        assert!(validate_interactive_text("hello\nworld\t1").is_ok());
        assert!(validate_interactive_text("/compact").is_ok());
        assert!(validate_interactive_text("   \n  ").is_err());
        assert!(validate_interactive_text("bad\x1b[201~escape").is_err());
        assert!(validate_interactive_text(&"a".repeat(4001)).is_err());
    }
}
