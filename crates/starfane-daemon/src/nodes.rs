//! Local multi-mux pool (`tmux` + `zellij`) and multi-host federation (`NodeProvider`).
//!
//! Supports mixing zero-install SSH ControlMaster hosts (`SshMuxNodeProvider`) and
//! full peer `starfaned` federation (`PeerDaemonNodeProvider` with multi-candidate
//! outbound endpoint failover & Split Ownership) on the same hub.

use crate::config::{NodeConfig, NodeKindConfig, NotifyOwnership};
use crate::core::transcript::{TranscriptQuery, TranscriptResponse};
use crate::notice::BoxFuture;
use starfane_proto::{
    id, ApiError, Confidence, DeliveryLevel, DeliveryReport, DeliveryStatus, HostId, HostKind,
    HostRecord, HostStatus, PaneId, PaneResolution, SessionId, SessionRecord, SessionState,
    LOCAL_HOST_ID,
};
use starfane_tmux::{
    check_interactive_gate, classify_panes_with_git_lookup, parse_panes, parse_zellij_pane_id,
    qualify_host_pane, qualify_host_session, shell_single_quote, CaptureRequest, ClientInfo,
    DiscoverOptions, DiscoveredPane, ExecTransport, FocusOutcome, GateConfig, PaneInfo,
    PanePreview, PaneStatus, SshExec, TerminalMux, Tmux, TmuxError, ZellijMux,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PEER_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_PEER_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
const SSH_SPLIT_MARKER: &str = "__SF_SSH_PS_SPLIT__";

// ---------------------------------------------------------------------------
// LocalMuxPool (Tmux + optional Zellij >= 0.44)
// ---------------------------------------------------------------------------

/// Coordinates all local terminal multiplexers (`tmux` and optional `zellij >= 0.44`).
#[derive(Debug, Clone)]
pub struct LocalMuxPool {
    pub tmux: Tmux,
    pub zellij: Option<ZellijMux>,
}

impl LocalMuxPool {
    pub fn new(tmux: Tmux, zellij_bin: PathBuf, zellij_enabled: bool) -> Self {
        let zellij = if zellij_enabled {
            let z = ZellijMux::new(zellij_bin);
            if z.available() {
                log::info!("zellij multiplexer detected ({:?})", z.version());
                Some(z)
            } else {
                None
            }
        } else {
            None
        };
        Self { tmux, zellij }
    }

    pub fn is_zellij_pane(pane: &PaneId) -> bool {
        parse_zellij_pane_id(pane.as_str()).is_some()
    }

    pub fn list_all_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        let mut panes = match self.tmux.list_panes() {
            Ok(p) => p,
            Err(e) if self.zellij.is_some() => {
                log::debug!("tmux list_panes skipped (zellij active): {e}");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        if let Some(zj) = &self.zellij {
            match zj.list_panes() {
                Ok(mut zpanes) => panes.append(&mut zpanes),
                Err(e) => log::debug!("zellij list_panes failed: {e}"),
            }
        }
        Ok(panes)
    }

    pub fn list_all_clients(&self) -> Result<Vec<ClientInfo>, TmuxError> {
        let mut clients = self.tmux.list_clients().unwrap_or_default();
        if let Some(zj) = &self.zellij {
            if let Ok(mut zclients) = zj.list_clients() {
                clients.append(&mut zclients);
            }
        }
        Ok(clients)
    }

    pub fn discover_panes_from(
        &self,
        panes: &[PaneInfo],
        opts: DiscoverOptions,
    ) -> Vec<DiscoveredPane> {
        let mut tmux_panes = Vec::new();
        let mut zj_panes = Vec::new();
        for p in panes {
            if Self::is_zellij_pane(&p.pane_id) {
                zj_panes.push(p.clone());
            } else {
                tmux_panes.push(p.clone());
            }
        }
        let mut out = if !tmux_panes.is_empty() {
            self.tmux.discover_panes_from(&tmux_panes, opts)
        } else {
            Vec::new()
        };
        if let (Some(zj), false) = (&self.zellij, zj_panes.is_empty()) {
            out.extend(zj.discover_panes_from(&zj_panes, opts));
        }
        out
    }

    pub fn capture_pane(
        &self,
        pane: &PaneId,
        req: &CaptureRequest,
    ) -> Result<PanePreview, TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.capture_pane(pane, req)
        } else {
            self.tmux.capture_pane(pane, req)
        }
    }

    pub fn capture_panes_tail_batch(
        &self,
        targets: &[(&PaneId, usize)],
    ) -> HashMap<PaneId, String> {
        let mut tmux_targets = Vec::new();
        let mut zj_targets = Vec::new();
        for &(p, n) in targets {
            if Self::is_zellij_pane(p) {
                zj_targets.push((p, n));
            } else {
                tmux_targets.push((p, n));
            }
        }
        let mut out = if !tmux_targets.is_empty() {
            self.tmux.capture_panes_tail_batch(&tmux_targets)
        } else {
            HashMap::new()
        };
        if let (Some(zj), false) = (&self.zellij, zj_targets.is_empty()) {
            out.extend(zj.capture_panes_tail_batch(&zj_targets));
        }
        out
    }

    pub fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.cursor_line(pane, cursor_y)
        } else {
            self.tmux.cursor_line(pane, cursor_y)
        }
    }

    pub fn paste_pointer_text(
        &self,
        pane: &PaneId,
        text: &str,
        cfg: &GateConfig,
    ) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.paste_pointer_text(pane, text, cfg)
        } else {
            self.tmux.paste_pointer_text(pane, text, cfg)
        }
    }

    pub fn paste_interactive_text(&self, pane: &PaneId, text: &str) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.paste_interactive_text(pane, text)
        } else {
            self.tmux.paste_interactive_text(pane, text)
        }
    }

    pub fn submit_enter(&self, pane: &PaneId) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.submit_enter(pane)
        } else {
            self.tmux.submit_enter(pane)
        }
    }

    pub fn focus_pane(&self, pane: &PaneId) -> Result<FocusOutcome, TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.focus_pane(pane)
        } else {
            self.tmux.focus_pane(pane)
        }
    }

    pub fn set_status(&self, pane: &PaneId, st: &PaneStatus) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            Ok(())
        } else {
            self.tmux.set_status(pane, st)
        }
    }

    pub fn clear_status(&self, pane: &PaneId) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            Ok(())
        } else {
            self.tmux.clear_status(pane)
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-host NodeProvider abstraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub host: HostRecord,
    pub sessions: Vec<SessionRecord>,
}

