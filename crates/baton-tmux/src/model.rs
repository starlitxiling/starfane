//! tmux data models and parsers for `list-panes` / `list-clients` output.

use crate::error::TmuxError;
use baton_proto::{PaneId, PipelineId, SessionState, StageKind};
use serde::Serialize;

/// Field separator used in every `-F` format (ASCII Unit Separator).
pub const FIELD_SEP: char = '\x1f';

pub(crate) const PANE_FORMAT: &str = "#{pane_id}\x1f#{session_name}\x1f#{window_id}\x1f#{window_index}\x1f#{pane_index}\x1f#{pane_pid}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{pane_active}\x1f#{window_active}\x1f#{session_attached}\x1f#{pane_in_mode}\x1f#{pane_dead}\x1f#{pane_synchronized}\x1f#{alternate_on}\x1f#{cursor_x}\x1f#{cursor_y}\x1f#{window_name}";
const PANE_FIELDS: usize = 18;

pub(crate) const CLIENT_FORMAT: &str =
    "#{client_tty}\x1f#{client_session}\x1f#{client_activity}\x1f#{client_flags}";
const CLIENT_FIELDS: usize = 4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneInfo {
    pub pane_id: PaneId,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub pane_pid: u32,
    pub current_command: String,
    pub current_path: String,
    pub pane_active: bool,
    pub window_active: bool,
    pub session_attached: u32,
    pub in_mode: bool,
    pub dead: bool,
    pub synchronized: bool,
    pub alternate_on: bool,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub window_name: String,
}

impl PaneInfo {
    /// Human-friendly label `<session_name>-<window_name>` (falls back to `<session_name>` if window name is empty).
    pub fn tmux_label(&self) -> String {
        let win = self.window_name.trim();
        if win.is_empty() {
            self.session_name.clone()
        } else {
            format!("{}-{win}", self.session_name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientInfo {
    pub tty: String,
    pub session_name: String,
    /// `client_activity`: Unix seconds of the client's last keyboard/mouse activity.
    pub activity_s: u64,
    pub flags: String,
}

/// Status written to pane user options by `Tmux::set_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneStatus {
    pub state: SessionState,
    pub role: Option<StageKind>,
    pub pipeline: Option<PipelineId>,
}

fn parse_num<T: std::str::FromStr>(field: &str, name: &str, line: &str) -> Result<T, TmuxError> {
    field
        .trim()
        .parse::<T>()
        .map_err(|_| TmuxError::Parse(format!("bad {name} {field:?} in line {line:?}")))
}

fn parse_flag(field: &str, name: &str, line: &str) -> Result<bool, TmuxError> {
    match field.trim() {
        "0" | "" => Ok(false),
        "1" => Ok(true),
        other => Err(TmuxError::Parse(format!(
            "bad {name} flag {other:?} in line {line:?}"
        ))),
    }
}

/// Parse a single `list-panes -a -F PANE_FORMAT` line.
pub fn parse_pane_line(line: &str) -> Result<PaneInfo, TmuxError> {
    let f: Vec<&str> = line.split(FIELD_SEP).collect();
    if f.len() != PANE_FIELDS {
        return Err(TmuxError::Parse(format!(
            "expected {PANE_FIELDS} fields, got {} in line {line:?}",
            f.len()
        )));
    }
    if !f[0].starts_with('%') {
        return Err(TmuxError::Parse(format!("bad pane_id {:?}", f[0])));
    }
    Ok(PaneInfo {
        pane_id: PaneId::new(f[0]),
        session_name: f[1].to_string(),
        window_id: f[2].to_string(),
        window_index: parse_num(f[3], "window_index", line)?,
        pane_index: parse_num(f[4], "pane_index", line)?,
        pane_pid: parse_num(f[5], "pane_pid", line)?,
        current_command: f[6].to_string(),
        current_path: f[7].to_string(),
        pane_active: parse_flag(f[8], "pane_active", line)?,
        window_active: parse_flag(f[9], "window_active", line)?,
        session_attached: parse_num(f[10], "session_attached", line)?,
        in_mode: parse_flag(f[11], "pane_in_mode", line)?,
        dead: parse_flag(f[12], "pane_dead", line)?,
        synchronized: parse_flag(f[13], "pane_synchronized", line)?,
        alternate_on: parse_flag(f[14], "alternate_on", line)?,
        cursor_x: parse_num(f[15], "cursor_x", line)?,
        cursor_y: parse_num(f[16], "cursor_y", line)?,
        window_name: f[17].to_string(),
    })
}

/// Parse full `list-panes` output; blank lines are skipped, any malformed line is an error.
pub fn parse_panes(out: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse_pane_line)
        .collect()
}

/// Parse a single `list-clients -F CLIENT_FORMAT` line.
pub fn parse_client_line(line: &str) -> Result<ClientInfo, TmuxError> {
    let f: Vec<&str> = line.split(FIELD_SEP).collect();
    if f.len() != CLIENT_FIELDS {
        return Err(TmuxError::Parse(format!(
            "expected {CLIENT_FIELDS} fields, got {} in line {line:?}",
            f.len()
        )));
    }
    Ok(ClientInfo {
        tty: f[0].to_string(),
        session_name: f[1].to_string(),
        activity_s: parse_num(f[2], "client_activity", line)?,
        flags: f[3].to_string(),
    })
}

pub fn parse_clients(out: &str) -> Result<Vec<ClientInfo>, TmuxError> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse_client_line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_format_field_count_matches() {
        assert_eq!(PANE_FORMAT.split(FIELD_SEP).count(), PANE_FIELDS);
        assert_eq!(CLIENT_FORMAT.split(FIELD_SEP).count(), CLIENT_FIELDS);
    }
}
