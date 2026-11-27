//! Terminal multiplexer abstraction ([`TerminalMux`]) supporting tmux and Zellij (>= 0.44).

use crate::discover::{CaptureRequest, DiscoverOptions, DiscoveredPane, PanePreview};
use crate::error::TmuxError;
use crate::gate::GateConfig;
use crate::model::{ClientInfo, PaneInfo, PaneStatus};
use crate::{FocusOutcome, SpawnWindow};
use serde::{Deserialize, Serialize};
use starfane_proto::{HostId, PaneId, SessionId, LOCAL_HOST_ID};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuxKind {
    Tmux,
    Zellij,
}

impl MuxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MuxKind::Tmux => "tmux",
            MuxKind::Zellij => "zellij",
        }
    }
}

impl fmt::Display for MuxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Capabilities advertised by a multiplexer backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxCapabilities {
    pub bracketed_paste_native: bool,
    pub client_activity_ts: bool,
    pub pane_user_options: bool,
    pub full_scrollback: bool,
    pub layout_snapshot: bool,
}

/// Snapshot used by the delivery gate before pasting into a pane.
#[derive(Debug, Clone)]
pub struct GateProbe {
    pub pane: PaneInfo,
    pub clients: Vec<ClientInfo>,
    pub cursor_line: String,
}

/// Unified terminal multiplexer interface implemented by [`crate::Tmux`] and [`crate::ZellijMux`].
pub trait TerminalMux: Send + Sync {
    fn kind(&self) -> MuxKind;
    fn capabilities(&self) -> MuxCapabilities;
    fn available(&self) -> bool;

    fn list_sessions(&self) -> Result<Vec<String>, TmuxError>;
    fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError>;
    fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError>;

    fn discover_panes_from(&self, panes: &[PaneInfo], opts: DiscoverOptions)
        -> Vec<DiscoveredPane>;

    fn capture_pane(&self, pane: &PaneId, req: &CaptureRequest) -> Result<PanePreview, TmuxError>;

    fn capture_panes_tail_batch(&self, targets: &[(&PaneId, usize)]) -> HashMap<PaneId, String>;

    fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError>;

    /// Capture the current visible screen of `pane` (rows `0..pane_height`, matching `cursor_y`).
    fn capture_visible_screen(&self, pane: &PaneId) -> Result<String, TmuxError> {
        self.capture_pane(pane, &CaptureRequest::tail(40))
            .map(|p| p.text)
    }

    /// Safely cancel tmux scrollback/copy-mode on `pane` without sending bytes to the PTY.
    /// Returns `Ok(true)` when the pane is confirmed out of mode.
    fn exit_copy_mode(&self, pane: &PaneId) -> Result<bool, TmuxError> {
        let _ = pane;
        Ok(false)
    }

    fn paste_pointer_text(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError>;

    fn paste_interactive_text(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError>;
    fn submit_enter(&self, pane: &PaneId) -> Result<(), TmuxError>;
    fn send_key(&self, pane: &PaneId, key: &str) -> Result<(), TmuxError>;

    fn spawn_window(&self, req: &SpawnWindow) -> Result<PaneId, TmuxError>;
    fn focus_pane(&self, pane: &PaneId) -> Result<FocusOutcome, TmuxError>;
    fn kill_pane(&self, pane: &PaneId) -> Result<(), TmuxError>;
    fn set_status(&self, pane: &PaneId, st: &PaneStatus) -> Result<(), TmuxError>;
    fn clear_status(&self, pane: &PaneId) -> Result<(), TmuxError>;
    fn dump_layout(&self, session: &str) -> Result<String, TmuxError>;
}

/// Validate an explicit human-triggered interactive menu/approval key against the strict allowlist
/// and map it to its safe `tmux send-keys` token. Rejects arbitrary strings or shell metacharacters.
pub fn validate_interactive_key(key: &str) -> Result<&'static str, TmuxError> {
    match key {
        "Enter" => Ok("C-m"),
        "Escape" => Ok("Escape"),
        "Up" => Ok("Up"),
        "Down" => Ok("Down"),
        "Left" => Ok("Left"),
        "Right" => Ok("Right"),
        "Tab" => Ok("Tab"),
        "Space" => Ok("Space"),
        "y" => Ok("y"),
        "n" => Ok("n"),
        "1" => Ok("1"),
        "2" => Ok("2"),
        "3" => Ok("3"),
        "4" => Ok("4"),
        "C-c" => Ok("C-c"),
        _ => Err(TmuxError::InvalidRequest(format!(
            "unsupported interactive key {key:?}; allowed: Enter, Escape, Up, Down, Left, Right, Tab, Space, y, n, 1, 2, 3, 4, C-c"
        ))),
    }
}

/// True if `id` is a local tmux pane id like `"%12"`.
pub fn is_tmux_pane_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('%') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Parse a Zellij canonical pane id `"zj:<session>/<numeric_id>"` -> `(session, numeric_id)`.
pub fn parse_zellij_pane_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix("zj:")?;
    let (sess, raw_id) = rest.rsplit_once('/')?;
    if sess.is_empty() || raw_id.is_empty() {
        return None;
    }
    let numeric = raw_id.strip_prefix("terminal_").unwrap_or(raw_id);
    if numeric.is_empty() || !numeric.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((sess, numeric))
}