pub trait NodeProvider: Send + Sync {
    fn host_id(&self) -> &HostId;
    fn kind(&self) -> HostKind;
    fn default_display_name(&self) -> &str;
    fn notify_ownership(&self) -> NotifyOwnership;

    fn sync<'a>(&'a self, display_name: &'a str) -> BoxFuture<'a, Result<NodeSnapshot, String>>;

    fn capture_pane<'a>(
        &'a self,
        local_pane: &'a PaneId,
        req: &'a CaptureRequest,
    ) -> BoxFuture<'a, Result<PanePreview, ApiError>>;

    fn fetch_transcript<'a>(
        &'a self,
        remote_session: &'a SessionId,
        query: &'a TranscriptQuery,
    ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>>;

    fn dispatch_interactive<'a>(
        &'a self,
        remote_session: Option<&'a SessionId>,
        local_pane: &'a PaneId,
        text: &'a str,
        settle_ms: u64,
    ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>>;

    fn focus_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>>;
}

pub fn build_remote_providers(
    nodes: &[NodeConfig],
    state_dir: &Path,
) -> HashMap<HostId, Arc<dyn NodeProvider>> {
    let mut map: HashMap<HostId, Arc<dyn NodeProvider>> = HashMap::new();
    let ssh_dir = state_dir.join("ssh-cm");
    for cfg in nodes {
        if !cfg.enabled {
            continue;
        }
        let host_id = HostId::new(cfg.id.trim());
        if host_id.is_empty() || host_id.as_str() == LOCAL_HOST_ID {
            continue;
        }
        let provider: Arc<dyn NodeProvider> = match cfg.kind {
            NodeKindConfig::Ssh => Arc::new(SshMuxNodeProvider::new(cfg, &ssh_dir)),
            NodeKindConfig::Peer => Arc::new(PeerDaemonNodeProvider::new(cfg)),
        };
        map.insert(host_id, provider);
    }
    map
}

// ---------------------------------------------------------------------------
// SshMuxNodeProvider (zero-install ControlMaster)
// ---------------------------------------------------------------------------

pub struct SshMuxNodeProvider {
    host_id: HostId,
    default_display_name: String,
    ownership: NotifyOwnership,
    include_shell_panes: bool,
    tmux: Tmux,
    transport: Arc<SshExec>,
    tmux_bin: String,
    tmux_socket: Option<String>,
}

