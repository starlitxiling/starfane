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

/// Strip an idle prompt prefix from the cursor line and report whether the remaining input
/// area is blank. `None` = the line does not look like this agent's idle prompt.
fn input_area_is_empty(
    agent: AgentKind,
    pane: &PaneInfo,
    line: &str,
    prompts: &[String],
) -> Option<bool> {
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
        return Some(rest.chars().all(is_filler));
    }
    None
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

    let input_empty = match cfg.idle_prompts.get(&agent) {
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