/// Detect the mux kind for a raw (unprefixed) or host-prefixed pane id.
pub fn pane_mux_kind(id: &str) -> MuxKind {
    let (_, local) = split_host_prefix(id);
    if parse_zellij_pane_id(local).is_some() {
        MuxKind::Zellij
    } else {
        MuxKind::Tmux
    }
}

/// Split an optional host prefix from a pane id (e.g. `"tencent:%12"` -> `(Some("tencent"), "%12")`,
/// `"mac-xiling:zj:work/1"` -> `(Some("mac-xiling"), "zj:work/1")`, `"%12"` -> `(None, "%12")`,
/// `"zj:work/1"` -> `(None, "zj:work/1")`).
pub fn split_host_prefix(id: &str) -> (Option<&str>, &str) {
    if is_tmux_pane_id(id) || parse_zellij_pane_id(id).is_some() {
        return (None, id);
    }
    if let Some((host, rest)) = id.split_once(':') {
        if !host.is_empty()
            && host != "zj"
            && (is_tmux_pane_id(rest) || parse_zellij_pane_id(rest).is_some())
        {
            return (Some(host), rest);
        }
    }
    (None, id)
}

/// Qualify a remote pane id with its immutable host id (`"local"` remains unprefixed for backwards compatibility).
pub fn qualify_host_pane(host_id: &HostId, local_pane: &PaneId) -> PaneId {
    if host_id.as_str().is_empty() || host_id.as_str() == LOCAL_HOST_ID {
        local_pane.clone()
    } else {
        PaneId::new(format!("{}:{}", host_id.as_str(), local_pane.as_str()))
    }
}

/// Qualify a remote session id with its immutable host id (`"local"` remains unprefixed).
pub fn qualify_host_session(host_id: &HostId, remote_session: &SessionId) -> SessionId {
    if host_id.as_str().is_empty() || host_id.as_str() == LOCAL_HOST_ID {
        remote_session.clone()
    } else {
        SessionId::new(format!("{}:{}", host_id.as_str(), remote_session.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_id_classification_and_host_prefix_splitting() {
        assert!(is_tmux_pane_id("%0"));
        assert!(is_tmux_pane_id("%123"));
        assert!(!is_tmux_pane_id("zj:work/1"));

        assert_eq!(parse_zellij_pane_id("zj:work/4"), Some(("work", "4")));
        assert_eq!(
            parse_zellij_pane_id("zj:my-sess/terminal_12"),
            Some(("my-sess", "12"))
        );
        assert_eq!(parse_zellij_pane_id("zj:/4"), None);
        assert_eq!(parse_zellij_pane_id("%4"), None);

        assert_eq!(split_host_prefix("%12"), (None, "%12"));
        assert_eq!(split_host_prefix("zj:work/1"), (None, "zj:work/1"));
        assert_eq!(split_host_prefix("tencent:%12"), (Some("tencent"), "%12"));
        assert_eq!(
            split_host_prefix("mac-xiling:zj:dev/3"),
            (Some("mac-xiling"), "zj:dev/3")
        );
        assert_eq!(pane_mux_kind("mac-xiling:zj:dev/3"), MuxKind::Zellij);
        assert_eq!(pane_mux_kind("tencent:%12"), MuxKind::Tmux);
    }

    #[test]
    fn validate_interactive_key_allows_strict_set_and_rejects_injection() {
        assert_eq!(validate_interactive_key("Enter").unwrap(), "C-m");
        assert_eq!(validate_interactive_key("Escape").unwrap(), "Escape");
        assert_eq!(validate_interactive_key("Up").unwrap(), "Up");
        assert_eq!(validate_interactive_key("Down").unwrap(), "Down");
        assert_eq!(validate_interactive_key("Left").unwrap(), "Left");
        assert_eq!(validate_interactive_key("Right").unwrap(), "Right");
        assert_eq!(validate_interactive_key("Tab").unwrap(), "Tab");
        assert_eq!(validate_interactive_key("Space").unwrap(), "Space");
        assert_eq!(validate_interactive_key("y").unwrap(), "y");
        assert_eq!(validate_interactive_key("n").unwrap(), "n");
        assert_eq!(validate_interactive_key("1").unwrap(), "1");
        assert_eq!(validate_interactive_key("2").unwrap(), "2");
        assert_eq!(validate_interactive_key("3").unwrap(), "3");
        assert_eq!(validate_interactive_key("4").unwrap(), "4");
        assert_eq!(validate_interactive_key("C-c").unwrap(), "C-c");

        for bad in [
            "",
            "Enter ",
            " Enter",
            "enter",
            "C-m",
            "C-d",
            "Enter; rm -rf /",
            "y\n",
            "5",
            "Y",
            "foo",
            "$(whoami)",
        ] {
            assert!(
                matches!(
                    validate_interactive_key(bad),
                    Err(TmuxError::InvalidRequest(_))
                ),
                "expected InvalidRequest for {bad:?}"
            );
        }
    }
}
