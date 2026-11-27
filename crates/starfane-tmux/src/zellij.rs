//! Zellij (>= 0.44) multiplexer backend implementing [`TerminalMux`].
//!
//! Uses Zellij's native CLI primitives:
//! - `zellij list-sessions -n` (without `-s`, so `(EXITED ...)` status is visible and filtered)
//! - `zellij -s <session> action list-panes --all --json`
//! - `zellij -s <session> action list-tabs --all --json`
//! - `zellij -s <session> action list-clients`
//! - `zellij -s <session> action dump-screen --pane-id terminal_<id> [--full]`
//! - `zellij -s <session> action paste -p terminal_<id> <text>`
//! - `zellij -s <session> action send-keys -p terminal_<id> Enter`
//! - `zellij -s <session> action focus-pane-id terminal_<id>`
//! - `zellij -s <session> action close-pane --pane-id terminal_<id>`
//! - `zellij -s <session> action new-pane --name <title> --cwd <dir> -- <argv...>`
//! - `zellij -s <session> action dump-layout`

use crate::discover::{
    cap_page_bytes, classify_panes_with_git_lookup, trim_and_cap_tail, CaptureRequest,
    DiscoverOptions, DiscoveredPane, PanePreview, MAX_CAPTURE_BYTES, MAX_CAPTURE_LINES,
    MAX_FULL_CAPTURE_BYTES, MAX_FULL_CAPTURE_LINES,
};
use crate::error::TmuxError;
use crate::exec::{ExecTransport, LocalExec};
use crate::gate::{validate_interactive_text, validate_pointer_text, GateConfig};
use crate::model::{ClientInfo, PaneInfo, PaneStatus};
use crate::mux::{
    parse_zellij_pane_id, validate_interactive_key, MuxCapabilities, MuxKind, SuspendPaneReq,
    TerminalMux, WakePaneReq,
};
use crate::{FocusOutcome, SpawnWindow, DEFAULT_DETACHED_SESSION};
use serde::Deserialize;
use starfane_proto::PaneId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MIN_ZELLIJ_VERSION: (u32, u32) = (0, 44);
const SESSION_INIT_WAIT: Duration = Duration::from_secs(3);
const SESSION_INIT_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct ZellijMux {
    pub bin: PathBuf,
    transport: Arc<dyn ExecTransport>,
}

impl ZellijMux {
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            transport: Arc::new(LocalExec::default()),
        }
    }

    pub fn with_transport(bin: PathBuf, transport: Arc<dyn ExecTransport>) -> Self {
        Self { bin, transport }
    }

    fn run_raw(&self, args: &[String], stdin: Option<&[u8]>) -> Result<String, TmuxError> {
        let out = self.transport.run(&self.bin, args, stdin)?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(TmuxError::Command {
                args: args.join(" "),
                code: out.code,
                stderr: out.stderr,
            })
        }
    }

    fn run_session_action(
        &self,
        session: &str,
        action_args: &[String],
    ) -> Result<String, TmuxError> {
        let mut args = Vec::with_capacity(3 + action_args.len());
        args.push("-s".to_string());
        args.push(session.to_string());
        args.push("action".to_string());
        args.extend(action_args.iter().cloned());
        self.run_raw(&args, None)
    }

    /// Detect the installed Zellij `(major, minor)` version if runnable.
    pub fn version(&self) -> Option<(u32, u32)> {
        let out = self
            .transport
            .run(&self.bin, &["--version".to_string()], None)
            .ok()?;
        if !out.success {
            return None;
        }
        parse_zellij_version(&out.stdout)
    }

    fn require_pane<'a>(&self, pane: &'a PaneId) -> Result<(&'a str, &'a str), TmuxError> {
        parse_zellij_pane_id(pane.as_str()).ok_or_else(|| {
            TmuxError::InvalidRequest(format!(
                "zellij pane id must look like zj:<session>/<id>, got {:?}",
                pane.as_str()
            ))
        })
    }

    fn ensure_session(&self, session: &str) -> Result<(), TmuxError> {
        let existing = self.list_sessions().unwrap_or_default();
        if existing.iter().any(|s| s == session) {
            return Ok(());
        }
        let args = vec!["attach".to_string(), "-b".to_string(), session.to_string()];
        self.run_raw(&args, None)?;
        let deadline = Instant::now() + SESSION_INIT_WAIT;
        let probe_args = vec![
            "list-panes".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        while Instant::now() < deadline {
            if let Ok(out) = self.run_session_action(session, &probe_args) {
                if !out.trim().is_empty() {
                    return Ok(());
                }
            }
            thread::sleep(SESSION_INIT_POLL);
        }
        Ok(())
    }

    /// Kill and delete a session (intended for sandbox tests).
    pub fn delete_session(&self, session: &str) -> Result<(), TmuxError> {
        let _ = self.run_raw(&["kill-session".to_string(), session.to_string()], None);
        let _ = self.run_raw(
            &[
                "delete-session".to_string(),
                "-f".to_string(),
                session.to_string(),
            ],
            None,
        );
        Ok(())
    }
}

impl TerminalMux for ZellijMux {
    fn kind(&self) -> MuxKind {
        MuxKind::Zellij
    }

    fn capabilities(&self) -> MuxCapabilities {
        MuxCapabilities {
            bracketed_paste_native: true,
            client_activity_ts: false,
            pane_user_options: false,
            full_scrollback: true,
            layout_snapshot: true,
        }
    }

    fn available(&self) -> bool {
        self.version().is_some_and(|v| v >= MIN_ZELLIJ_VERSION)
    }

    fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        // Must omit `-s` (`--short`), because `-s` strips `(EXITED - attach to resurrect)`
        // and would make dead resurrectable sessions indistinguishable from active ones.
        let out = self.transport.run(
            &self.bin,
            &["list-sessions".to_string(), "-n".to_string()],
            None,
        )?;
        if !out.success {
            let lower = format!("{}{}", out.stdout, out.stderr).to_ascii_lowercase();
            if lower.contains("no active zellij sessions") || lower.contains("no sessions") {
                return Ok(Vec::new());
            }
            return Err(TmuxError::Command {
                args: "list-sessions -n".into(),
                code: out.code,
                stderr: out.stderr,
            });
        }
        Ok(parse_zellij_sessions(&out.stdout))
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        let sessions = self.list_sessions()?;
        let mut all = Vec::new();
        let panes_args = vec![
            "list-panes".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        let tabs_args = vec![
            "list-tabs".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        let clients_args = vec!["list-clients".to_string()];

        for sess in sessions {
            let panes_json = match self.run_session_action(&sess, &panes_args) {
                Ok(s) if !s.trim().is_empty() => s,
                _ => continue,
            };
            let tabs_json = self
                .run_session_action(&sess, &tabs_args)
                .unwrap_or_default();
            let tabs = parse_zellij_tabs_json(&tabs_json).unwrap_or_default();
            let attached_count = self
                .run_session_action(&sess, &clients_args)
                .map(|c| parse_zellij_clients_text(&sess, &c, now_secs(), None).len() as u32)
                .unwrap_or(0);
            if let Ok(mut parsed) =
                parse_zellij_panes_json(&sess, &panes_json, &tabs, attached_count)
            {
                all.append(&mut parsed);
            }
        }
        Ok(all)
    }

    fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError> {
        let sessions = self.list_sessions()?;
        let mut all = Vec::new();
        let now_s = now_secs();
        let clients_args = vec!["list-clients".to_string()];
        let panes_args = vec![
            "list-panes".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        for sess in sessions {
            if let Ok(out) = self.run_session_action(&sess, &clients_args) {
                let raw_clients = parse_zellij_clients_text(&sess, &out, now_s, None);
                if raw_clients.is_empty() {
                    continue;
                }
                // Resolve each client's focused `terminal_<id>` token to its `tab_<tab_id>`
                // so `ClientInfo.window_id` aligns with `PaneInfo.window_id` for gate/watching checks.
                let pane_to_tab = self
                    .run_session_action(&sess, &panes_args)
                    .ok()
                    .and_then(|json| extract_zellij_pane_tab_map(&json).ok());
                all.extend(parse_zellij_clients_text(
                    &sess,
                    &out,
                    now_s,
                    pane_to_tab.as_ref(),
                ));
            }
        }
        Ok(all)
    }

    fn discover_panes_from(
        &self,
        panes: &[PaneInfo],
        opts: DiscoverOptions,
    ) -> Vec<DiscoveredPane> {
        if panes.is_empty() {
            return Vec::new();
        }
        // Zellij panes carry `pane_command` directly in `PaneInfo.current_command`, so
        // `classify_panes_with_git_lookup` classifies them without needing a PID subtree walk.
        classify_panes_with_git_lookup(panes, "", opts, self.transport.is_local())
    }

    fn capture_pane(&self, pane: &PaneId, req: &CaptureRequest) -> Result<PanePreview, TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        let term_id = format!("terminal_{numeric}");
        let mut args = vec!["dump-screen".to_string(), "--pane-id".to_string(), term_id];
        if req.all || req.before_line.is_some() || req.lines > 60 {
            args.push("--full".to_string());
        }
        let raw = self.run_session_action(sess, &args)?;
        Ok(slice_zellij_dump(pane, &raw, req))
    }

    fn capture_panes_tail_batch(&self, targets: &[(&PaneId, usize)]) -> HashMap<PaneId, String> {
        let mut out = HashMap::new();
        for (pane, lines) in targets {
            let Ok((sess, numeric)) = self.require_pane(pane) else {
                continue;
            };
            let args = vec![
                "dump-screen".to_string(),
                "--pane-id".to_string(),
                format!("terminal_{numeric}"),
            ];
            if let Ok(raw) = self.run_session_action(sess, &args) {
                let text = trim_and_cap_tail(
                    &raw,
                    (*lines).clamp(1, MAX_CAPTURE_LINES),
                    MAX_CAPTURE_BYTES,
                );
                out.insert((*pane).clone(), text);
            }
        }
        out
    }

    fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        let args = vec![
            "dump-screen".to_string(),
            "--pane-id".to_string(),
            format!("terminal_{numeric}"),
        ];
        let raw = self.run_session_action(sess, &args)?;
        let line = raw
            .lines()
            .nth(cursor_y as usize)
            .unwrap_or("")
            .trim_end_matches('\r')
            .to_string();
        Ok(line)
    }

    fn paste_pointer_text(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        validate_pointer_text(text, cfg)?;
        let args = vec![
            "paste".to_string(),
            "-p".to_string(),
            format!("terminal_{numeric}"),
            "--".to_string(),
            text.to_string(),
        ];
        self.run_session_action(sess, &args)?;
        Ok(())
    }

    fn paste_interactive_text(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        validate_interactive_text(text)?;
        let args = vec![
            "paste".to_string(),
            "-p".to_string(),
            format!("terminal_{numeric}"),
            "--".to_string(),
            text.to_string(),
        ];
        self.run_session_action(sess, &args)?;
        Ok(())
    }

