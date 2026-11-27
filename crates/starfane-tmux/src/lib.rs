//! tmux inspection, focus detection, 7-point injection gate, L4 interactive spawn,
//! and FD-isolated persistent SSH conduits.
//!
//! Pure decision logic ([`check_injection_gate`], [`is_user_watching`], [`resolve_pane`],
//! [`validate_pointer_text`]) is separated from process I/O so it can be matrix-tested without
//! a tmux server.

mod backup;
mod conduit;
mod discover;
mod error;
mod exec;
mod gate;
mod model;
mod mux;
mod resolve;
mod zellij;

pub use backup::{
    build_resume_command, build_snapshot, list_snapshots, load_latest_snapshot, load_snapshot_file,
    save_snapshot_to_dir, shell_single_quote, validate_resume_id, BackupSaveOutcome, PaneAgentHint,
    RestoreOptions, RestoreReport, ResumedAgentInfo, SavedPane, SavedSession, SavedWindow,
    SnapshotSummary, TmuxBackupSnapshot, BACKUP_VERSION,
};
pub use conduit::{
    ConduitLane, ConduitMeta, PersistentSshConduit, RemoteStatMode, SshConduitPool,
    CONDUIT_FALLBACK_COOLDOWN, CONDUIT_HANDSHAKE_FAILURE_THRESHOLD,
};
pub use discover::{
    cap_page_bytes, capture_pane_tail, classify_panes, classify_panes_with,
    classify_panes_with_git_lookup, discover_agent_panes, trim_and_cap_tail, CaptureRequest,
    DiscoverOptions, DiscoveredPane, PanePreview, MAX_CAPTURE_BYTES, MAX_CAPTURE_LINES,
    MAX_FULL_CAPTURE_BYTES, MAX_FULL_CAPTURE_LINES,
};
pub use error::TmuxError;
pub use exec::{
    ssh_universal_quote, ExecBytesOutput, ExecOutput, ExecTransport, LocalExec, SshExec,
    DEFAULT_SSH_CONNECT_TIMEOUT_SECS, DEFAULT_SSH_TIMEOUT, SSH_BIN_ENV,
};
pub use gate::{
    adaptive_paste_settle_ms, check_injection_gate, check_injection_gate_with_screen,
    check_interactive_gate, detect_blocking_startup_prompt, detect_codex_update_prompt,
    detect_folder_trust_prompt, is_bare_slash_command, is_paste_stuck_in_prompt, is_user_watching,
    validate_interactive_text, validate_pointer_text, GateConfig,
};
pub use model::{
    parse_client_line, parse_clients, parse_pane_line, parse_panes, parse_window_line,
    parse_windows, split_tmux_fields, ClientInfo, PaneInfo, PaneStatus, WindowInfo, FIELD_SEP,
    PANE_FORMAT, TMUX_FIELD_SEP,
};
pub use mux::{
    is_tmux_pane_id, pane_mux_kind, parse_zellij_pane_id, qualify_host_pane, qualify_host_session,
    split_host_prefix, validate_interactive_key, GateProbe, MuxCapabilities, MuxKind,
    SuspendPaneReq, TerminalMux, WakePaneReq,
};
pub use resolve::{pid_ancestors, resolve_pane, PaneHint};
pub use zellij::{
    parse_zellij_clients_text, parse_zellij_panes_json, parse_zellij_sessions,
    parse_zellij_tabs_json, parse_zellij_version, slice_zellij_dump, ZellijMux, ZellijTabInfo,
    MIN_ZELLIJ_VERSION,
};

use starfane_proto::{PaneId, SessionState};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_DETACHED_SESSION: &str = "agent-session";

/// Pane user options written by [`Tmux::set_status`].
pub const OPT_STATE: &str = "@starfane_state";
pub const OPT_ROLE: &str = "@starfane_role";
pub const OPT_PIPELINE: &str = "@starfane_pipeline";
/// Mirror of the state for user status-line formats (`#{@agent_state}`).
pub const OPT_AGENT_STATE: &str = "@agent_state";

const BEL: &[u8] = b"\x07";
static BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Tmux {
    pub bin: PathBuf,
    /// `tmux -L <name>` socket (tests use `starfane-test…`); `None` = the user's default server.
    pub socket_name: Option<String>,
    pub(crate) transport: Arc<dyn ExecTransport>,
}

/// Outcome of focusing a tmux pane across single-client, multi-client, or detached servers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FocusOutcome {
    pub pane_id: PaneId,
    pub session_name: String,
    pub tmux_target: String,
    pub switched_client: Option<String>,
    pub already_in_session: bool,
}

#[derive(Debug, Clone)]
pub struct SpawnWindow {
    /// `None` → use (or create) [`DEFAULT_DETACHED_SESSION`] (`"agent-session"`).
    pub target_session: Option<String>,
    /// Create `target_session` automatically if it does not already exist (always `true` when `target_session` is `None`).
    pub create_session: bool,
    pub window_name: String,
    pub cwd: PathBuf,
    /// e.g. `("STARFANE_BIND", "B-…")`.
    pub env: Vec<(String, String)>,
    /// Program and args, executed directly (no shell). From `adapters::build_l4_spawn_cmd`.
    pub argv: Vec<String>,
}