impl SshMuxNodeProvider {
    pub fn new(cfg: &NodeConfig, ssh_dir: &Path) -> Self {
        let host_id = HostId::new(cfg.id.trim());
        let display = cfg
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(host_id.as_str())
            .to_string();
        let control_path =
            ssh_dir.join(format!("cm-{}.sock", sanitize_sock_name(host_id.as_str())));
        let transport = Arc::new(
            SshExec::new(cfg.ssh_target.trim(), control_path).with_extra_args(cfg.ssh_args.clone()),
        );
        let tmux_bin = if cfg.tmux_bin.trim().is_empty() {
            "tmux".to_string()
        } else {
            cfg.tmux_bin.trim().to_string()
        };
        let tmux_socket = {
            let s = cfg.tmux_socket.trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        let tmux = Tmux::with_transport(
            PathBuf::from(&tmux_bin),
            tmux_socket.clone(),
            transport.clone(),
        );
        Self {
            host_id,
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            include_shell_panes: cfg.include_shell_panes,
            tmux,
            transport,
            tmux_bin,
            tmux_socket,
        }
    }

    /// Build a single-RTT remote shell script that fetches `tmux list-panes -a` AND `ps -eo` together.
    fn compound_discover_script(&self) -> String {
        let sock_part = match &self.tmux_socket {
            Some(s) => format!("-L {} ", shell_single_quote(s)),
            None => String::new(),
        };
        format!(
            "{} {}list-panes -a -F {} 2>/dev/null || true; printf '\\n{}\\n'; ps -eo pid=,ppid=,args= 2>/dev/null || true",
            shell_single_quote(&self.tmux_bin),
            sock_part,
            shell_single_quote(starfane_tmux::PANE_FORMAT),
            SSH_SPLIT_MARKER
        )
    }
}

pub fn parse_ssh_compound_output(
    host_id: &HostId,
    display_name: &str,
    stdout: &str,
    include_shell_panes: bool,
    now_ms: u64,
) -> Result<Vec<SessionRecord>, String> {
    let (panes_raw, ps_raw) = stdout
        .split_once(SSH_SPLIT_MARKER)
        .ok_or_else(|| "missing SSH discovery split marker".to_string())?;
    let panes = parse_panes(panes_raw).map_err(|e| e.to_string())?;
    let discovered = classify_panes_with_git_lookup(
        &panes,
        ps_raw,
        DiscoverOptions {
            include_shell_panes,
        },
        false,
    );
    let mut sessions = Vec::with_capacity(discovered.len());
    for dp in discovered {
        let qualified_pane = qualify_host_pane(host_id, &dp.pane_id);
        let legacy_sid = SessionId::new(format!("tmux-{}", dp.pane_id.as_str()));
        let qualified_sid = qualify_host_session(host_id, &legacy_sid);
        sessions.push(SessionRecord {
            session_id: qualified_sid,
            agent: dp.agent,
            pane: Some(qualified_pane),
            pane_resolution: PaneResolution::Env,
            cwd: Some(dp.cwd),
            git_root: None,
            model: dp.model,
            effort: dp.effort,
            state: if dp.pane_dead {
                SessionState::Ended
            } else {
                SessionState::Idle
            },
            state_since_ms: now_ms,
            last_event_id: None,
            last_event_ms: now_ms,
            last_message: None,
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Heuristic,
            tmux_target: Some(dp.tmux_target),
            tmux_label: Some(dp.tmux_label),
            discovered_only: true,
            transcript_path: None,
            metrics: None,
            host_id: Some(host_id.clone()),
            host_display_name: Some(display_name.to_string()),
            mux_kind: Some("tmux".to_string()),
        });
    }
    Ok(sessions)
}

impl NodeProvider for SshMuxNodeProvider {
    fn host_id(&self) -> &HostId {
        &self.host_id
    }
    fn kind(&self) -> HostKind {
        HostKind::Ssh
    }
    fn default_display_name(&self) -> &str {
        &self.default_display_name
    }
    fn notify_ownership(&self) -> NotifyOwnership {
        self.ownership
    }