    fn submit_enter(&self, pane: &PaneId) -> Result<(), TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        let args = vec![
            "send-keys".to_string(),
            "-p".to_string(),
            format!("terminal_{numeric}"),
            "Enter".to_string(),
        ];
        self.run_session_action(sess, &args)?;
        Ok(())
    }

    fn send_key(&self, pane: &PaneId, key: &str) -> Result<(), TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        let zj_key = map_interactive_key_for_zellij(key)?;
        let args = vec![
            "send-keys".to_string(),
            "-p".to_string(),
            format!("terminal_{numeric}"),
            zj_key.to_string(),
        ];
        match self.run_session_action(sess, &args) {
            Ok(_) => Ok(()),
            Err(TmuxError::Command {
                args: cmd_args,
                code,
                stderr,
            }) => {
                let lower = stderr.to_ascii_lowercase();
                if lower.contains("unexpected argument")
                    || lower.contains("wasn't expected")
                    || lower.contains("unrecognized subcommand")
                {
                    return Err(TmuxError::InvalidRequest(
                        "installed zellij does not support targeted `action send-keys -p`; upgrade to zellij >= 0.44".into(),
                    ));
                }
                Err(TmuxError::Command {
                    args: cmd_args,
                    code,
                    stderr,
                })
            }
            Err(e) => Err(e),
        }
    }

    fn spawn_window(&self, req: &SpawnWindow) -> Result<PaneId, TmuxError> {
        if req.argv.is_empty() || req.argv[0].is_empty() {
            return Err(TmuxError::InvalidRequest("argv is empty".into()));
        }
        if req.window_name.is_empty() {
            return Err(TmuxError::InvalidRequest("window_name is empty".into()));
        }
        let session = req
            .target_session
            .clone()
            .unwrap_or_else(|| DEFAULT_DETACHED_SESSION.to_string());
        if session.is_empty() || session.contains(['/', ':']) {
            return Err(TmuxError::InvalidRequest(format!(
                "bad zellij session name {session:?}"
            )));
        }
        let may_create = req.create_session || req.target_session.is_none();
        if may_create {
            self.ensure_session(&session)?;
        }
        let mut args = vec![
            "new-pane".to_string(),
            "--name".to_string(),
            req.window_name.clone(),
            "--cwd".to_string(),
            req.cwd.to_string_lossy().into_owned(),
            "--".to_string(),
        ];
        if !req.env.is_empty() {
            args.push("/usr/bin/env".to_string());
            for (k, v) in &req.env {
                args.push(format!("{k}={v}"));
            }
        }
        args.extend(req.argv.iter().cloned());
        let out = self.run_session_action(&session, &args)?;
        let raw = out.trim();
        let numeric = raw.strip_prefix("terminal_").unwrap_or(raw);
        if numeric.is_empty() || !numeric.bytes().all(|b| b.is_ascii_digit()) {
            return Err(TmuxError::Parse(format!(
                "zellij new-pane printed unexpected pane id: {out:?}"
            )));
        }
        Ok(PaneId::new(format!("zj:{session}/{numeric}")))
    }

    fn suspend_pane(&self, req: &SuspendPaneReq) -> Result<PaneId, TmuxError> {
        let (sess, numeric) = self.require_pane(&req.pane)?;
        let args = build_zellij_replace_pane_args(
            numeric,
            req.pane_name.as_deref(),
            req.cwd.as_deref(),
            true,
            &req.resume_argv,
        )?;
        let list_args = vec![
            "list-panes".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        let before_raw = self
            .run_session_action(sess, &list_args)
            .unwrap_or_default();
        let before_ids = parse_zellij_terminal_ids(&before_raw);
        let out = self.run_session_action(sess, &args)?;
        let raw = out.trim();
        let direct = raw.strip_prefix("terminal_").unwrap_or(raw);
        if !direct.is_empty() && direct.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(PaneId::new(format!("zj:{sess}/{direct}")));
        }

        // Zellij 0.46 prints empty stdout on `new-pane --in-place --close-replaced-pane`.
        // Diff `list-panes --all --json` against `before_ids` to obtain the replacement pane id.
        for attempt in 0..8 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(40));
            }
            if let Ok(after_raw) = self.run_session_action(sess, &list_args) {
                if let Some(new_id) = pick_replacement_zellij_terminal_id(
                    &after_raw,
                    &before_ids,
                    req.pane_name.as_deref(),
                ) {
                    return Ok(PaneId::new(format!("zj:{sess}/{new_id}")));
                }
            }
        }
        Err(TmuxError::Parse(format!(
            "zellij suspend new-pane did not yield replacement pane id (old={})",
            req.pane
        )))
    }

    fn wake_suspended_pane(&self, req: &WakePaneReq) -> Result<PaneId, TmuxError> {
        self.submit_enter(&req.pane)?;
        Ok(req.pane.clone())
    }

    fn focus_pane(&self, pane: &PaneId) -> Result<FocusOutcome, TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        let term_id = format!("terminal_{numeric}");
        self.run_session_action(sess, &["focus-pane-id".to_string(), term_id])?;
        let clients = self
            .run_session_action(sess, &["list-clients".to_string()])
            .map(|c| parse_zellij_clients_text(sess, &c, now_secs(), None))
            .unwrap_or_default();
        let switched_client = clients.first().map(|c| c.tty.clone());
        Ok(FocusOutcome {
            pane_id: pane.clone(),
            session_name: sess.to_string(),
            tmux_target: format!("zj:{sess}/{numeric}"),
            already_in_session: switched_client.is_some(),
            switched_client,
        })
    }

    fn kill_pane(&self, pane: &PaneId) -> Result<(), TmuxError> {
        let (sess, numeric) = self.require_pane(pane)?;
        // Always pass `--pane-id` explicitly so Zellij targets ONLY `terminal_<numeric>` and
        // fails safely on older binaries rather than closing the currently focused pane.
        let args = vec![
            "close-pane".to_string(),
            "--pane-id".to_string(),
            format!("terminal_{numeric}"),
        ];
        match self.run_session_action(sess, &args) {
            Ok(_) => Ok(()),
            Err(TmuxError::Command {
                args: cmd_args,
                code,
                stderr,
            }) => {
                let lower = stderr.to_ascii_lowercase();
                if lower.contains("unexpected argument") || lower.contains("wasn't expected") {
                    return Err(TmuxError::InvalidRequest(
                        "installed zellij does not support `action close-pane --pane-id`; refusing to close the currently focused pane".into(),
                    ));
                }
                if lower.contains("not found")
                    || lower.contains("no such pane")
                    || lower.contains("no active zellij sessions")
                    || lower.contains("no session named")
                    || lower.contains("does not exist")
                {
                    return Ok(());
                }
                Err(TmuxError::Command {
                    args: cmd_args,
                    code,
                    stderr,
                })
            }
            Err(e) => Err(e),
        }
    }

    fn set_status(&self, pane: &PaneId, _st: &PaneStatus) -> Result<(), TmuxError> {
        // Zellij does not expose per-pane arbitrary user options like tmux `@starfane_state`.
        self.require_pane(pane)?;
        Ok(())
    }

    fn clear_status(&self, pane: &PaneId) -> Result<(), TmuxError> {
        self.require_pane(pane)?;
        Ok(())
    }

    fn dump_layout(&self, session: &str) -> Result<String, TmuxError> {
        self.run_session_action(session, &["dump-layout".to_string()])
    }
}