fn s(v: &str) -> String {
    v.to_string()
}

fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_pane(p: &PaneId) -> Result<(), TmuxError> {
    let id = p.as_str();
    if id.len() > 1 && id.starts_with('%') && id[1..].chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        Err(TmuxError::InvalidRequest(format!(
            "pane id must look like %12, got {id:?}"
        )))
    }
}

fn stderr_means_pane_gone(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("can't find pane")
        || lower.contains("no such pane")
        || lower.contains("pane not found")
}

/// Unique-per-call paste buffer name: `starfane_<pid>_<nanos>_<seq>`.
fn new_buffer_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("starfane_{}_{nanos:x}_{seq}", std::process::id())
}

impl Tmux {
    pub fn new(bin: PathBuf, socket_name: Option<String>) -> Self {
        Tmux {
            bin,
            socket_name,
            transport: Arc::new(LocalExec::default()),
        }
    }

    pub fn with_transport(
        bin: PathBuf,
        socket_name: Option<String>,
        transport: Arc<dyn ExecTransport>,
    ) -> Self {
        Tmux {
            bin,
            socket_name,
            transport,
        }
    }

    pub fn transport(&self) -> &Arc<dyn ExecTransport> {
        &self.transport
    }

    fn base_args(&self) -> Vec<String> {
        match &self.socket_name {
            Some(n) => vec![s("-L"), n.clone()],
            None => Vec::new(),
        }
    }

    fn output(&self, args: &[String], stdin: Option<&[u8]>) -> Result<exec::Output, TmuxError> {
        let mut full = self.base_args();
        full.extend_from_slice(args);
        self.transport.run(&self.bin, &full, stdin)
    }

    /// Run a tmux subcommand and return stdout; non-zero exit becomes [`TmuxError::Command`].
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, TmuxError> {
        let owned: Vec<String> = args.iter().map(|a| s(a)).collect();
        self.run_owned(&owned, stdin)
    }