    fn sync<'a>(&'a self, display_name: &'a str) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        let host_id = self.host_id.clone();
        let display = display_name.to_string();
        let include_shell = self.include_shell_panes;
        let transport = self.transport.clone();
        let script = self.compound_discover_script();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || transport.run_shell(&script))
                .await
                .map_err(|e| format!("ssh task join: {e}"))?
                .map_err(|e| e.to_string())?;
            if !out.success {
                return Err(format!(
                    "ssh {} failed (exit {:?}): {}",
                    host_id,
                    out.code,
                    out.stderr.trim()
                ));
            }
            let now = id::now_ms();
            let sessions =
                parse_ssh_compound_output(&host_id, &display, &out.stdout, include_shell, now)?;
            let count = sessions
                .iter()
                .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
                .count();
            Ok(NodeSnapshot {
                host: HostRecord {
                    id: host_id,
                    display_name: display,
                    kind: HostKind::Ssh,
                    status: HostStatus::Online,
                    active_endpoint: Some(self.transport.target().to_string()),
                    last_seen_ms: Some(now),
                    error: None,
                    session_count: count,
                },
                sessions,
            })
        })
    }

    fn capture_pane<'a>(
        &'a self,
        local_pane: &'a PaneId,
        req: &'a CaptureRequest,
    ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
        let tmux = self.tmux.clone();
        let pane = local_pane.clone();
        let req = req.clone();
        let host_id = self.host_id.clone();
        Box::pin(async move {
            let mut preview = tokio::task::spawn_blocking(move || tmux.capture_pane(&pane, &req))
                .await
                .map_err(|e| ApiError::internal(format!("ssh capture task: {e}")))?
                .map_err(|e| ApiError::internal(format!("ssh capture: {e}")))?;
            preview.pane_id = qualify_host_pane(&host_id, &preview.pane_id);
            Ok(preview)
        })
    }

    fn fetch_transcript<'a>(
        &'a self,
        _remote_session: &'a SessionId,
        _query: &'a TranscriptQuery,
    ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
        Box::pin(async move {
            Err(ApiError::not_found(
                "zero-install SSH host has no remote transcript daemon; use terminal view",
            ))
        })
    }

    fn dispatch_interactive<'a>(
        &'a self,
        _remote_session: Option<&'a SessionId>,
        local_pane: &'a PaneId,
        text: &'a str,
        settle_ms: u64,
    ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
        let tmux = self.tmux.clone();
        let pane = local_pane.clone();
        let text = text.to_string();
        let host_id = self.host_id.clone();
        Box::pin(async move {
            let qualified = qualify_host_pane(&host_id, &pane);
            // Enforce the 3 hard tmux safety checks (PaneAlive, NotInMode, NotSynchronized)
            // over SSH before pasting, matching local interactive delivery safety.
            let t_gate = tmux.clone();
            let p_gate = pane.clone();
            let gate = tokio::task::spawn_blocking(move || {
                let panes = t_gate.list_panes().map_err(|e| e.to_string())?;
                let info = panes
                    .into_iter()
                    .find(|p| p.pane_id == p_gate)
                    .ok_or_else(|| format!("remote pane {p_gate} not found"))?;
                Ok::<_, String>(check_interactive_gate(&info))
            })
            .await
            .map_err(|e| ApiError::internal(format!("ssh gate task: {e}")))?
            .map_err(|e| ApiError::new(starfane_proto::ErrorCode::Unavailable, e))?;

            if !gate.passed {
                return Ok(DeliveryReport {
                    level: DeliveryLevel::L2Inject,
                    status: DeliveryStatus::Failed,
                    gate: Some(gate),
                    pane: Some(qualified),
                    ack: None,
                });
            }

            let t1 = tmux.clone();
            let p1 = pane.clone();
            let txt = text.clone();
            tokio::task::spawn_blocking(move || t1.paste_interactive_text(&p1, &txt))
                .await
                .map_err(|e| ApiError::internal(format!("ssh paste task: {e}")))?
                .map_err(|e| ApiError::internal(format!("ssh paste: {e}")))?;
            tokio::time::sleep(Duration::from_millis(settle_ms)).await;
            let p2 = pane.clone();
            tokio::task::spawn_blocking(move || tmux.submit_enter(&p2))
                .await
                .map_err(|e| ApiError::internal(format!("ssh submit task: {e}")))?
                .map_err(|e| ApiError::internal(format!("ssh submit: {e}")))?;
            Ok(DeliveryReport {
                level: DeliveryLevel::L2Inject,
                status: DeliveryStatus::Delivered,
                gate: Some(gate),
                pane: Some(qualified),
                ack: Some(format!("ssh:{host_id}")),
            })
        })
    }

    fn focus_pane<'a>(
        &'a self,
        _remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
        let tmux = self.tmux.clone();
        let pane = local_pane.clone();
        let target = self.transport.target().to_string();
        let tmux_bin = self.tmux_bin.clone();
        let tmux_socket = self.tmux_socket.clone();
        Box::pin(async move {
            let outcome = tokio::task::spawn_blocking(move || tmux.focus_pane(&pane))
                .await
                .map_err(|e| ApiError::internal(format!("ssh focus task: {e}")))?
                .map_err(|e| ApiError::internal(format!("ssh focus: {e}")))?;
            let sock_part = match &tmux_socket {
                Some(s) => format!("-L {} ", shell_single_quote(s)),
                None => String::new(),
            };
            let remote_attach = format!(
                "{} {}attach -t {}",
                shell_single_quote(&tmux_bin),
                sock_part,
                shell_single_quote(&outcome.session_name)
            );
            let attach_cmd = format!(
                "ssh -t {} {}",
                shell_single_quote(&target),
                shell_single_quote(&remote_attach)
            );
            Ok(serde_json::json!({
                "focused": true,
                "session_name": outcome.session_name,
                "tmux_target": outcome.tmux_target,
                "attach_command": attach_cmd,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// PeerDaemonNodeProvider (multi-candidate failover + Split Ownership)
// ---------------------------------------------------------------------------

pub struct PeerDaemonNodeProvider {
    host_id: HostId,
    default_display_name: String,
    ownership: NotifyOwnership,
    endpoints: Vec<String>,
    token: Option<String>,
    active_endpoint_idx: Mutex<usize>,
}

impl PeerDaemonNodeProvider {
    pub fn new(cfg: &NodeConfig) -> Self {
        let host_id = HostId::new(cfg.id.trim());
        let display = cfg
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(host_id.as_str())
            .to_string();
        let token = if !cfg.token.trim().is_empty() {
            Some(cfg.token.trim().to_string())
        } else if !cfg.token_env.trim().is_empty() {
            std::env::var(cfg.token_env.trim())
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let endpoints: Vec<String> = cfg
            .endpoints
            .iter()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .collect();
        Self {
            host_id,
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            endpoints,
            token,
            active_endpoint_idx: Mutex::new(0),
        }
    }

    fn ordered_candidates(&self) -> Vec<(usize, String)> {
        if self.endpoints.is_empty() {
            return Vec::new();
        }
        let start = *self
            .active_endpoint_idx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            % self.endpoints.len();
        (0..self.endpoints.len())
            .map(|offset| {
                let idx = (start + offset) % self.endpoints.len();
                (idx, self.endpoints[idx].clone())
            })
            .collect()
    }

    /// Execute an HTTP request across multi-candidate outbound endpoints with sticky failover.
    /// Transport errors and 5xx responses fail over to the next candidate; authoritative 4xx
    /// application responses (400/404/409/422) from a live peer are returned immediately.
    async fn request_json(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(String, serde_json::Value), PeerHttpError> {
        let candidates = self.ordered_candidates();
        if candidates.is_empty() {
            return Err(PeerHttpError::Transport(
                "no peer endpoints configured".into(),
            ));
        }
        let mut last_err = PeerHttpError::Transport("no peer endpoints tried".into());
        for (idx, base) in candidates {
            let url = format!("{base}{path_and_query}");
            match http_request_json(method, &url, self.token.as_deref(), body).await {
                Ok(val) => {
                    *self
                        .active_endpoint_idx
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = idx;
                    return Ok((base, val));
                }
                Err(PeerHttpError::Http { status, body })
                    if matches!(status, 400 | 404 | 409 | 422) =>
                {
                    *self
                        .active_endpoint_idx
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = idx;
                    return Err(PeerHttpError::Http { status, body });
                }
                Err(e) => {
                    last_err = e.with_prefix(&base);
                }
            }
        }
        Err(last_err)
    }
}

impl NodeProvider for PeerDaemonNodeProvider {
    fn host_id(&self) -> &HostId {
        &self.host_id
    }
    fn kind(&self) -> HostKind {
        HostKind::Peer
    }
    fn default_display_name(&self) -> &str {
        &self.default_display_name
    }
    fn notify_ownership(&self) -> NotifyOwnership {
        self.ownership
    }

    fn sync<'a>(&'a self, display_name: &'a str) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        let display = display_name.to_string();
        Box::pin(async move {
            let (active_ep, env) = self
                .request_json("GET", "/api/state", None)
                .await
                .map_err(|e| e.to_string())?;
            let data = env.get("data").unwrap_or(&env);
            let raw_sessions = data
                .get("sessions")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let parsed: Vec<SessionRecord> = serde_json::from_value(raw_sessions)
                .map_err(|e| format!("parse peer sessions: {e}"))?;
            let mut qualified = Vec::with_capacity(parsed.len());
            for mut s in parsed {
                // Only import sessions local to that peer (prevent federation loops).
                if !s.is_local_host() {
                    continue;
                }
                s.session_id = qualify_host_session(&self.host_id, &s.session_id);
                s.pane = s.pane.as_ref().map(|p| qualify_host_pane(&self.host_id, p));
                s.parent_session_id = s
                    .parent_session_id
                    .as_ref()
                    .map(|p| qualify_host_session(&self.host_id, p));
                s.host_id = Some(self.host_id.clone());
                s.host_display_name = Some(display.clone());
                qualified.push(s);
            }
            let now = id::now_ms();
            let count = qualified
                .iter()
                .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
                .count();
            Ok(NodeSnapshot {
                host: HostRecord {
                    id: self.host_id.clone(),
                    display_name: display,
                    kind: HostKind::Peer,
                    status: HostStatus::Online,
                    active_endpoint: Some(active_ep),
                    last_seen_ms: Some(now),
                    error: None,
                    session_count: count,
                },
                sessions: qualified,
            })
        })
    }

    fn capture_pane<'a>(
        &'a self,
        local_pane: &'a PaneId,
        req: &'a CaptureRequest,
    ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
        let mut qs = format!("lines={}", req.lines);
        if req.all {
            qs.push_str("&all=1");
        }
        if let Some(before) = req.before_line {
            qs.push_str(&format!("&before_line={before}"));
        }
        let encoded_pane = pct_encode(local_pane.as_str());
        let path = format!("/api/panes/{encoded_pane}/preview?{qs}");
        let host_id = self.host_id.clone();
        Box::pin(async move {
            let (_, env) = self
                .request_json("GET", &path, None)
                .await
                .map_err(|e| e.into_api_error("peer preview failed"))?;
            let data = env.get("data").cloned().unwrap_or(env);
            let mut preview: PanePreview = serde_json::from_value(data)
                .map_err(|e| ApiError::internal(format!("decode peer preview: {e}")))?;
            preview.pane_id = qualify_host_pane(&host_id, &preview.pane_id);
            Ok(preview)
        })
    }

    fn fetch_transcript<'a>(
        &'a self,
        remote_session: &'a SessionId,
        query: &'a TranscriptQuery,
    ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
        let mut qs = format!("limit={}", query.limit);
        if query.all {
            qs.push_str("&all=1");
        }
        if let Some(before) = query.before {
            qs.push_str(&format!("&before={before}"));
        }
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/transcript?{qs}");
        Box::pin(async move {
            let (_, env) = self
                .request_json("GET", &path, None)
                .await
                .map_err(|e| e.into_api_error("peer transcript failed"))?;
            let data = env.get("data").cloned().unwrap_or(env);
            serde_json::from_value(data)
                .map_err(|e| ApiError::internal(format!("decode peer transcript: {e}")))
        })
    }

    fn dispatch_interactive<'a>(
        &'a self,
        remote_session: Option<&'a SessionId>,
        local_pane: &'a PaneId,
        text: &'a str,
        _settle_ms: u64,
    ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
        let target = match remote_session {
            Some(sid) => serde_json::json!({
                "type": "session",
                "id": sid.as_str(),
            }),
            None => serde_json::json!({
                "type": "pane",
                "id": local_pane.as_str(),
            }),
        };
        let body = serde_json::json!({
            "target": target,
            "text": text,
            "interactive": true,
        });
        let host_id = self.host_id.clone();
        let qualified_pane = qualify_host_pane(&self.host_id, local_pane);
        Box::pin(async move {
            let (_, env) = self
                .request_json("POST", "/api/dispatch", Some(&body))
                .await
                .map_err(|e| e.into_api_error("peer dispatch failed"))?;
            let data = env.get("data").cloned().unwrap_or(env);
            let mut report: DeliveryReport = serde_json::from_value(data)
                .map_err(|e| ApiError::internal(format!("decode peer dispatch report: {e}")))?;
            report.pane = Some(match report.pane {
                Some(p) => qualify_host_pane(&host_id, &p),
                None => qualified_pane,
            });
            Ok(report)
        })
    }

    fn focus_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        _local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/focus");
        Box::pin(async move {
            let (_, env) = self
                .request_json("POST", &path, Some(&serde_json::json!({})))
                .await
                .map_err(|e| e.into_api_error("peer focus failed"))?;
            Ok(env.get("data").cloned().unwrap_or(env))
        })
    }
}

// ---------------------------------------------------------------------------
// Minimal Zero-Dependency Async HTTP/1.1 Client (http:// via Tokio TcpStream; https:// via curl)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PeerHttpError {
    Transport(String),
    Http { status: u16, body: String },
}

impl PeerHttpError {
    fn with_prefix(self, base: &str) -> Self {
        match self {
            PeerHttpError::Transport(m) => PeerHttpError::Transport(format!("{base}: {m}")),
            PeerHttpError::Http { status, body } => PeerHttpError::Http {
                status,
                body: format!("{base}: {body}"),
            },
        }
    }

    fn into_api_error(self, ctx: &str) -> ApiError {
        use starfane_proto::ErrorCode;
        match self {
            PeerHttpError::Transport(m) => {
                ApiError::new(ErrorCode::Unavailable, format!("{ctx}: {m}"))
            }
            PeerHttpError::Http { status, body } => {
                let msg = format!("{ctx}: HTTP {status}: {body}");
                let code = match status {
                    400 => ErrorCode::BadRequest,
                    404 => ErrorCode::NotFound,
                    409 => ErrorCode::Conflict,
                    422 => ErrorCode::GateRejected,
                    503 => ErrorCode::Unavailable,
                    _ => ErrorCode::Internal,
                };
                ApiError::new(code, msg)
            }
        }
    }
}

impl std::fmt::Display for PeerHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerHttpError::Transport(m) => f.write_str(m),
            PeerHttpError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
        }
    }
}