fn map_interactive_key_for_zellij(key: &str) -> Result<&'static str, TmuxError> {
    validate_interactive_key(key)?;
    match key {
        "Enter" => Ok("Enter"),
        "Escape" => Ok("Esc"),
        "Up" => Ok("Up"),
        "Down" => Ok("Down"),
        "Left" => Ok("Left"),
        "Right" => Ok("Right"),
        "Tab" => Ok("Tab"),
        "Space" => Ok("Space"),
        "Backspace" => Ok("Backspace"),
        "C-c" => Ok("Ctrl c"),
        "y" => Ok("y"),
        "n" => Ok("n"),
        "1" => Ok("1"),
        "2" => Ok("2"),
        "3" => Ok("3"),
        "4" => Ok("4"),
        "5" => Ok("5"),
        "6" => Ok("6"),
        "7" => Ok("7"),
        "8" => Ok("8"),
        "9" => Ok("9"),
        _ => Err(TmuxError::InvalidRequest(format!(
            "unsupported interactive key {key:?}"
        ))),
    }
}

/// Parse `"zellij 0.46.0"` (or similar) into `(major, minor)`.
pub fn parse_zellij_version(stdout: &str) -> Option<(u32, u32)> {
    let token = stdout
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor_raw = parts.next()?;
    let minor_digits: String = minor_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let minor: u32 = minor_digits.parse().ok()?;
    Some((major, minor))
}

/// Parse `zellij list-sessions -n -s` output into active (non-EXITED) session names.
pub fn parse_zellij_sessions(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("EXITED"))
        .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZellijTabInfo {
    pub tab_id: u32,
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub is_sync_panes_active: bool,
}

#[derive(Debug, Deserialize)]
struct ZellijCommandText {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct ZellijRunningCommand {
    #[serde(default)]
    command: Option<ZellijCommandText>,
}

#[derive(Debug, Deserialize)]
struct ZellijCommandState {
    #[serde(default)]
    running: Option<ZellijRunningCommand>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawZellijPane {
    id: u32,
    #[serde(default)]
    is_plugin: bool,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    exited: bool,
    #[serde(default)]
    pane_content_rows: u32,
    #[serde(default)]
    cursor_coordinates_in_pane: Option<[u32; 2]>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    pane_command: Option<String>,
    #[serde(default)]
    pane_cwd: Option<String>,
    #[serde(default)]
    command_state: Option<ZellijCommandState>,
    #[serde(default)]
    tab_id: u32,
    #[serde(default)]
    tab_position: u32,
    #[serde(default)]
    tab_name: String,
}

pub fn parse_zellij_tabs_json(raw: &str) -> Result<HashMap<u32, ZellijTabInfo>, TmuxError> {
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let tabs: Vec<ZellijTabInfo> = serde_json::from_str(raw)
        .map_err(|e| TmuxError::Parse(format!("zellij tabs json: {e}")))?;
    Ok(tabs.into_iter().map(|t| (t.tab_id, t)).collect())
}

pub fn parse_zellij_panes_json(
    session_name: &str,
    raw: &str,
    tabs: &HashMap<u32, ZellijTabInfo>,
    session_attached: u32,
) -> Result<Vec<PaneInfo>, TmuxError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<RawZellijPane> = serde_json::from_str(raw)
        .map_err(|e| TmuxError::Parse(format!("zellij panes json: {e}")))?;
    let mut out = Vec::new();
    let mut per_tab_idx: HashMap<u32, u32> = HashMap::new();

    for p in items {
        if p.is_plugin {
            continue;
        }
        let tab = tabs.get(&p.tab_id);
        let pane_idx = {
            let slot = per_tab_idx.entry(p.tab_id).or_insert(0);
            let cur = *slot;
            *slot += 1;
            cur
        };
        let current_command = p
            .pane_command
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                p.command_state
                    .as_ref()
                    .and_then(|cs| cs.running.as_ref())
                    .and_then(|r| r.command.as_ref())
                    .map(|c| c.text.clone())
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| p.terminal_command.filter(|s| !s.trim().is_empty()))
            .unwrap_or_default();
        let current_path = p
            .pane_cwd
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                p.command_state
                    .as_ref()
                    .and_then(|cs| cs.cwd.clone())
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_default();
        let (cursor_x, cursor_y) = p
            .cursor_coordinates_in_pane
            .map(|[x, y]| (x, y))
            .unwrap_or((0, 0));
        let window_name = if !p.tab_name.is_empty() {
            p.tab_name
        } else {
            tab.map(|t| t.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("tab-{}", p.tab_position))
        };

        out.push(PaneInfo {
            pane_id: PaneId::new(format!("zj:{session_name}/{}", p.id)),
            session_name: session_name.to_string(),
            window_id: format!("tab_{}", p.tab_id),
            window_index: p.tab_position,
            pane_index: pane_idx,
            pane_pid: 0,
            current_command,
            current_path,
            pane_active: p.is_focused,
            window_active: tab.is_some_and(|t| t.active) || p.is_focused,
            session_attached,
            in_mode: false,
            dead: p.exited,
            synchronized: tab.is_some_and(|t| t.is_sync_panes_active),
            alternate_on: false,
            cursor_x,
            cursor_y,
            window_name,
            session_group: String::new(),
            history_size: 0,
            pane_height: p.pane_content_rows,
        });
    }
    Ok(out)
}

/// Extract a map of `"terminal_<id>"` -> `"tab_<tab_id>"` from `list-panes --all --json`.
pub fn extract_zellij_pane_tab_map(raw: &str) -> Result<HashMap<String, String>, TmuxError> {
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let items: Vec<RawZellijPane> = serde_json::from_str(raw)
        .map_err(|e| TmuxError::Parse(format!("zellij panes json: {e}")))?;
    let mut map = HashMap::with_capacity(items.len());
    for p in items {
        let prefix = if p.is_plugin { "plugin" } else { "terminal" };
        map.insert(format!("{prefix}_{}", p.id), format!("tab_{}", p.tab_id));
    }
    Ok(map)
}

/// Parse `zellij -s <sess> action list-clients` tabular output.
/// When `pane_to_tab` is supplied, maps the client's focused `ZELLIJ_PANE_ID` (e.g. `terminal_0`)
/// into `tab_<tab_id>` so `ClientInfo.window_id` matches `PaneInfo.window_id`.
pub fn parse_zellij_clients_text(
    session_name: &str,
    raw: &str,
    now_s: u64,
    pane_to_tab: Option<&HashMap<String, String>>,
) -> Vec<ClientInfo> {
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || (idx == 0 && trimmed.starts_with("CLIENT_ID")) {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(client_id) = parts.next() else {
            continue;
        };
        let pane_token = parts.next().unwrap_or("");
        let window_id = pane_to_tab
            .and_then(|m| m.get(pane_token))
            .cloned()
            .unwrap_or_else(|| pane_token.to_string());
        out.push(ClientInfo {
            tty: format!("zj-client:{session_name}:{client_id}"),
            session_name: session_name.to_string(),
            activity_s: now_s,
            flags: "attached,focused".to_string(),
            session_group: String::new(),
            window_id,
        });
    }
    out
}