    fn run_owned(&self, args: &[String], stdin: Option<&[u8]>) -> Result<String, TmuxError> {
        let out = self.output(args, stdin)?;
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

    /// Is the tmux binary runnable (`tmux -V` succeeds)?
    pub fn available(&self) -> bool {
        self.transport
            .run(&self.bin, &[s("-V")], None)
            .map(|o| o.success)
            .unwrap_or(false)
    }

    /// All panes on the server. A missing tmux server is not an error: returns an empty list.
    pub fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        match self.run(&["list-panes", "-a", "-F", model::PANE_FORMAT], None) {
            Ok(out) => parse_panes(&out),
            Err(e) if e.is_no_server() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// All attached clients. A missing tmux server yields an empty list.
    pub fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError> {
        match self.run(&["list-clients", "-F", model::CLIENT_FORMAT], None) {
            Ok(out) => parse_clients(&out),
            Err(e) if e.is_no_server() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Names of all sessions on the server. A missing tmux server yields an empty list.
    pub fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        match self.run(&["list-sessions", "-F", "#{session_name}"], None) {
            Ok(out) => Ok(out
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()),
            Err(e) if e.is_no_server() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// All windows on the server with layout strings. A missing tmux server yields an empty list.
    pub fn list_windows(&self) -> Result<Vec<WindowInfo>, TmuxError> {
        match self.run(&["list-windows", "-a", "-F", model::WINDOW_FORMAT], None) {
            Ok(out) => parse_windows(&out),
            Err(e) if e.is_no_server() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Text of screen line `cursor_y` of the pane (trailing blanks are trimmed by tmux).
    pub fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError> {
        valid_pane(pane)?;
        let y = cursor_y.to_string();
        let out = self.run(
            &[
                "capture-pane",
                "-p",
                "-t",
                pane.as_str(),
                "-S",
                &y,
                "-E",
                &y,
            ],
            None,
        )?;
        Ok(out.trim_end_matches(['\n', '\r']).to_string())
    }

    /// Capture only the currently visible screen rows (`0..pane_height`) so row index `cursor_y`
    /// lines up 1-to-1 with `PaneInfo.cursor_y`.
    pub fn capture_visible_screen(&self, pane: &PaneId) -> Result<String, TmuxError> {
        valid_pane(pane)?;
        self.run(&["capture-pane", "-p", "-t", pane.as_str()], None)
    }

    /// Query `#{pane_mode}` for `pane` (empty when not in any tmux mode).
    pub fn pane_mode(&self, pane: &PaneId) -> Result<String, TmuxError> {
        valid_pane(pane)?;
        let out = self.run(
            &["display-message", "-p", "-t", pane.as_str(), "#{pane_mode}"],
            None,
        )?;
        Ok(out.trim().to_string())
    }

    /// Safely exit tmux scrollback `copy-mode` / `view-mode` on `pane` via `send-keys -X cancel`
    /// without forwarding any bytes to the underlying PTY application.
    /// Returns `Ok(true)` when the pane is confirmed out of mode (`pane_in_mode == 0`), or
    /// `Ok(false)` when the pane is in a non-cancellable interactive chooser (e.g. `tree-mode`)
    /// or remains in mode after cancel.
    pub fn exit_copy_mode(&self, pane: &PaneId) -> Result<bool, TmuxError> {
        valid_pane(pane)?;
        let state = self.run(
            &[
                "display-message",
                "-p",
                "-t",
                pane.as_str(),
                "#{pane_in_mode}\t#{pane_mode}",
            ],
            None,
        )?;
        let trimmed = state.trim_end_matches(['\n', '\r']);
        let parts = model::split_tmux_fields(trimmed);
        let in_mode_raw = parts.first().copied().unwrap_or(trimmed);
        let mode_raw = parts.get(1).copied().unwrap_or("");
        if in_mode_raw.trim() == "0" {
            return Ok(true);
        }
        let mode = mode_raw.trim();
        if !mode.is_empty() && !matches!(mode, "copy-mode" | "view-mode") {
            return Ok(false);
        }
        if let Err(e) = self.run(&["send-keys", "-t", pane.as_str(), "-X", "cancel"], None) {
            let lower = e.to_string().to_ascii_lowercase();
            if !lower.contains("not in a mode") {
                return Err(e);
            }
        }
        let after = self.run(
            &[
                "display-message",
                "-p",
                "-t",
                pane.as_str(),
                "#{pane_in_mode}",
            ],
            None,
        )?;
        Ok(after.trim() == "0")
    }

    /// Paste validated pointer `text` into `pane` using bracketed paste (`paste-buffer -d -p`)
    /// without blocking for the settle pause or sending Enter. Callers on async runtimes should
    /// pair this with `tokio::time::sleep(cfg.paste_settle_ms)` followed by [`Tmux::submit_enter`].
    pub fn paste_pointer_text(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        validate_pointer_text(text, cfg)?;
        self.paste_raw_bracketed(pane, text)
    }

    /// Paste validated interactive `text` into `pane` using bracketed paste (`paste-buffer -d -p`)
    /// without blocking for the settle pause or sending Enter.
    pub fn paste_interactive_text(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        validate_interactive_text(text)?;
        self.paste_raw_bracketed(pane, text)
    }

    /// Press Enter (`C-m`) in `pane` after an asynchronous paste-settle delay.
    pub fn submit_enter(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        self.run(&["send-keys", "-t", pane.as_str(), "C-m"], None)?;
        Ok(())
    }

    /// Send a single allowlisted interactive navigation/approval key to `pane`.
    /// Validates the pane id and key against [`validate_interactive_key`], verifies the pane
    /// exists and is alive, safely exits tmux `copy-mode`/`view-mode` if active, and delivers
    /// the key via `tmux send-keys`.
    pub fn send_key(&self, pane: &PaneId, key: &str) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        let tmux_key = validate_interactive_key(key)?;
        let state = self.run(
            &[
                "display-message",
                "-p",
                "-t",
                pane.as_str(),
                "#{pane_dead}\t#{pane_in_mode}\t#{pane_mode}",
            ],
            None,
        )?;
        let trimmed = state.trim_end_matches(['\n', '\r']);
        let parts = model::split_tmux_fields(trimmed);
        if parts.first().is_some_and(|v| v.trim() == "1") {
            return Err(TmuxError::Command {
                args: format!("send-keys -t {}", pane.as_str()),
                code: Some(1),
                stderr: format!("pane {} is dead", pane.as_str()),
            });
        }
        if parts.get(1).is_some_and(|v| v.trim() == "1") {
            let mode = parts.get(2).copied().unwrap_or("").trim();
            if !mode.is_empty() && !matches!(mode, "copy-mode" | "view-mode") {
                return Err(TmuxError::Command {
                    args: format!("send-keys -t {}", pane.as_str()),
                    code: Some(1),
                    stderr: format!("pane {} is in non-cancellable mode {mode:?}", pane.as_str()),
                });
            }
            if let Err(e) = self.run(&["send-keys", "-t", pane.as_str(), "-X", "cancel"], None) {
                let lower = e.to_string().to_ascii_lowercase();
                if !lower.contains("not in a mode") {
                    return Err(e);
                }
            }
            thread::sleep(Duration::from_millis(30));
            let after = self.run(
                &[
                    "display-message",
                    "-p",
                    "-t",
                    pane.as_str(),
                    "#{pane_in_mode}",
                ],
                None,
            )?;
            if after.trim() != "0" {
                return Err(TmuxError::Command {
                    args: format!("send-keys -t {}", pane.as_str()),
                    code: Some(1),
                    stderr: format!("pane {} remained in copy-mode after cancel", pane.as_str()),
                });
            }
        }
        if tmux_key.len() == 1 {
            self.run(&["send-keys", "-t", pane.as_str(), "-l", tmux_key], None)?;
        } else {
            self.run(&["send-keys", "-t", pane.as_str(), tmux_key], None)?;
        }
        Ok(())
    }

    fn paste_raw_bracketed(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError> {
        let buf = new_buffer_name();
        self.run(&["load-buffer", "-b", &buf, "-"], Some(text.as_bytes()))?;
        if let Err(e) = self.run(
            &["paste-buffer", "-d", "-p", "-b", &buf, "-t", pane.as_str()],
            None,
        ) {
            // paste-buffer -d only deletes on success; do not leak the buffer. The paste
            // error is the one worth reporting, so a cleanup failure is intentionally secondary.
            let _ = self.run(&["delete-buffer", "-b", &buf], None);
            return Err(e);
        }
        Ok(())
    }

    /// Paste `text` into `pane` as one bracketed paste, wait for the TUI's paste-burst
    /// detector to settle, then press Enter. **Does not run the gate**: callers must call
    /// [`check_injection_gate`] first. Never uses `send-keys -l`, `C-u` or `C-c`.
    pub fn inject_pointer(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        self.paste_pointer_text(pane, text, cfg)?;
        thread::sleep(Duration::from_millis(cfg.paste_settle_ms));
        self.submit_enter(pane)
    }

    /// Inject explicit interactive user input via bracketed paste (`paste-buffer -d -p`) +
    /// adaptive settle pause + `C-m`. Allows multi-line or slash text once validated by
    /// [`validate_interactive_text`]; callers must still enforce [`check_interactive_gate`].
    pub fn inject_interactive(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        self.paste_interactive_text(pane, text)?;
        let settle = adaptive_paste_settle_ms(cfg.paste_settle_ms, text);
        thread::sleep(Duration::from_millis(settle));
        self.submit_enter(pane)
    }

    /// Open a new window running `req.argv` (no shell) and return its pane id.
    pub fn spawn_window(&self, req: &SpawnWindow) -> Result<PaneId, TmuxError> {
        if req.argv.is_empty() || req.argv[0].is_empty() {
            return Err(TmuxError::InvalidRequest("argv is empty".into()));
        }
        if req.window_name.is_empty() {
            return Err(TmuxError::InvalidRequest("window_name is empty".into()));
        }
        for (k, _) in &req.env {
            if !valid_env_key(k) {
                return Err(TmuxError::InvalidRequest(format!("bad env key {k:?}")));
            }
        }
        let session = req
            .target_session
            .clone()
            .unwrap_or_else(|| DEFAULT_DETACHED_SESSION.to_string());
        if session.is_empty() || session.contains([':', '.']) {
            return Err(TmuxError::InvalidRequest(format!(
                "bad session name {session:?}"
            )));
        }
        let cwd = req.cwd.to_string_lossy().into_owned();
        let target = format!("={session}:");
        let may_create = req.create_session || req.target_session.is_none();
        let has = self.output(&[s("has-session"), s("-t"), format!("={session}")], None)?;
        if !has.success && !may_create {
            return Err(TmuxError::Command {
                args: format!("has-session -t ={session}"),
                code: has.code,
                stderr: has.stderr,
            });
        }

        let new_window_args = || {
            let mut args = vec![
                s("new-window"),
                s("-d"),
                s("-P"),
                s("-F"),
                s("#{pane_id}"),
                s("-n"),
                req.window_name.clone(),
                s("-c"),
                cwd.clone(),
            ];
            for (k, v) in &req.env {
                args.push(s("-e"));
                args.push(format!("{k}={v}"));
            }
            args.push(s("-t"));
            args.push(target.clone());
            args.push(s("--"));
            args.extend(req.argv.iter().cloned());
            args
        };

        let out = if has.success {
            self.run_owned(&new_window_args(), None)?
        } else {
            let mut new_sess_args = vec![
                s("new-session"),
                s("-d"),
                s("-P"),
                s("-F"),
                s("#{pane_id}"),
                s("-s"),
                session.clone(),
                s("-n"),
                req.window_name.clone(),
                s("-c"),
                cwd.clone(),
            ];
            for (k, v) in &req.env {
                new_sess_args.push(s("-e"));
                new_sess_args.push(format!("{k}={v}"));
            }
            new_sess_args.push(s("--"));
            new_sess_args.extend(req.argv.iter().cloned());
            match self.run_owned(&new_sess_args, None) {
                Ok(out) => out,
                // Lost a creation race with another spawner: append window to the existing session.
                Err(TmuxError::Command { ref stderr, .. })
                    if stderr.contains("duplicate session") =>
                {
                    self.run_owned(&new_window_args(), None)?
                }
                Err(e) => return Err(e),
            }
        };
        let pane = PaneId::new(out.trim());
        valid_pane(&pane).map_err(|_| TmuxError::Parse(format!("tmux spawn printed {out:?}")))?;

        // Keep a crashed agent's pane (and its error output) around for diagnosis.
        if let Err(e) = self.run(
            &[
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "remain-on-exit",
                "failed",
            ],
            None,
        ) {
            if let TmuxError::Command { stderr, .. } = &e {
                let l = stderr.to_ascii_lowercase();
                if l.contains("no such pane") || l.contains("can't find pane") {
                    // The command died before we could pin the pane: the spawn failed.
                    return Err(TmuxError::Command {
                        args: format!("new-window -- {}", req.argv.join(" ")),
                        code: None,
                        stderr: format!("command exited immediately; pane {pane} is gone"),
                    });
                }
            }
            // Older tmux (< 3.3) lacks the `failed` value; the window is already running,
            // so this must not discard the pane id.
            eprintln!("starfane-tmux: remain-on-exit failed not applied to {pane}: {e}");
        }
        Ok(pane)
    }

    /// Publish agent state on the pane as user options (`@starfane_state`, `@starfane_role`,
    /// `@starfane_pipeline`, plus `@agent_state` for status-line formats). `None` fields are unset.
    pub fn set_status(&self, pane: &PaneId, st: &PaneStatus) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        let state = state_name(st.state);
        self.set_opt(pane, OPT_STATE, Some(state))?;
        self.set_opt(pane, OPT_AGENT_STATE, Some(state))?;
        let role = st.role.map(|r| r.as_str().to_string());
        self.set_opt(pane, OPT_ROLE, role.as_deref())?;
        let pl = st.pipeline.as_ref().map(|p| p.as_str().to_string());
        self.set_opt(pane, OPT_PIPELINE, pl.as_deref())
    }

    /// Remove every option written by [`Tmux::set_status`].
    pub fn clear_status(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        for opt in [OPT_STATE, OPT_AGENT_STATE, OPT_ROLE, OPT_PIPELINE] {
            self.set_opt(pane, opt, None)?;
        }
        Ok(())
    }

    fn set_opt(&self, pane: &PaneId, opt: &str, val: Option<&str>) -> Result<(), TmuxError> {
        match val {
            Some(v) => self.run(&["set-option", "-p", "-t", pane.as_str(), opt, v], None),
            None => self.run(&["set-option", "-p", "-u", "-t", pane.as_str(), opt], None),
        }
        .map(|_| ())
    }

    /// Ring the terminal bell for the pane (marks its window with the bell flag) by writing
    /// BEL to the pane's tty. No-op on remote SSH transports.
    pub fn ring_bell(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        if !self.transport.is_local() {
            return Ok(());
        }
        let tty = self.run(
            &["display-message", "-p", "-t", pane.as_str(), "#{pane_tty}"],
            None,
        )?;
        let tty = tty.trim();
        if tty.is_empty() {
            return Err(TmuxError::Parse("pane has no tty".into()));
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(tty)
            .and_then(|mut f| std::io::Write::write_all(&mut f, BEL))
            .map_err(|source| TmuxError::Io {
                what: format!("write bell to {tty}"),
                source,
            })
    }

    /// Rank a tmux client for focus selection:
    /// - Prefers clients already viewing `target_window_id` (if given).
    /// - Honors `active-pane` client flag.
    /// - Only trusts `focused` when the client's activity is within 60s of the freshest client
    ///   in the candidate set, preventing stale `focused` flags (when `focus-events` is off) from
    ///   beating a freshly active terminal.
    fn focus_client_rank(
        c: &ClientInfo,
        max_activity_s: u64,
        target_window_id: Option<&str>,
    ) -> (bool, bool, bool, u64) {
        const STALE_FOCUS_LAG_SECS: u64 = 60;
        let on_window = target_window_id.is_some_and(|w| !w.is_empty() && c.window_id == w);
        let active_pane = c.flags.split(',').any(|f| f.trim() == "active-pane");
        let fresh_focused = c.flags.split(',').any(|f| f.trim() == "focused")
            && max_activity_s.saturating_sub(c.activity_s) <= STALE_FOCUS_LAG_SECS;
        (on_window, active_pane, fresh_focused, c.activity_s)
    }

    /// Pick the best client to receive focus for `target_session`:
    /// prefers clients already attached to `target_session` (by `active-pane`, non-stale
    /// `focused`, then latest activity) so sibling multi-client terminals are not hijacked,
    /// then falls back to the most recently active/focused attached client overall.
    pub fn pick_focus_client<'a>(
        clients: &'a [ClientInfo],
        target_session: &str,
    ) -> Option<&'a ClientInfo> {
        let same: Vec<&ClientInfo> = clients
            .iter()
            .filter(|c| c.session_name == target_session)
            .collect();
        if !same.is_empty() {
            let max_act = same.iter().map(|c| c.activity_s).max().unwrap_or(0);
            return same
                .into_iter()
                .max_by_key(|c| Self::focus_client_rank(c, max_act, None));
        }
        let max_act = clients.iter().map(|c| c.activity_s).max().unwrap_or(0);
        clients
            .iter()
            .max_by_key(|c| Self::focus_client_rank(c, max_act, None))
    }

    /// Pick the best attached client for focusing `pane`, honoring `#{session_group}`,
    /// client `#{window_id}`, `active-pane`, and non-stale `focused`.
    /// When `allow_cross_session_switch` is `false`, returns `None` for detached sessions
    /// instead of hijacking an unrelated terminal client.
    pub fn pick_focus_client_for_pane<'a>(
        clients: &'a [ClientInfo],
        pane: &PaneInfo,
        allow_cross_session_switch: bool,
    ) -> Option<&'a ClientInfo> {
        let same: Vec<&ClientInfo> = clients
            .iter()
            .filter(|c| pane.same_session_or_group(c))
            .collect();
        if !same.is_empty() {
            let max_act = same.iter().map(|c| c.activity_s).max().unwrap_or(0);
            return same.into_iter().max_by_key(|c| {
                Self::focus_client_rank(c, max_act, Some(pane.window_id.as_str()))
            });
        }
        if !allow_cross_session_switch {
            return None;
        }
        let max_act = clients.iter().map(|c| c.activity_s).max().unwrap_or(0);
        clients
            .iter()
            .max_by_key(|c| Self::focus_client_rank(c, max_act, None))
    }

    /// Select `pane`'s window and pane, and focus an attached client in the same session
    /// or `session_group` without hijacking unrelated sessions' terminals. Succeeds cleanly
    /// even when the target session is detached (returning `switched_client: None`).
    pub fn focus_pane(&self, pane: &PaneId) -> Result<FocusOutcome, TmuxError> {
        valid_pane(pane)?;
        let panes = self.list_panes()?;
        let info = panes
            .into_iter()
            .find(|p| &p.pane_id == pane)
            .ok_or_else(|| TmuxError::Command {
                args: format!("focus {pane}"),
                code: Some(1),
                stderr: format!("pane {pane} not found"),
            })?;
        let tmux_target = format!(
            "{}:{}.{}",
            info.session_name, info.window_index, info.pane_index
        );
        let clients = self.list_clients()?;
        let matched = Self::pick_focus_client_for_pane(&clients, &info, false);
        let (switched_client, already_in_session) = match matched {
            Some(client) if client.session_name == info.session_name => {
                self.run(&["select-window", "-t", pane.as_str()], None)?;
                self.run(&["select-pane", "-t", pane.as_str()], None)?;
                (Some(client.tty.clone()), true)
            }
            Some(client) => {
                // Grouped session sharing windows: select the window on the attached sibling
                // session so the user's client displays this window without leaving its group.
                let group_win = format!("={}:{}", client.session_name, info.window_index);
                self.run(&["select-window", "-t", &group_win], None)?;
                self.run(&["select-pane", "-t", pane.as_str()], None)?;
                (Some(client.tty.clone()), true)
            }
            None => {
                // Target session is detached: pre-select window & pane inside that session without
                // hijacking clients attached to other unrelated sessions (e.g. `agent-session`).
                self.run(&["select-window", "-t", pane.as_str()], None)?;
                self.run(&["select-pane", "-t", pane.as_str()], None)?;
                (None, false)
            }
        };
        Ok(FocusOutcome {
            pane_id: pane.clone(),
            session_name: info.session_name,
            tmux_target,
            switched_client,
            already_in_session,
        })
    }

    /// Focus `pane` on the best matching client (`focus_pane` wrapper).
    pub fn switch_to(&self, pane: &PaneId) -> Result<(), TmuxError> {
        self.focus_pane(pane).map(|_| ())
    }

    /// Kill a single tmux pane (`tmux kill-pane -t <pane_id>`).
    ///
    /// Automatically closes the containing window when `pane` is its sole pane while preserving
    /// sibling split panes and the rest of the tmux session. Treats an already-gone pane or
    /// missing server idempotently as `Ok(())`.
    pub fn kill_pane(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        match self.run(&["kill-pane", "-t", pane.as_str()], None) {
            Ok(_) => Ok(()),
            Err(e) if e.is_no_server() => Ok(()),
            Err(TmuxError::Command { ref stderr, .. }) if stderr_means_pane_gone(stderr) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Build arguments for `tmux respawn-pane -k` suspending an active agent pane into a lightweight shell waiter.
    pub fn build_respawn_pane_args(
        pane: &PaneId,
        cwd: Option<&str>,
        waiter_script: &str,
    ) -> Result<Vec<String>, TmuxError> {
        valid_pane(pane)?;
        if waiter_script.trim().is_empty() {
            return Err(TmuxError::InvalidRequest(
                "suspend waiter script cannot be empty".into(),
            ));
        }
        let mut args = vec![
            "respawn-pane".to_string(),
            "-k".to_string(),
            "-t".to_string(),
            pane.0.clone(),
        ];
        if let Some(dir) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
            args.push("-c".to_string());
            args.push(dir.to_string());
        }
        args.push("--".to_string());
        args.push("/bin/sh".to_string());
        args.push("-lc".to_string());
        args.push(waiter_script.to_string());
        Ok(args)
    }

    /// Replace the running process inside `pane` with the interactive suspend waiter while preserving the pane/window.
    pub fn suspend_pane(&self, req: &crate::mux::SuspendPaneReq) -> Result<PaneId, TmuxError> {
        let args =
            Self::build_respawn_pane_args(&req.pane, req.cwd.as_deref(), &req.waiter_script)?;
        self.run_owned(&args, None)?;
        Ok(req.pane.clone())
    }

    /// Wake a suspended pane waiting on `[Enter]` by sending Enter (`C-m`) into its PTY.
    pub fn wake_suspended_pane(&self, req: &crate::mux::WakePaneReq) -> Result<PaneId, TmuxError> {
        self.submit_enter(&req.pane)?;
        Ok(req.pane.clone())
    }

    /// Stop the tmux server on this socket. Intended for sandbox sockets in tests; a server
    /// that is already gone is not an error.
    pub fn kill_server(&self) -> Result<(), TmuxError> {
        match self.run(&["kill-server"], None) {
            Ok(_) => Ok(()),
            Err(e) if e.is_no_server() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl TerminalMux for Tmux {
    fn kind(&self) -> MuxKind {
        MuxKind::Tmux
    }

    fn capabilities(&self) -> MuxCapabilities {
        MuxCapabilities {
            bracketed_paste_native: true,
            client_activity_ts: true,
            pane_user_options: true,
            full_scrollback: true,
            layout_snapshot: true,
        }
    }

    fn available(&self) -> bool {
        Tmux::available(self)
    }

    fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        Tmux::list_sessions(self)
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        Tmux::list_panes(self)
    }

    fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError> {
        Tmux::list_clients(self)
    }

    fn discover_panes_from(
        &self,
        panes: &[PaneInfo],
        opts: DiscoverOptions,
    ) -> Vec<DiscoveredPane> {
        Tmux::discover_panes_from(self, panes, opts)
    }

    fn capture_pane(&self, pane: &PaneId, req: &CaptureRequest) -> Result<PanePreview, TmuxError> {
        Tmux::capture_pane(self, pane, req)
    }

    fn capture_panes_tail_batch(&self, targets: &[(&PaneId, usize)]) -> HashMap<PaneId, String> {
        Tmux::capture_panes_tail_batch(self, targets)
    }

    fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError> {
        Tmux::cursor_line(self, pane, cursor_y)
    }

    fn capture_visible_screen(&self, pane: &PaneId) -> Result<String, TmuxError> {
        Tmux::capture_visible_screen(self, pane)
    }

    fn exit_copy_mode(&self, pane: &PaneId) -> Result<bool, TmuxError> {
        Tmux::exit_copy_mode(self, pane)
    }

    fn paste_pointer_text(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        Tmux::paste_pointer_text(self, pane, text, cfg)
    }

    fn paste_interactive_text(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError> {
        Tmux::paste_interactive_text(self, pane, text)
    }

    fn submit_enter(&self, pane: &PaneId) -> Result<(), TmuxError> {
        Tmux::submit_enter(self, pane)
    }

    fn send_key(&self, pane: &PaneId, key: &str) -> Result<(), TmuxError> {
        Tmux::send_key(self, pane, key)
    }

    fn spawn_window(&self, req: &SpawnWindow) -> Result<PaneId, TmuxError> {
        Tmux::spawn_window(self, req)
    }

    fn suspend_pane(&self, req: &crate::mux::SuspendPaneReq) -> Result<PaneId, TmuxError> {
        Tmux::suspend_pane(self, req)
    }

    fn wake_suspended_pane(&self, req: &crate::mux::WakePaneReq) -> Result<PaneId, TmuxError> {
        Tmux::wake_suspended_pane(self, req)
    }

    fn focus_pane(&self, pane: &PaneId) -> Result<FocusOutcome, TmuxError> {
        Tmux::focus_pane(self, pane)
    }

    fn kill_pane(&self, pane: &PaneId) -> Result<(), TmuxError> {
        Tmux::kill_pane(self, pane)
    }

    fn set_status(&self, pane: &PaneId, st: &PaneStatus) -> Result<(), TmuxError> {
        Tmux::set_status(self, pane, st)
    }

    fn clear_status(&self, pane: &PaneId) -> Result<(), TmuxError> {
        Tmux::clear_status(self, pane)
    }

    fn dump_layout(&self, session: &str) -> Result<String, TmuxError> {
        let windows = self.list_windows()?;
        let lines: Vec<String> = windows
            .into_iter()
            .filter(|w| w.session_name == session)
            .map(|w| format!("{}:{} {}", w.window_index, w.window_name, w.window_layout))
            .collect();
        Ok(lines.join("\n"))
    }
}

/// snake_case wire name of a state (matches serde `rename_all = "snake_case"`).
/// Exhaustive on purpose: a new `SessionState` variant must be named here.
fn state_name(st: SessionState) -> &'static str {
    match st {
        SessionState::Unknown => "unknown",
        SessionState::Starting => "starting",
        SessionState::Working => "working",
        SessionState::Idle => "idle",
        SessionState::NeedsPermission => "needs_permission",
        SessionState::NeedsInput => "needs_input",
        SessionState::Error => "error",
        SessionState::Suspended => "suspended",
        SessionState::Ended => "ended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_client(tty: &str, session: &str, activity_s: u64, flags: &str) -> ClientInfo {
        ClientInfo {
            tty: tty.into(),
            session_name: session.into(),
            activity_s,
            flags: flags.into(),
            session_group: String::new(),
            window_id: String::new(),
        }
    }

    #[test]
    fn pick_focus_client_for_pane_handles_groups_stale_focus_active_pane_and_detached() {
        let pane_grouped = PaneInfo {
            pane_id: PaneId::new("%7"),
            session_name: "work".into(),
            window_id: "@2".into(),
            window_index: 2,
            pane_index: 0,
            pane_pid: 100,
            current_command: "claude".into(),
            current_path: "/repo".into(),
            pane_active: true,
            window_active: true,
            session_attached: 1,
            in_mode: false,
            dead: false,
            synchronized: false,
            alternate_on: false,
            cursor_x: 0,
            cursor_y: 0,
            window_name: "w".into(),
            session_group: "grp-work".into(),
            history_size: 10,
            pane_height: 24,
        };

        // 1. Stale `focused` (500s older) loses to freshly active client without `focused`.
        let clients = vec![
            ClientInfo {
                tty: "/dev/ttys001".into(),
                session_name: "work-2".into(),
                activity_s: 1_000,
                flags: "attached,focused".into(),
                session_group: "grp-work".into(),
                window_id: "@1".into(),
            },
            ClientInfo {
                tty: "/dev/ttys002".into(),
                session_name: "work-3".into(),
                activity_s: 1_500,
                flags: "attached".into(),
                session_group: "grp-work".into(),
                window_id: "@1".into(),
            },
        ];
        let picked = Tmux::pick_focus_client_for_pane(&clients, &pane_grouped, false).unwrap();
        assert_eq!(picked.tty, "/dev/ttys002");

        // 2. Client with `active-pane` flag beats equal-window sibling.
        let clients_ap = vec![
            ClientInfo {
                tty: "/dev/ttys010".into(),
                session_name: "work".into(),
                activity_s: 2_000,
                flags: "attached,focused".into(),
                session_group: "grp-work".into(),
                window_id: "@2".into(),
            },
            ClientInfo {
                tty: "/dev/ttys011".into(),
                session_name: "work-2".into(),
                activity_s: 1_995,
                flags: "attached,active-pane".into(),
                session_group: "grp-work".into(),
                window_id: "@2".into(),
            },
        ];
        let picked_ap =
            Tmux::pick_focus_client_for_pane(&clients_ap, &pane_grouped, false).unwrap();
        assert_eq!(picked_ap.tty, "/dev/ttys011");

        // 3. Detached independent session (`agent-session`) must NOT hijack clients on other sessions.
        let mut detached_pane = pane_grouped.clone();
        detached_pane.session_name = "agent-session".into();
        detached_pane.session_group = String::new();
        detached_pane.session_attached = 0;
        assert!(Tmux::pick_focus_client_for_pane(&clients_ap, &detached_pane, false).is_none());
    }

    #[test]
    fn pick_focus_client_preserves_multi_client_sessions() {
        let clients = vec![
            mk_client("/dev/ttys001", "prome", 9_999, "attached,focused,UTF-8"),
            mk_client("/dev/ttys043", "learning", 5_000, "attached,UTF-8"),
            mk_client("/dev/ttys044", "learning", 6_000, "attached,focused,UTF-8"),
        ];

        // Prefers same-session client (and focused/newer within it) over busier other session.
        let hit = Tmux::pick_focus_client(&clients, "learning").unwrap();
        assert_eq!(hit.tty, "/dev/ttys044");

        // When target session is detached, falls back to focused/most active attached client.
        let fallback = Tmux::pick_focus_client(&clients, "spot").unwrap();
        assert_eq!(fallback.tty, "/dev/ttys001");

        // Without focused flag, picks highest activity_s.
        let unfocused = vec![
            mk_client("/dev/ttys001", "a", 100, "attached"),
            mk_client("/dev/ttys002", "b", 250, "attached"),
        ];
        assert_eq!(
            Tmux::pick_focus_client(&unfocused, "spot").unwrap().tty,
            "/dev/ttys002"
        );

        // No attached clients -> None.
        assert!(Tmux::pick_focus_client(&[], "spot").is_none());
    }

    #[test]
    fn builds_tmux_suspend_respawn_and_wake_commands() {
        use crate::exec::ExecOutput;
        use crate::mux::{SuspendPaneReq, WakePaneReq};
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
        let tmux = Tmux::with_transport(PathBuf::from("tmux"), None, exec.clone());
        let pane = PaneId::new("%14");

        let out_pane = tmux
            .suspend_pane(&SuspendPaneReq {
                pane: pane.clone(),
                cwd: Some("/repo/work".into()),
                pane_name: Some("claude-1".into()),
                waiter_script: "read -r _k; exec claude --resume abc".into(),
                resume_argv: vec!["claude".into(), "--resume".into(), "abc".into()],
            })
            .unwrap();
        assert_eq!(out_pane, pane);

        let woke = tmux
            .wake_suspended_pane(&WakePaneReq {
                pane: pane.clone(),
                cwd: Some("/repo/work".into()),
                pane_name: Some("claude-1".into()),
                resume_argv: vec!["claude".into(), "--resume".into(), "abc".into()],
            })
            .unwrap();
        assert_eq!(woke, pane);

        assert!(Tmux::build_respawn_pane_args(&PaneId::new("zj:work/1"), None, "echo hi").is_err());

        let calls = exec.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            vec![
                "respawn-pane",
                "-k",
                "-t",
                "%14",
                "-c",
                "/repo/work",
                "--",
                "/bin/sh",
                "-lc",
                "read -r _k; exec claude --resume abc",
            ]
        );
        assert_eq!(calls[1], vec!["send-keys", "-t", "%14", "C-m"]);
    }
}