async fn http_request_json(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, PeerHttpError> {
    if let Some(rest) = url.strip_prefix("http://") {
        tokio::time::timeout(
            PEER_HTTP_TIMEOUT,
            http1_plain_json(method, rest, token, body),
        )
        .await
        .map_err(|_| {
            PeerHttpError::Transport(format!("timeout after {}s", PEER_HTTP_TIMEOUT.as_secs()))
        })?
    } else if url.starts_with("https://") {
        curl_http_json(method, url, token, body).await
    } else {
        Err(PeerHttpError::Transport(format!(
            "unsupported peer endpoint scheme: {url}"
        )))
    }
}

async fn http1_plain_json(
    method: &str,
    without_scheme: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, PeerHttpError> {
    let (host_port, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    let connect_addr = if host_port.starts_with('[') {
        if host_port.contains("]:") {
            host_port.to_string()
        } else {
            format!("{host_port}:80")
        }
    } else if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };

    let mut stream = TcpStream::connect(&connect_addr)
        .await
        .map_err(|e| PeerHttpError::Transport(format!("connect {connect_addr}: {e}")))?;

    let body_bytes = match body {
        Some(v) => serde_json::to_vec(v)
            .map_err(|e| PeerHttpError::Transport(format!("encode body: {e}")))?,
        None => Vec::new(),
    };

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(tok) = token {
        if tok.contains(['\r', '\n']) {
            return Err(PeerHttpError::Transport(
                "invalid newline in bearer token".into(),
            ));
        }
        req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if body.is_some() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
        req.push_str(&format!("Origin: http://{host_port}\r\n"));
    }
    req.push_str("\r\n");

    let mut wire = req.into_bytes();
    wire.extend_from_slice(&body_bytes);
    stream
        .write_all(&wire)
        .await
        .map_err(|e| PeerHttpError::Transport(format!("write request: {e}")))?;

    let mut raw = Vec::new();
    stream
        .take(MAX_PEER_RESPONSE_BYTES + 1)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| PeerHttpError::Transport(format!("read response: {e}")))?;
    if (raw.len() as u64) > MAX_PEER_RESPONSE_BYTES {
        return Err(PeerHttpError::Transport(format!(
            "peer response exceeded {} MiB limit",
            MAX_PEER_RESPONSE_BYTES / (1024 * 1024)
        )));
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| PeerHttpError::Transport("malformed HTTP response".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status_code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let is_chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    let body_slice = &raw[split + 4..];
    let decoded = if is_chunked {
        decode_chunked(body_slice).map_err(PeerHttpError::Transport)?
    } else {
        body_slice.to_vec()
    };
    if !(200..300).contains(&status_code) {
        return Err(PeerHttpError::Http {
            status: status_code,
            body: String::from_utf8_lossy(&decoded).trim().to_string(),
        });
    }
    serde_json::from_slice(&decoded)
        .map_err(|e| PeerHttpError::Transport(format!("parse JSON: {e}")))
}

fn decode_chunked(mut src: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut saw_terminator = false;
    while let Some(pos) = src.windows(2).position(|w| w == b"\r\n") {
        let size_line = std::str::from_utf8(&src[..pos])
            .map_err(|e| format!("chunk size utf8: {e}"))?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_line, 16)
            .map_err(|e| format!("chunk size hex {size_line:?}: {e}"))?;
        src = &src[pos + 2..];
        if size == 0 {
            saw_terminator = true;
            break;
        }
        if src.len() < size + 2 || &src[size..size + 2] != b"\r\n" {
            return Err("truncated chunked body".into());
        }
        out.extend_from_slice(&src[..size]);
        src = &src[size + 2..];
    }
    if !saw_terminator {
        return Err("incomplete chunked response (missing 0 terminator)".into());
    }
    Ok(out)
}

fn curl_escape_config_val(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

async fn curl_http_json(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, PeerHttpError> {
    if url.contains(['\r', '\n']) {
        return Err(PeerHttpError::Transport(
            "invalid newline in peer URL".into(),
        ));
    }
    if token.is_some_and(|t| t.contains(['\r', '\n'])) {
        return Err(PeerHttpError::Transport(
            "invalid newline in peer token".into(),
        ));
    }
    let mut cfg_stdin = format!("url = \"{}\"\n", curl_escape_config_val(url));
    if let Some(tok) = token {
        cfg_stdin.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_escape_config_val(tok)
        ));
    }
    if let Some(val) = body {
        let body_str = serde_json::to_string(val)
            .map_err(|e| PeerHttpError::Transport(format!("encode body: {e}")))?;
        if let Some(rest) = url.strip_prefix("https://") {
            let host_port = rest.split('/').next().unwrap_or("");
            if !host_port.is_empty() {
                cfg_stdin.push_str(&format!(
                    "header = \"Origin: https://{}\"\n",
                    curl_escape_config_val(host_port)
                ));
            }
        }
        cfg_stdin.push_str("header = \"Content-Type: application/json\"\n");
        cfg_stdin.push_str(&format!(
            "data-raw = \"{}\"\n",
            curl_escape_config_val(&body_str)
        ));
    }
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        "6",
        "-X",
        method,
        "-w",
        "\n%{http_code}",
        "-K",
        "-",
    ]);
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| PeerHttpError::Transport(format!("spawn curl: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg_stdin.as_bytes()).await;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| PeerHttpError::Transport(format!("wait curl: {e}")))?;
    if !out.status.success() {
        return Err(PeerHttpError::Transport(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let full = String::from_utf8_lossy(&out.stdout);
    let (body_part, code_part) = full.rsplit_once('\n').unwrap_or(("", full.as_ref()));
    let status = code_part.trim().parse::<u16>().unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(PeerHttpError::Http {
            status,
            body: body_part.trim().to_string(),
        });
    }
    serde_json::from_str(body_part)
        .map_err(|e| PeerHttpError::Transport(format!("parse curl JSON: {e}")))
}

fn sanitize_sock_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Split a possibly host-prefixed session id (e.g. `"tencent:tmux-%1"` or `"mac-xiling:uuid"`).
pub fn split_session_host(sid: &str, known_hosts: &HashSet<HostId>) -> Option<(HostId, SessionId)> {
    let (prefix, rest) = sid.split_once(':')?;
    let hid = HostId::new(prefix);
    if known_hosts.contains(&hid) && !rest.is_empty() {
        Some((hid, SessionId::new(rest)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn ssh_compound_output_parses_and_qualifies_host_prefix() {
        let sep = starfane_tmux::FIELD_SEP.to_string();
        let pane_row = [
            "%12", "dev", "@1", "1", "0", "4242", "claude", "/srv/app", "1", "1", "1", "0", "0",
            "0", "0", "0", "10", "main", "", "500", "40",
        ]
        .join(&sep);
        let ps_row = "4242 1 claude --model opus\n";
        let combined = format!("{pane_row}\n{SSH_SPLIT_MARKER}\n{ps_row}");
        let host = HostId::new("tencent");
        let sessions =
            parse_ssh_compound_output(&host, "Tencent GPU", &combined, false, 12345).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.session_id.as_str(), "tencent:tmux-%12");
        assert_eq!(s.pane.as_ref().unwrap().as_str(), "tencent:%12");
        assert_eq!(s.host_id.as_ref().unwrap().as_str(), "tencent");
        assert_eq!(s.host_display_name.as_deref(), Some("Tencent GPU"));
        assert_eq!(s.model.as_deref(), Some("opus"));
    }

    #[tokio::test]
    async fn peer_provider_fails_over_across_candidate_endpoints_and_prefixes_sessions() {
        // Bind an ephemeral local TCP listener serving a minimal `/api/state` response.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "ok": true,
                "data": {
                    "sessions": [{
                        "session_id": "sess-99",
                        "agent": "codex",
                        "pane": "%4",
                        "pane_resolution": "env",
                        "cwd": "/Users/xiling/app",
                        "state": "idle",
                        "state_since_ms": 100,
                        "last_event_ms": 200,
                        "confidence": "exact",
                        "tmux_target": "work:1.0",
                        "tmux_label": "work-app"
                    }]
                }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let cfg = NodeConfig {
            id: "mac-xiling".into(),
            display_name: Some("Mac Xiling".into()),
            kind: NodeKindConfig::Peer,
            enabled: true,
            // First endpoint is closed/unreachable; second endpoint is our live server -> failover!
            endpoints: vec!["http://127.0.0.1:1".into(), format!("http://{live_addr}")],
            ..NodeConfig::default()
        };
        let provider = PeerDaemonNodeProvider::new(&cfg);
        assert_eq!(provider.notify_ownership(), NotifyOwnership::NodeLocal);

        let snap = provider.sync("Mac Xiling").await.unwrap();
        assert_eq!(snap.host.status, HostStatus::Online);
        assert_eq!(
            snap.host.active_endpoint.as_deref(),
            Some(format!("http://{live_addr}").as_str())
        );
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].session_id.as_str(), "mac-xiling:sess-99");
        assert_eq!(
            snap.sessions[0].pane.as_ref().unwrap().as_str(),
            "mac-xiling:%4"
        );
        assert_eq!(
            snap.sessions[0].host_id.as_ref().unwrap().as_str(),
            "mac-xiling"
        );
        server.await.unwrap();
    }

    #[test]
    fn decode_chunked_parses_valid_and_rejects_truncated_streams() {
        let valid = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(valid).unwrap(), b"hello world");

        let truncated_no_zero = b"5\r\nhello\r\n";
        assert!(decode_chunked(truncated_no_zero).is_err());

        let truncated_mid = b"5\r\nhel";
        assert!(decode_chunked(truncated_mid).is_err());
    }

    #[tokio::test]
    async fn peer_provider_handles_chunked_response_and_missing_confidence_field() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // Hand-crafted older peer payload omitting `confidence`, `pane_resolution`, `state_since_ms`
            let body = serde_json::json!({
                "ok": true,
                "data": {
                    "sessions": [{
                        "session_id": "legacy-peer-1",
                        "agent": "claude",
                        "pane": "%7",
                        "state": "idle"
                    }]
                }
            })
            .to_string();
            let chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let cfg = NodeConfig {
            id: "old-peer".into(),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec![format!("http://{live_addr}")],
            ..NodeConfig::default()
        };
        let provider = PeerDaemonNodeProvider::new(&cfg);
        let snap = provider.sync("Old Peer").await.unwrap();
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].confidence, Confidence::Heuristic);
        server.await.unwrap();
    }
}