/// Slice a full or viewport Zellij `dump-screen` capture according to `CaptureRequest`.
pub fn slice_zellij_dump(pane_id: &PaneId, raw: &str, req: &CaptureRequest) -> PanePreview {
    let mut all_lines: Vec<&str> = raw.lines().map(|l| l.trim_end_matches('\r')).collect();
    while all_lines.last().is_some_and(|l| l.trim().is_empty()) {
        all_lines.pop();
    }
    let total_lines = all_lines.len();
    let max_bytes = if req.all || req.before_line.is_some() {
        MAX_FULL_CAPTURE_BYTES
    } else {
        MAX_CAPTURE_BYTES
    };
    let max_lines = if req.all {
        MAX_FULL_CAPTURE_LINES
    } else if req.before_line.is_some() {
        req.lines.clamp(1, MAX_FULL_CAPTURE_LINES)
    } else {
        req.lines.clamp(1, MAX_CAPTURE_LINES)
    };

    let end_idx = match req.before_line {
        Some(before) => before.min(total_lines),
        None => total_lines,
    };
    let start_idx = if req.all && req.before_line.is_none() {
        total_lines.saturating_sub(max_lines)
    } else {
        end_idx.saturating_sub(max_lines)
    };
    let mut truncated = req.all && total_lines > max_lines;
    let joined = all_lines[start_idx..end_idx].join("\n");
    let (text, dropped_top, byte_truncated) = cap_page_bytes(&joined, max_lines, max_bytes, false);
    truncated |= byte_truncated;

    let returned_lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    let effective_start = start_idx.saturating_add(dropped_top);
    let has_more = effective_start > 0;
    PanePreview {
        pane_id: pane_id.clone(),
        lines: returned_lines,
        text,
        in_mode: false,
        dead: false,
        total_lines,
        start_line: effective_start,
        has_more,
        next_before_line: has_more.then_some(effective_start),
        truncated,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build arguments for `zellij action new-pane --in-place --close-replaced-pane`.
pub fn build_zellij_replace_pane_args(
    old_numeric_id: &str,
    name: Option<&str>,
    cwd: Option<&str>,
    start_suspended: bool,
    resume_argv: &[String],
) -> Result<Vec<String>, TmuxError> {
    if resume_argv.is_empty() || resume_argv[0].trim().is_empty() {
        return Err(TmuxError::InvalidRequest("resume_argv is empty".into()));
    }
    let numeric = old_numeric_id
        .trim()
        .strip_prefix("terminal_")
        .unwrap_or(old_numeric_id.trim());
    if numeric.is_empty() || !numeric.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TmuxError::InvalidRequest(format!(
            "invalid zellij numeric pane id {old_numeric_id:?}"
        )));
    }
    let mut args = vec![
        "new-pane".to_string(),
        "--in-place".to_string(),
        "--close-replaced-pane".to_string(),
        "--pane-id".to_string(),
        format!("terminal_{numeric}"),
    ];
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--name".to_string());
        args.push(n.to_string());
    }
    if let Some(dir) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--cwd".to_string());
        args.push(dir.to_string());
    }
    if start_suspended {
        args.push("--start-suspended".to_string());
    }
    args.push("--".to_string());
    args.extend(resume_argv.iter().cloned());
    Ok(args)
}

fn parse_zellij_terminal_ids(raw: &str) -> std::collections::HashSet<u32> {
    serde_json::from_str::<Vec<RawZellijPane>>(raw)
        .map(|items| {
            items
                .into_iter()
                .filter(|p| !p.is_plugin)
                .map(|p| p.id)
                .collect()
        })
        .unwrap_or_default()
}

