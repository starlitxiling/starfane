//! tmux inspection, focus detection, 7-point injection gate, and L4 interactive spawn.
//!
//! Std-only (plus `baton-proto` / `serde`): every tmux call is a synchronous child process
//! with a hard timeout. Pure decision logic ([`check_injection_gate`], [`is_user_watching`],
//! [`resolve_pane`], [`validate_pointer_text`]) is separated from process I/O so it can be
//! matrix-tested without a tmux server.

mod discover;
mod error;
mod exec;
mod gate;
mod model;
mod resolve;

pub use discover::{
    capture_pane_tail, classify_panes, discover_agent_panes, trim_and_cap_tail, DiscoveredPane,
    PanePreview, MAX_CAPTURE_BYTES,
};
pub use error::TmuxError;
pub use gate::{
    check_injection_gate, check_interactive_gate, is_bare_slash_command, is_user_watching,
    validate_interactive_text, validate_pointer_text, GateConfig,
};
pub use model::{
    parse_client_line, parse_clients, parse_pane_line, parse_panes, ClientInfo, PaneInfo,
    PaneStatus, FIELD_SEP,
};
pub use resolve::{pid_ancestors, resolve_pane, PaneHint};

use baton_proto::{PaneId, SessionState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_DETACHED_SESSION: &str = "baton";

/// Pane user options written by [`Tmux::set_status`].
pub const OPT_STATE: &str = "@baton_state";
pub const OPT_ROLE: &str = "@baton_role";
pub const OPT_PIPELINE: &str = "@baton_pipeline";
/// Mirror of the state for user status-line formats (`#{@agent_state}`).
pub const OPT_AGENT_STATE: &str = "@agent_state";

const BEL: &[u8] = b"\x07";
static BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Tmux {
    pub bin: PathBuf,
    /// `tmux -L <name>` socket (tests use `baton-test…`); `None` = the user's default server.
    pub socket_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpawnWindow {
    /// `None` → use (or create) the detached session [`DEFAULT_DETACHED_SESSION`].
    pub target_session: Option<String>,
    pub window_name: String,
    pub cwd: PathBuf,
    /// e.g. `("BATON_BIND", "B-…")`.
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

/// Unique-per-call paste buffer name: `baton_<pid>_<nanos>_<seq>`.
fn new_buffer_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("baton_{}_{nanos:x}_{seq}", std::process::id())
}

impl Tmux {
    pub fn new(bin: PathBuf, socket_name: Option<String>) -> Self {
        Tmux { bin, socket_name }
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
        exec::run(&self.bin, &full, stdin)
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
        exec::run(&self.bin, &[s("-V")], None)
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

    /// Paste `text` into `pane` as one bracketed paste, wait for the TUI's paste-burst
    /// detector to settle, then press Enter. **Does not run the gate**: callers must call
    /// [`check_injection_gate`] first. Never uses `send-keys -l`, `C-u` or `C-c`.
    pub fn inject_pointer(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        validate_pointer_text(text, cfg)?;
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
        thread::sleep(Duration::from_millis(cfg.paste_settle_ms));
        self.run(&["send-keys", "-t", pane.as_str(), "C-m"], None)?;
        Ok(())
    }

    /// Inject explicit interactive user input via bracketed paste (`paste-buffer -d -p`) +
    /// settle pause + `C-m`. Allows multi-line or slash text once validated by
    /// [`validate_interactive_text`]; callers must still enforce [`check_interactive_gate`].
    pub fn inject_interactive(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        validate_interactive_text(text)?;
        let buf = new_buffer_name();
        self.run(&["load-buffer", "-b", &buf, "-"], Some(text.as_bytes()))?;
        if let Err(e) = self.run(
            &["paste-buffer", "-d", "-p", "-b", &buf, "-t", pane.as_str()],
            None,
        ) {
            let _ = self.run(&["delete-buffer", "-b", &buf], None);
            return Err(e);
        }
        thread::sleep(Duration::from_millis(cfg.paste_settle_ms));
        self.run(&["send-keys", "-t", pane.as_str(), "C-m"], None)?;
        Ok(())
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
        self.ensure_session(&session, &cwd, req.target_session.is_none())?;

        let mut args = vec![
            s("new-window"),
            s("-d"),
            s("-P"),
            s("-F"),
            s("#{pane_id}"),
            s("-n"),
            req.window_name.clone(),
            s("-c"),
            cwd,
        ];
        for (k, v) in &req.env {
            args.push(s("-e"));
            args.push(format!("{k}={v}"));
        }
        args.push(s("-t"));
        args.push(target);
        args.push(s("--"));
        args.extend(req.argv.iter().cloned());
        let out = self.run_owned(&args, None)?;
        let pane = PaneId::new(out.trim());
        valid_pane(&pane).map_err(|_| TmuxError::Parse(format!("new-window printed {out:?}")))?;

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
            eprintln!("baton-tmux: remain-on-exit failed not applied to {pane}: {e}");
        }
        Ok(pane)
    }

    /// Make sure `session` exists. An explicit target session that is missing is an error
    /// unless it is the default detached one, which is created on demand.
    fn ensure_session(&self, session: &str, cwd: &str, may_create: bool) -> Result<(), TmuxError> {
        let exact = format!("={session}");
        let has = self.output(&[s("has-session"), s("-t"), exact], None)?;
        if has.success {
            return Ok(());
        }
        if !may_create {
            return Err(TmuxError::Command {
                args: format!("has-session -t ={session}"),
                code: has.code,
                stderr: has.stderr,
            });
        }
        match self.run(&["new-session", "-d", "-s", session, "-c", cwd], None) {
            Ok(_) => Ok(()),
            // Lost a creation race with another spawner: the session exists now.
            Err(TmuxError::Command { stderr, .. }) if stderr.contains("duplicate session") => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Publish agent state on the pane as user options (`@baton_state`, `@baton_role`,
    /// `@baton_pipeline`, plus `@agent_state` for status-line formats). `None` fields are unset.
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
    /// BEL to the pane's tty.
    pub fn ring_bell(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
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

    /// `switch-client -t <pane>` on the most recently active client.
    pub fn switch_to(&self, pane: &PaneId) -> Result<(), TmuxError> {
        valid_pane(pane)?;
        self.run(&["switch-client", "-t", pane.as_str()], None)
            .map(|_| ())
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
        SessionState::Ended => "ended",
    }
}