fn pick_replacement_zellij_terminal_id(
    raw: &str,
    before_ids: &std::collections::HashSet<u32>,
    expected_title: Option<&str>,
) -> Option<u32> {
    let items: Vec<RawZellijPane> = serde_json::from_str(raw).ok()?;
    items
        .into_iter()
        .filter(|p| !p.is_plugin && !before_ids.contains(&p.id))
        .max_by_key(|p| {
            (
                p.pane_command.is_none(),
                expected_title.is_some_and(|t| !t.is_empty() && p.title == t),
                p.id,
            )
        })
        .map(|p| p.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use starfane_proto::AgentKind;

    #[test]
    fn parses_zellij_version_and_sessions() {
        assert_eq!(parse_zellij_version("zellij 0.46.0\n"), Some((0, 46)));
        assert_eq!(parse_zellij_version("zellij 0.44.1-rc1"), Some((0, 44)));
        assert_eq!(parse_zellij_version("zellij 0.43.1"), Some((0, 43)));
        assert!(parse_zellij_version("zellij 0.46.0").unwrap() >= MIN_ZELLIJ_VERSION);
        assert!(parse_zellij_version("zellij 0.43.9").unwrap() < MIN_ZELLIJ_VERSION);

        let sessions = parse_zellij_sessions(
            "work [Created 2m ago]\nold-sess [Created 1h ago] (EXITED - attach to resurrect)\ndev [Created 5s ago]\n",
        );
        assert_eq!(sessions, vec!["work", "dev"]);
    }

    #[test]
    fn zellij_clients_map_terminal_id_to_tab_window_for_watching_gate() {
        let tabs_json = r#"[
          {"tab_id": 0, "position": 0, "name": "Tab #1", "active": true, "is_sync_panes_active": false},
          {"tab_id": 1, "position": 1, "name": "Tab #2", "active": false, "is_sync_panes_active": false}
        ]"#;
        let panes_json = r#"[
          {"id": 0, "is_plugin": false, "is_focused": true, "title": "p0", "exited": false, "pane_content_rows": 40, "pane_command": "claude", "pane_cwd": "/tmp", "tab_id": 0, "tab_position": 0, "tab_name": "Tab #1"},
          {"id": 1, "is_plugin": false, "is_focused": true, "title": "p1", "exited": false, "pane_content_rows": 40, "pane_command": "codex", "pane_cwd": "/tmp", "tab_id": 1, "tab_position": 1, "tab_name": "Tab #2"}
        ]"#;
        let tabs = parse_zellij_tabs_json(tabs_json).unwrap();
        let panes = parse_zellij_panes_json("work", panes_json, &tabs, 1).unwrap();
        let pane_tab_map = extract_zellij_pane_tab_map(panes_json).unwrap();
        let clients_txt = "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n1 terminal_0 claude\n";
        let clients = parse_zellij_clients_text("work", clients_txt, 1000, Some(&pane_tab_map));
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].window_id, "tab_0");
        assert!(crate::is_user_watching(&panes[0], &clients, 1002, 30));
        assert!(!crate::is_user_watching(&panes[1], &clients, 1002, 30));
    }

    #[test]
    fn parses_real_zellij_046_panes_and_tabs_json_and_classifies_agents() {
        let tabs_json = r#"[
          {"tab_id": 0, "position": 0, "name": "Tab #1", "active": true, "is_sync_panes_active": false},
          {"tab_id": 1, "position": 1, "name": "Review", "active": false, "is_sync_panes_active": true}
        ]"#;
        let panes_json = r#"[
          {"id": 2, "is_plugin": true, "is_focused": false, "title": "tab-bar", "exited": false, "tab_id": 0, "tab_position": 0, "tab_name": "Tab #1"},
          {"id": 0, "is_plugin": false, "is_focused": true, "title": "Pane #1", "exited": false, "pane_content_rows": 47, "cursor_coordinates_in_pane": [4, 12], "pane_command": "claude --model opus", "pane_cwd": "/tmp/repo", "tab_id": 0, "tab_position": 0, "tab_name": "Tab #1"},
          {"id": 1, "is_plugin": false, "is_focused": false, "title": "shell", "exited": false, "pane_content_rows": 47, "cursor_coordinates_in_pane": [0, 2], "pane_command": "/bin/zsh", "pane_cwd": "/tmp/repo", "tab_id": 1, "tab_position": 1, "tab_name": "Review"}
        ]"#;
        let tabs = parse_zellij_tabs_json(tabs_json).unwrap();
        let panes = parse_zellij_panes_json("work", panes_json, &tabs, 1).unwrap();
        assert_eq!(panes.len(), 2, "plugin panes must be filtered out");
        assert_eq!(panes[0].pane_id.as_str(), "zj:work/0");
        assert_eq!(panes[0].cursor_x, 4);
        assert_eq!(panes[0].cursor_y, 12);
        assert!(panes[0].window_active);
        assert!(!panes[0].synchronized);

        assert_eq!(panes[1].pane_id.as_str(), "zj:work/1");
        assert!(panes[1].synchronized);
        assert_eq!(panes[1].window_name, "Review");

        let discovered = classify_panes_with_git_lookup(
            &panes,
            "",
            DiscoverOptions {
                include_shell_panes: true,
            },
            false,
        );
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0].agent, AgentKind::Claude);
        assert_eq!(discovered[0].model.as_deref(), Some("opus"));
        assert_eq!(discovered[1].agent, AgentKind::Shell);
    }

    #[test]
    fn slices_zellij_scrollback_with_cursor_pagination() {
        let dump: String = (0..100).map(|i| format!("line-{i:03}\n")).collect();
        let pid = PaneId::new("zj:work/1");

        let tail = slice_zellij_dump(&pid, &dump, &CaptureRequest::tail(20));
        assert_eq!(tail.total_lines, 100);
        assert_eq!(tail.start_line, 80);
        assert_eq!(tail.lines, 20);
        assert!(tail.has_more);
        assert_eq!(tail.next_before_line, Some(80));
        assert!(tail.text.starts_with("line-080\n"));
        assert!(tail.text.ends_with("line-099"));

        let prev = slice_zellij_dump(&pid, &dump, &CaptureRequest::page_before(80, 30));
        assert_eq!(prev.start_line, 50);
        assert_eq!(prev.lines, 30);
        assert_eq!(prev.next_before_line, Some(50));
        assert!(prev.text.starts_with("line-050\n"));
        assert!(prev.text.ends_with("line-079"));

        let full = slice_zellij_dump(&pid, &dump, &CaptureRequest::full());
        assert_eq!(full.start_line, 0);
        assert_eq!(full.lines, 100);
        assert!(!full.has_more);
        assert_eq!(full.next_before_line, None);
    }

    #[test]
    fn paste_passes_double_dash_separator_for_leading_dash_text() {
        use crate::exec::ExecOutput;
        use std::path::Path;
        use std::sync::Mutex;

        #[derive(Debug, Default)]
        struct CaptureExec {
            calls: Mutex<Vec<Vec<String>>>,
        }
        impl ExecTransport for CaptureExec {
            fn run(
                &self,
                _bin: &Path,
                args: &[String],
                _stdin: Option<&[u8]>,
            ) -> Result<ExecOutput, TmuxError> {
                self.calls.lock().unwrap().push(args.to_vec());
                Ok(ExecOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    success: true,
                    code: Some(0),
                })
            }
            fn run_shell(&self, _script: &str) -> Result<ExecOutput, TmuxError> {
                unreachable!()
            }
            fn run_shell_bytes(
                &self,
                _script: &str,
                _stdin: Option<&[u8]>,
                _timeout: std::time::Duration,
            ) -> Result<crate::exec::ExecBytesOutput, TmuxError> {
                unreachable!()
            }
            fn is_local(&self) -> bool {
                true
            }
        }

        let exec = Arc::new(CaptureExec::default());
        let zj = ZellijMux::with_transport(PathBuf::from("zellij"), exec.clone());
        let pane = PaneId::new("zj:work/7");
        zj.paste_interactive_text(&pane, "--help -la").unwrap();
        zj.paste_pointer_text(&pane, "- [ ] check", &GateConfig::default())
            .unwrap();

        let calls = exec.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            vec![
                "-s",
                "work",
                "action",
                "paste",
                "-p",
                "terminal_7",
                "--",
                "--help -la"
            ]
        );
        assert_eq!(
            calls[1],
            vec![
                "-s",
                "work",
                "action",
                "paste",
                "-p",
                "terminal_7",
                "--",
                "- [ ] check"
            ]
        );
    }

    #[test]
    fn kill_pane_targets_explicit_pane_id_and_handles_missing_flag_or_gone_pane() {
        use crate::exec::ExecOutput;
        use std::path::Path;
        use std::sync::Mutex;

        #[derive(Debug)]
        struct StubExec {
            calls: Mutex<Vec<Vec<String>>>,
            reply: ExecOutput,
        }
        impl ExecTransport for StubExec {
            fn run(
                &self,
                _bin: &Path,
                args: &[String],
                _stdin: Option<&[u8]>,
            ) -> Result<ExecOutput, TmuxError> {
                self.calls.lock().unwrap().push(args.to_vec());
                Ok(self.reply.clone())
            }
            fn run_shell(&self, _script: &str) -> Result<ExecOutput, TmuxError> {
                unreachable!()
            }
            fn run_shell_bytes(
                &self,
                _script: &str,
                _stdin: Option<&[u8]>,
                _timeout: std::time::Duration,
            ) -> Result<crate::exec::ExecBytesOutput, TmuxError> {
                unreachable!()
            }
            fn is_local(&self) -> bool {
                true
            }
        }

        // 1. Happy path passes `--pane-id terminal_<id>` explicitly.
        let ok_exec = Arc::new(StubExec {
            calls: Mutex::new(Vec::new()),
            reply: ExecOutput {
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                code: Some(0),
            },
        });
        let zj = ZellijMux::with_transport(PathBuf::from("zellij"), ok_exec.clone());
        zj.kill_pane(&PaneId::new("zj:work/11")).unwrap();
        assert_eq!(
            *ok_exec.calls.lock().unwrap(),
            vec![vec![
                "-s",
                "work",
                "action",
                "close-pane",
                "--pane-id",
                "terminal_11",
            ]]
        );

        // 2. Rejects invalid pane id without executing zellij.
        assert!(matches!(
            zj.kill_pane(&PaneId::new("%11")),
            Err(TmuxError::InvalidRequest(_))
        ));
        assert_eq!(ok_exec.calls.lock().unwrap().len(), 1);

        // 3. Rejects unsupported `--pane-id` cleanly with descriptive InvalidRequest error.
        let old_exec = Arc::new(StubExec {
            calls: Mutex::new(Vec::new()),
            reply: ExecOutput {
                stdout: String::new(),
                stderr: "error: unexpected argument '--pane-id' found".into(),
                success: false,
                code: Some(2),
            },
        });
        let zj_old = ZellijMux::with_transport(PathBuf::from("zellij"), old_exec);
        let err = zj_old.kill_pane(&PaneId::new("zj:work/11")).unwrap_err();
        assert!(matches!(err, TmuxError::InvalidRequest(_)), "{err}");
        assert!(err.to_string().contains("--pane-id"), "{err}");

        // 4. Treats already-gone pane idempotently.
        let gone_exec = Arc::new(StubExec {
            calls: Mutex::new(Vec::new()),
            reply: ExecOutput {
                stdout: String::new(),
                stderr: "pane terminal_11 not found".into(),
                success: false,
                code: Some(1),
            },
        });
        let zj_gone = ZellijMux::with_transport(PathBuf::from("zellij"), gone_exec);
        zj_gone.kill_pane(&PaneId::new("zj:work/11")).unwrap();
    }

    #[test]
    fn send_key_validates_allowlist_and_maps_keys_to_targeted_zellij_send_keys() {
        use crate::exec::ExecOutput;
        use std::path::Path;
        use std::sync::Mutex;

        #[derive(Debug, Default)]
        struct CaptureExec {
            calls: Mutex<Vec<Vec<String>>>,
        }
        impl ExecTransport for CaptureExec {
            fn run(
                &self,
                _bin: &Path,
                args: &[String],
                _stdin: Option<&[u8]>,
            ) -> Result<ExecOutput, TmuxError> {
                self.calls.lock().unwrap().push(args.to_vec());
                Ok(ExecOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    success: true,
                    code: Some(0),
                })
            }
            fn run_shell(&self, _script: &str) -> Result<ExecOutput, TmuxError> {
                unreachable!()
            }
            fn run_shell_bytes(
                &self,
                _script: &str,
                _stdin: Option<&[u8]>,
                _timeout: std::time::Duration,
            ) -> Result<crate::exec::ExecBytesOutput, TmuxError> {
                unreachable!()
            }
            fn is_local(&self) -> bool {
                true
            }
        }

        let exec = Arc::new(CaptureExec::default());
        let zj = ZellijMux::with_transport(PathBuf::from("zellij"), exec.clone());
        let pane = PaneId::new("zj:work/9");

        zj.send_key(&pane, "Down").unwrap();
        zj.send_key(&pane, "Enter").unwrap();
        zj.send_key(&pane, "Escape").unwrap();
        zj.send_key(&pane, "C-c").unwrap();
        zj.send_key(&pane, "1").unwrap();

        assert!(matches!(
            zj.send_key(&pane, "Enter; rm -rf /"),
            Err(TmuxError::InvalidRequest(_))
        ));
        assert!(matches!(
            zj.send_key(&PaneId::new("%9"), "Enter"),
            Err(TmuxError::InvalidRequest(_))
        ));

        let calls = exec.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(
            calls[0],
            vec![
                "-s",
                "work",
                "action",
                "send-keys",
                "-p",
                "terminal_9",
                "Down"
            ]
        );
        assert_eq!(
            calls[1],
            vec![
                "-s",
                "work",
                "action",
                "send-keys",
                "-p",
                "terminal_9",
                "Enter"
            ]
        );
        assert_eq!(
            calls[2],
            vec![
                "-s",
                "work",
                "action",
                "send-keys",
                "-p",
                "terminal_9",
                "Esc"
            ]
        );
        assert_eq!(
            calls[3],
            vec![
                "-s",
                "work",
                "action",
                "send-keys",
                "-p",
                "terminal_9",
                "Ctrl c"
            ]
        );
        assert_eq!(
            calls[4],
            vec!["-s", "work", "action", "send-keys", "-p", "terminal_9", "1"]
        );
        assert_eq!(map_interactive_key_for_zellij("5").unwrap(), "5");
        assert_eq!(map_interactive_key_for_zellij("9").unwrap(), "9");
        assert_eq!(
            map_interactive_key_for_zellij("Backspace").unwrap(),
            "Backspace"
        );
    }

    #[test]
    fn suspend_and_wake_pane_use_start_suspended_and_targeted_enter() {
        use crate::exec::ExecOutput;
        use std::path::Path;
        use std::sync::Mutex;

        #[derive(Debug)]
        struct SeqExec {
            calls: Mutex<Vec<Vec<String>>>,
            list_count: Mutex<usize>,
        }
        impl ExecTransport for SeqExec {
            fn run(
                &self,
                _bin: &Path,
                args: &[String],
                _stdin: Option<&[u8]>,
            ) -> Result<ExecOutput, TmuxError> {
                self.calls.lock().unwrap().push(args.to_vec());
                let action = args.get(3).map(String::as_str).unwrap_or("");
                let stdout = if action == "list-panes" {
                    let mut c = self.list_count.lock().unwrap();
                    *c += 1;
                    if *c == 1 {
                        r#"[{"id":2,"is_plugin":true,"title":"tab-bar"},{"id":4,"is_plugin":false,"title":"claude-1","pane_command":"claude"}]"#.to_string()
                    } else {
                        r#"[{"id":2,"is_plugin":true,"title":"tab-bar"},{"id":11,"is_plugin":false,"title":"claude-1","pane_command":null}]"#.to_string()
                    }
                } else {
                    String::new()
                };
                Ok(ExecOutput {
                    stdout,
                    stderr: String::new(),
                    success: true,
                    code: Some(0),
                })
            }
            fn run_shell(&self, _script: &str) -> Result<ExecOutput, TmuxError> {
                unreachable!()
            }
            fn run_shell_bytes(
                &self,
                _script: &str,
                _stdin: Option<&[u8]>,
                _timeout: std::time::Duration,
            ) -> Result<crate::exec::ExecBytesOutput, TmuxError> {
                unreachable!()
            }
            fn is_local(&self) -> bool {
                true
            }
        }

        let exec = Arc::new(SeqExec {
            calls: Mutex::new(Vec::new()),
            list_count: Mutex::new(0),
        });
        let zj = ZellijMux::with_transport(PathBuf::from("zellij"), exec.clone());
        let new_pane = zj
            .suspend_pane(&SuspendPaneReq {
                pane: PaneId::new("zj:work/4"),
                cwd: Some("/repo".into()),
                pane_name: Some("claude-1".into()),
                waiter_script: String::new(),
                resume_argv: vec!["claude".into(), "--resume".into(), "id-1".into()],
            })
            .unwrap();
        assert_eq!(new_pane, PaneId::new("zj:work/11"));

        let woke = zj
            .wake_suspended_pane(&WakePaneReq {
                pane: new_pane.clone(),
                cwd: Some("/repo".into()),
                pane_name: Some("claude-1".into()),
                resume_argv: vec!["claude".into(), "--resume".into(), "id-1".into()],
            })
            .unwrap();
        assert_eq!(woke, new_pane);

        let calls = exec.calls.lock().unwrap();
        assert_eq!(
            calls[1],
            vec![
                "-s",
                "work",
                "action",
                "new-pane",
                "--in-place",
                "--close-replaced-pane",
                "--pane-id",
                "terminal_4",
                "--name",
                "claude-1",
                "--cwd",
                "/repo",
                "--start-suspended",
                "--",
                "claude",
                "--resume",
                "id-1",
            ]
        );
        assert_eq!(
            calls[3],
            vec![
                "-s",
                "work",
                "action",
                "send-keys",
                "-p",
                "terminal_11",
                "Enter",
            ]
        );
    }

    #[test]
    #[ignore = "needs zellij >= 0.44; run with `cargo test -p starfane-tmux -- --include-ignored`"]
    fn sandbox_zellij_list_spawn_paste_dump_and_focus() {
        let zj = ZellijMux::new(PathBuf::from("zellij"));
        if !zj.available() {
            return;
        }
        let sess = format!("sf-zj-test-{}", std::process::id());
        let _ = zj.delete_session(&sess);

        let spawned = zj
            .spawn_window(&SpawnWindow {
                target_session: Some(sess.clone()),
                create_session: true,
                window_name: "sandbox-pane".into(),
                cwd: std::env::temp_dir(),
                env: vec![("STARFANE_TEST_ENV".into(), "ok123".into())],
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo ZJ_BOOT_$STARFANE_TEST_ENV; cat".into(),
                ],
            })
            .expect("spawn_window in zellij");
        assert!(spawned.as_str().starts_with(&format!("zj:{sess}/")));

        thread::sleep(Duration::from_millis(300));
        let panes = zj.list_panes().expect("list_panes");
        assert!(
            panes.iter().any(|p| p.pane_id == spawned),
            "spawned pane {spawned} not found in {panes:?}"
        );

        zj.paste_interactive_text(&spawned, "ZJ_PASTE_MARKER")
            .expect("paste_interactive_text");
        zj.submit_enter(&spawned).expect("submit_enter");
        // Leading-dash text must be passed after `--` so Zellij's clap parser does not reject it as a flag.
        zj.paste_interactive_text(&spawned, "--zj-dash-flag -la")
            .expect("paste_interactive_text with leading dash");
        zj.submit_enter(&spawned).expect("submit_enter after dash");
        thread::sleep(Duration::from_millis(250));

        let cap = zj
            .capture_pane(&spawned, &CaptureRequest::full())
            .expect("capture_pane");
        assert!(
            cap.text.contains("ZJ_BOOT_ok123"),
            "missing env echo in dump: {:?}",
            cap.text
        );
        assert!(
            cap.text.contains("ZJ_PASTE_MARKER"),
            "missing pasted text in dump: {:?}",
            cap.text
        );
        assert!(
            cap.text.contains("--zj-dash-flag -la"),
            "missing leading-dash pasted text in dump: {:?}",
            cap.text
        );

        let focus = zj.focus_pane(&spawned).expect("focus_pane");
        assert_eq!(focus.session_name, sess);

        let layout = zj.dump_layout(&sess).expect("dump_layout");
        assert!(!layout.trim().is_empty());

        let _ = zj.delete_session(&sess);
    }
}
