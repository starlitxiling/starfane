//! Local multi-mux pool (`tmux` + `zellij`) and multi-host federation (`NodeProvider`).
//!
//! Supports mixing zero-install SSH ControlMaster hosts (`SshMuxNodeProvider`) and
//! full peer `starfaned` federation (`PeerDaemonNodeProvider` with multi-candidate
//! outbound endpoint failover & Split Ownership) on the same hub.

use crate::config::{NodeConfig, NodeKindConfig, NotifyOwnership, TeamConfig};
use crate::core::discover::{legacy_session_id, next_sequential_window_name};
use crate::core::metrics::{
    encode_claude_project_dir, epoch_i64_to_ms, extract_agy_effort_from_model,
    extract_codex_status_title, is_post_birth_timestamp, merge_metrics,
    parse_lsof_agy_presence_all, parse_ps_etime_secs, parse_status_line_metrics,
};
use crate::core::transcript::{
    parse_iso8601_ms, TranscriptIndexCache, TranscriptQuery, TranscriptResponse,
};
use crate::core::uploads::{detect_image_magic, MAX_IMAGE_BYTES};
use crate::core::util::sanitize_line;
use crate::notice::BoxFuture;
use crate::remote_mirror::{
    parse_codex_default_effort_toml, parse_sf_stat_line, RemoteFileStat, RemoteTranscriptMirror,
};
use serde::Serialize;
use serde_json::Value;
use starfane_proto::{
    id, AgentKind, ApiError, Confidence, DeliveryLevel, DeliveryReport, DeliveryStatus, Effort,
    ErrorCode, EventDetail, EventId, EventKind, GateCheck, GateReport, HostId, HostKind,
    HostRecord, HostStatus, NormalizedEvent, PaneId, PaneResolution, SessionId, SessionMetrics,
    SessionRecord, SessionState, LOCAL_HOST_ID, PROTO_VERSION,
};
use starfane_tmux::{
    adaptive_paste_settle_ms, cap_page_bytes, classify_panes_with_git_lookup,
    detect_blocking_startup_prompt, detect_codex_update_prompt, detect_folder_trust_prompt,
    is_paste_stuck_in_prompt, parse_panes, parse_zellij_pane_id, qualify_host_pane,
    qualify_host_session, shell_single_quote, trim_and_cap_tail, validate_interactive_text,
    CaptureRequest, ClientInfo, DiscoverOptions, DiscoveredPane, ExecTransport, FocusOutcome,
    GateConfig, PaneInfo, PanePreview, PaneStatus, SshExec, TerminalMux, Tmux, TmuxError,
    ZellijMux, MAX_CAPTURE_BYTES, MAX_CAPTURE_LINES, MAX_FULL_CAPTURE_BYTES,
    MAX_FULL_CAPTURE_LINES,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

const PEER_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_PEER_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
const SSH_SPLIT_MARKER: &str = "__SF_SSH_PS_SPLIT__";

const SEC_META: &str = "__SF_SEC_META__";
const SEC_PANES: &str = "__SF_SEC_PANES__";
const SEC_PS: &str = "__SF_SEC_PS__";
const SEC_TAILS: &str = "__SF_SEC_TAILS__";
const SEC_RESOLVE: &str = "__SF_SEC_RESOLVE__";
const SEC_TR_STATS: &str = "__SF_SEC_TR_STATS__";

const PF_META: &str = "__SF_PF_META__";
const PF_WINDOWS: &str = "__SF_PF_WINDOWS__";
const PF_TEAM_TOML: &str = "__SF_PF_TEAM_TOML__";
const PF_AGENT_CFG: &str = "__SF_PF_AGENT_CFG__";

const SSH_BG_MAX_CONCURRENT: usize = 4;
const SSH_INTERACTIVE_MAX_CONCURRENT: usize = 1;
const LEGACY_STABLE_TICKS: u32 = 3;
const LEGACY_SPAWN_GRACE_MS: u64 = 15_000;
const TAIL_CACHE_MAX_TICKS: u8 = 2;
const CLAUDE_PID_START_MAX_SKEW_MS: u64 = 120_000;
const RECENT_CLOSE_TOMBSTONE_MS: u64 = 5_000;
const MAX_STAGED_CACHE_ENTRIES: usize = 256;
const RTT_UNSET: u64 = u64::MAX;

/// Remote non-interactive `$PATH` bootstrap prepended to SSH POSIX `/bin/sh` scripts so
/// user-installed `claude`, `codex`, `agy`, and `tmux` binaries are found even when login
/// profiles are skipped.
const REMOTE_PATH_BOOTSTRAP: &str = r#"for _d in "$HOME/.local/bin" "$HOME/.cargo/bin" "$HOME/.npm-global/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
  [ -d "$_d" ] && case ":$PATH:" in *":$_d:"*) ;; *) PATH="$PATH:$_d" ;; esac
done
export PATH
"#;

/// Portable POSIX `stat` wrapper emitting `<inode> <size_bytes> <mtime_epoch_sec>` on both
/// GNU/Linux (`stat -c`) and macOS/BSD (`stat -f`).
const REMOTE_SF_STAT_FN: &str = r#"sf_stat() {
  stat -c '%i %s %Y' "$1" 2>/dev/null || stat -f '%i %z %m' "$1" 2>/dev/null || printf '0 -1 0\n'
}
"#;

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

    pub fn capture_visible_screen(&self, pane: &PaneId) -> Result<String, TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.capture_visible_screen(pane)
        } else {
            self.tmux.capture_visible_screen(pane)
        }
    }

    pub fn exit_copy_mode(&self, pane: &PaneId) -> Result<bool, TmuxError> {
        if Self::is_zellij_pane(pane) {
            Ok(false)
        } else {
            self.tmux.exit_copy_mode(pane)
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

    pub fn kill_pane(&self, pane: &PaneId) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.kill_pane(pane)
        } else {
            self.tmux.kill_pane(pane)
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
// Multi-host NodeProvider abstraction & HostCircuitBreaker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub host: HostRecord,
    pub sessions: Vec<SessionRecord>,
    /// Heuristic events produced during SSH discovery (`TurnComplete`, `NeedsPermission`, `NeedsInput`).
    pub events: Vec<NormalizedEvent>,
}

/// Request payload for spawning an interactive coding-agent session on a remote host.
#[derive(Debug, Clone)]
pub struct RemoteSpawnReq {
    pub agent: AgentKind,
    pub project_root: Option<String>,
    pub tmux_session: String,
    pub window_name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub prompt: Option<String>,
    pub known_host_projects: Vec<String>,
}

/// Result of spawning an interactive coding-agent session on a remote host.
#[derive(Debug, Clone)]
pub struct RemoteSpawnOutcome {
    pub session: SessionRecord,
    pub response: serde_json::Value,
}

/// Outcome of an explicit immediate host probe (`POST /api/hosts/{id}/probe`).
#[derive(Debug, Clone, Serialize)]
pub struct HostProbeOutcome {
    pub host_id: HostId,
    pub status: HostStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing)]
    pub snapshot: Option<NodeSnapshot>,
}

/// Per-host circuit breaker isolating flaky or unreachable WAN/VPN hosts:
/// - `Online` (`0` consecutive failures)
/// - `Degraded` (`1..=2` failures: backoff `5s -> 10s`)
/// - `Offline` (`>= 3` failures: exponential backoff `15s -> 30s -> 60s` with ±10% jitter;
///   background reads fast-fail in 0ms until `next_retry_ms` or explicit `reset_for_probe`)
#[derive(Debug, Clone)]
pub struct HostCircuitBreaker {
    host_hash: u64,
    consecutive_failures: u32,
    next_retry_ms: Option<u64>,
    last_error: Option<String>,
}

impl HostCircuitBreaker {
    pub fn new(host_id: &str) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        host_id.hash(&mut h);
        Self {
            host_hash: h.finish(),
            consecutive_failures: 0,
            next_retry_ms: None,
            last_error: None,
        }
    }

    pub fn status(&self) -> HostStatus {
        match self.consecutive_failures {
            0 => HostStatus::Online,
            1..=2 => HostStatus::Degraded,
            _ => HostStatus::Offline,
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn next_retry_ms(&self) -> Option<u64> {
        self.next_retry_ms
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.next_retry_ms = None;
        self.last_error = None;
    }

    pub fn record_failure(&mut self, now_ms: u64, err: impl Into<String>) -> (HostStatus, u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(err.into());
        let delay_ms = self.compute_backoff_ms(now_ms, self.consecutive_failures);
        let retry_at = now_ms.saturating_add(delay_ms);
        self.next_retry_ms = Some(retry_at);
        (self.status(), retry_at)
    }

    fn compute_backoff_ms(&self, now_ms: u64, failures: u32) -> u64 {
        match failures {
            0 => 0,
            1 => 5_000,
            2 => 10_000,
            n => {
                let base_ms: u64 = match n {
                    3 => 15_000,
                    4 => 30_000,
                    _ => 60_000,
                };
                // Deterministic ±10% jitter (`900..=1100` thousandths) without external crates.
                let mix = now_ms.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    ^ self.host_hash
                    ^ (u64::from(n)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                let mixed = (mix ^ (mix >> 30)).wrapping_mul(0x94d0_49bb_1331_11eb);
                let factor_milli = 900 + ((mixed ^ (mixed >> 31)) % 201);
                base_ms.saturating_mul(factor_milli) / 1_000
            }
        }
    }

    /// Clear the retry gate so an explicit user `probe_now` executes immediately.
    pub fn reset_for_probe(&mut self) {
        self.next_retry_ms = None;
    }

    /// True if a scheduled background sync should be skipped because the backoff window is active.
    pub fn active_backoff_until(&self, now_ms: u64) -> Option<u64> {
        self.next_retry_ms.filter(|&until| now_ms < until)
    }

    /// Fast-fail silent background reads (`capture_pane`, `fetch_transcript`, `list_tmux_sessions`)
    /// in 0ms when the host circuit is `Offline` and still inside its cooldown window.
    pub fn fast_fail_if_offline(&self, host_id: &HostId, now_ms: u64) -> Result<(), ApiError> {
        if self.status() == HostStatus::Offline {
            if let Some(until) = self.active_backoff_until(now_ms) {
                let rem_secs = until.saturating_sub(now_ms).div_ceil(1_000);
                return Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("host {host_id} is offline (retry in {rem_secs}s)"),
                ));
            }
        }
        Ok(())
    }
}

pub trait NodeProvider: Send + Sync {
    fn host_id(&self) -> &HostId;
    fn kind(&self) -> HostKind;
    fn default_display_name(&self) -> &str;
    fn notify_ownership(&self) -> NotifyOwnership;

    fn sync_with_claimed<'a>(
        &'a self,
        display_name: &'a str,
        claimed_external_ids: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<NodeSnapshot, String>>;

    fn sync<'a>(&'a self, display_name: &'a str) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        Box::pin(async move {
            let empty = HashSet::new();
            self.sync_with_claimed(display_name, &empty).await
        })
    }

    fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>>;

    fn spawn_session<'a>(
        &'a self,
        display_name: &'a str,
        req: &'a RemoteSpawnReq,
    ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>>;

    fn stage_input_files<'a>(
        &'a self,
        files: &'a [(String, PathBuf)],
    ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>>;

    fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>>;

    fn rtt_ms(&self) -> Option<u64>;

    fn next_retry_ms(&self) -> Option<u64>;

    fn circuit_status(&self) -> HostStatus;

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

    fn close_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>>;
}

pub fn build_remote_providers(
    nodes: &[NodeConfig],
    state_dir: &Path,
) -> HashMap<HostId, Arc<dyn NodeProvider>> {
    let shared_idx = Arc::new(Mutex::new(TranscriptIndexCache::default()));
    build_remote_providers_with_transcript_cache(nodes, state_dir, shared_idx)
}

pub fn build_remote_providers_with_transcript_cache(
    nodes: &[NodeConfig],
    state_dir: &Path,
    transcript_index: Arc<Mutex<TranscriptIndexCache>>,
) -> HashMap<HostId, Arc<dyn NodeProvider>> {
    let shared_mirror = Arc::new(RemoteTranscriptMirror::new(state_dir.to_path_buf()));
    let mut map: HashMap<HostId, Arc<dyn NodeProvider>> = HashMap::new();
    for cfg in nodes {
        if !cfg.enabled {
            continue;
        }
        let host_id = HostId::new(cfg.id.trim());
        if host_id.is_empty() || host_id.as_str() == LOCAL_HOST_ID {
            continue;
        }
        let provider: Arc<dyn NodeProvider> = match cfg.kind {
            NodeKindConfig::Ssh => Arc::new(SshMuxNodeProvider::with_shared_caches(
                cfg,
                state_dir,
                shared_mirror.clone(),
                transcript_index.clone(),
            )),
            NodeKindConfig::Peer => Arc::new(PeerDaemonNodeProvider::new(cfg)),
        };
        map.insert(host_id, provider);
    }
    map
}

// ---------------------------------------------------------------------------
// Remote SSH Discovery State, V2 Compound Script & Remote-Clock Isolation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SshTailCacheEntry {
    history_size: u32,
    cursor_x: u32,
    cursor_y: u32,
    in_mode: bool,
    current_command: String,
    ticks_cached: u8,
    tail: String,
}

#[derive(Debug, Clone)]
struct SshPaneWatch {
    last_hash: u64,
    stable_ticks: u32,
    state: SessionState,
    state_since_ms: u64,
    last_event_ms: u64,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct SshSpawnedPaneMeta {
    remote_spawned_at_ms: u64,
    hub_spawned_at_ms: u64,
    agent: AgentKind,
    cwd: String,
    git_root: Option<String>,
    tmux_target: String,
    tmux_label: String,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedRemoteTranscript {
    path: String,
    external_id: Option<String>,
    authoritative: bool,
    last_stat: Option<RemoteFileStat>,
    metrics: Option<SessionMetrics>,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteSessionLookup {
    agent: AgentKind,
    remote_path: String,
    latest_stat: Option<RemoteFileStat>,
    metrics: Option<SessionMetrics>,
}

#[derive(Debug, Clone)]
struct RemoteClaudePidInfo {
    pid: u32,
    session_id: String,
    cwd: Option<String>,
    started_at_ms: Option<u64>,
    file_mtime_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct RemoteCodexThreadRow {
    id: String,
    rollout_path: String,
    cwd: String,
    name: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone)]
struct RemoteCodexRolloutCandidate {
    id: String,
    cwd: String,
    timestamp_ms: Option<u64>,
    path: String,
    stat: RemoteFileStat,
}

#[derive(Debug, Clone)]
struct RemoteAgyRootRow {
    conversation_id: String,
    workspace_uris: String,
    status: String,
    last_modified_time: String,
    last_user_input_time: String,
    step_count: i64,
}

#[derive(Debug, Clone)]
struct RemoteAgyChildRow {
    conversation_id: String,
    parent_conversation_id: String,
    agent_name: String,
    status: String,
    killed: i64,
}

#[derive(Debug, Default)]
struct SshDiscoveryState {
    remote_home: Option<String>,
    /// OS process birth timestamps strictly in the **Remote Host Clock Domain** (`remote_now_ms - etime`).
    pid_birth_remote_ms: HashMap<u32, u64>,
    pane_agent_pid: HashMap<PaneId, u32>,
    pane_agent: HashMap<PaneId, AgentKind>,
    pane_first_seen_remote_ms: HashMap<PaneId, u64>,
    pane_birth_remote_ms: HashMap<PaneId, u64>,
    spawned_panes: HashMap<PaneId, SshSpawnedPaneMeta>,
    tail_cache: HashMap<PaneId, SshTailCacheEntry>,
    pane_watch: HashMap<PaneId, SshPaneWatch>,
    resolved_by_pane: HashMap<PaneId, ResolvedRemoteTranscript>,
    /// Lookup from unqualified remote `SessionId` (`tmux-%N`, subagent conv id, etc.) -> remote transcript info.
    session_lookup: HashMap<SessionId, RemoteSessionLookup>,
    /// Cached remote SQLite DB signatures & rows to skip re-running `sqlite3` on unchanged DBs.
    agy_db_sig: Option<String>,
    agy_roots: Vec<RemoteAgyRootRow>,
    agy_children: Vec<RemoteAgyChildRow>,
    codex_db_sig: Option<String>,
    codex_threads: Vec<RemoteCodexThreadRow>,
    /// Brief tombstone set populated on `close_pane` so an in-flight `sync` cannot re-insert a closed pane.
    recently_closed_panes: HashMap<PaneId, u64>,
    /// Latest stat map from `__SF_SEC_TR_STATS__`.
    latest_tr_stats: HashMap<String, RemoteFileStat>,
}

impl SshDiscoveryState {
    fn evict_pane(&mut self, local_pane: &PaneId, now_hub_ms: u64) {
        self.pane_agent_pid.remove(local_pane);
        self.pane_agent.remove(local_pane);
        self.pane_first_seen_remote_ms.remove(local_pane);
        self.pane_birth_remote_ms.remove(local_pane);
        self.spawned_panes.remove(local_pane);
        self.tail_cache.remove(local_pane);
        self.pane_watch.remove(local_pane);
        self.resolved_by_pane.remove(local_pane);
        let leg = legacy_session_id(local_pane);
        self.session_lookup.remove(&leg);
        self.recently_closed_panes
            .insert(local_pane.clone(), now_hub_ms);
    }

    fn cached_tail_fingerprints(&self) -> Vec<String> {
        self.tail_cache
            .iter()
            .filter(|(_, e)| e.ticks_cached < TAIL_CACHE_MAX_TICKS)
            .map(|(pane, e)| {
                format!(
                    "{}|{}|{}|{}|{}|{}",
                    pane.as_str(),
                    e.history_size,
                    e.cursor_x,
                    e.cursor_y,
                    u8::from(e.in_mode),
                    e.current_command
                )
            })
            .collect()
    }

    fn known_transcript_paths(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for r in self.resolved_by_pane.values() {
            if seen.insert(r.path.clone()) {
                paths.push(r.path.clone());
            }
        }
        for l in self.session_lookup.values() {
            if seen.insert(l.remote_path.clone()) {
                paths.push(l.remote_path.clone());
            }
        }
        paths
    }
}

#[derive(Debug, Default)]
struct StagedFileCache {
    entries: HashMap<(String, u64, u64), String>,
    order: VecDeque<(String, u64, u64)>,
}

impl StagedFileCache {
    fn get(&self, key: &(String, u64, u64)) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    fn insert(&mut self, key: (String, u64, u64), remote_path: String) {
        if let Some(slot) = self.entries.get_mut(&key) {
            *slot = remote_path;
            return;
        }
        while self.entries.len() >= MAX_STAGED_CACHE_ENTRIES {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, remote_path);
    }
}

/// Parsed structured output of `compound_discover_script_v2`.
#[derive(Debug, Default)]
pub struct ParsedSshDiscoveryV2 {
    pub remote_now_ms: u64,
    pub remote_home: String,
    pub panes: Vec<PaneInfo>,
    pub ps_for_classify: String,
    pub pid_birth_remote_ms: HashMap<u32, u64>,
    pub live_remote_pids: HashSet<u32>,
    pub captured_tails: HashMap<PaneId, String>,
    claude_sessions: Vec<RemoteClaudePidInfo>,
    agy_locks_by_pid: HashMap<u32, Vec<String>>,
    agy_db_sig: Option<String>,
    agy_roots: Option<Vec<RemoteAgyRootRow>>,
    agy_children: Option<Vec<RemoteAgyChildRow>>,
    codex_resume_paths: HashMap<String, String>,
    codex_db_sig: Option<String>,
    codex_threads: Option<Vec<RemoteCodexThreadRow>>,
    codex_rollouts: Vec<RemoteCodexRolloutCandidate>,
    pub tr_stats: HashMap<String, RemoteFileStat>,
}

/// Parse `LC_ALL=C ps -eo pid=,ppid=,etime=,args=` into:
/// 1. `pid ppid args` text compatible with `starfane_tmux::classify_panes_with_git_lookup`,
/// 2. `pid -> birth_remote_ms` computed strictly in the remote host clock domain,
/// 3. full set of live remote PIDs for dead-PID pruning.
fn parse_ps_with_etime(
    ps_raw: &str,
    remote_now_ms: u64,
) -> (String, HashMap<u32, u64>, HashSet<u32>) {
    let mut normalized = String::with_capacity(ps_raw.len());
    let mut births = HashMap::new();
    let mut live_pids = HashSet::new();
    for line in ps_raw.lines() {
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
        let Some(tok3) = parts.next() else {
            continue;
        };
        live_pids.insert(pid);
        if let Some(etime_secs) = parse_ps_etime_secs(tok3) {
            if remote_now_ms > 0 {
                births.insert(
                    pid,
                    remote_now_ms.saturating_sub(etime_secs.saturating_mul(1_000)),
                );
            }
            let args = after_n_tokens(trimmed, 3);
            if !args.is_empty() {
                normalized.push_str(&format!("{pid} {ppid} {args}\n"));
            }
        } else {
            // Backward-compatible fallback if a synthetic line omitted `etime`.
            let args = after_n_tokens(trimmed, 2);
            if !args.is_empty() {
                normalized.push_str(&format!("{pid} {ppid} {args}\n"));
            }
        }
    }
    (normalized, births, live_pids)
}

fn after_n_tokens(s: &str, n: usize) -> &str {
    let mut idx = 0;
    let bytes = s.as_bytes();
    for _ in 0..n {
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

fn split_marker_line_first<'a>(src: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let needle = format!("\n{marker}\n");
    if let Some(rest) = src.strip_prefix(&format!("{marker}\n")) {
        return Some(("", rest));
    }
    if let Some((before, after)) = src.split_once(&needle) {
        return Some((before, after));
    }
    let tail_needle = format!("\n{marker}");
    if let Some(before) = src.strip_suffix(&tail_needle) {
        return Some((before, ""));
    }
    None
}

fn split_marker_line_last<'a>(src: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let needle = format!("\n{marker}\n");
    if let Some((before, after)) = src.rsplit_once(&needle) {
        return Some((before, after));
    }
    let tail_needle = format!("\n{marker}");
    if let Some(before) = src.strip_suffix(&tail_needle) {
        return Some((before, ""));
    }
    if let Some(rest) = src.strip_prefix(&format!("{marker}\n")) {
        return Some(("", rest));
    }
    None
}

fn parse_stat_and_path(line: &str) -> Option<(RemoteFileStat, String)> {
    let (stat, path) = parse_sf_stat_line(line)?;
    Some((stat, path.to_string()))
}

fn extract_presence_lock_uuid(path: &str) -> Option<String> {
    let p = path.trim();
    if !p.contains("/presence/") || !p.ends_with(".lock") {
        return None;
    }
    let stem = Path::new(p).file_stem()?.to_str()?;
    if stem.len() >= 16
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Some(stem.to_string())
    } else {
        None
    }
}

/// Parse all 6 deterministic sections of `compound_discover_script_v2`.
///
/// Uses forward splitting for `META -> PANES -> PS -> TAILS` and reverse (`rsplit_once`)
/// splitting for `TR_STATS` and `RESOLVE` so terminal tail captures containing literal marker
/// strings can never corrupt section boundaries.
pub fn parse_ssh_compound_v2_output(
    stdout: &str,
    fallback_now_ms: u64,
) -> Result<ParsedSshDiscoveryV2, String> {
    let clean = stdout.replace("\r\n", "\n");
    let (_, after_meta) = split_marker_line_first(&clean, SEC_META)
        .ok_or_else(|| "missing __SF_SEC_META__ marker".to_string())?;
    let (meta_sec, after_panes) = split_marker_line_first(after_meta, SEC_PANES)
        .ok_or_else(|| "missing __SF_SEC_PANES__ marker".to_string())?;
    let (panes_sec, after_ps) = split_marker_line_first(after_panes, SEC_PS)
        .ok_or_else(|| "missing __SF_SEC_PS__ marker".to_string())?;
    let (ps_sec, after_tails) = split_marker_line_first(after_ps, SEC_TAILS)
        .ok_or_else(|| "missing __SF_SEC_TAILS__ marker".to_string())?;
    let (before_tr_stats, tr_stats_sec) = split_marker_line_last(after_tails, SEC_TR_STATS)
        .ok_or_else(|| "missing __SF_SEC_TR_STATS__ marker".to_string())?;
    let (tails_sec, resolve_sec) = split_marker_line_last(before_tr_stats, SEC_RESOLVE)
        .ok_or_else(|| "missing __SF_SEC_RESOLVE__ marker".to_string())?;

    let mut out = ParsedSshDiscoveryV2::default();
    let mut remote_now_s: Option<u64> = None;
    for line in meta_sec.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("REMOTE_NOW_S=") {
            if let Ok(sec) = v.trim().parse::<u64>() {
                if sec > 0 {
                    remote_now_s = Some(sec);
                }
            }
        } else if let Some(v) = t.strip_prefix("REMOTE_HOME=") {
            out.remote_home = v.trim().to_string();
        }
    }
    out.remote_now_ms = remote_now_s
        .map(|s| s.saturating_mul(1_000))
        .unwrap_or(fallback_now_ms);

    out.panes = parse_panes(panes_sec).map_err(|e| e.to_string())?;
    let (ps_norm, pid_births, live_pids) = parse_ps_with_etime(ps_sec, out.remote_now_ms);
    out.ps_for_classify = ps_norm;
    out.pid_birth_remote_ms = pid_births;
    out.live_remote_pids = live_pids;

    // Parse captured pane tails (`__SF_TAIL_BEGIN__ %N` .. `__SF_TAIL_END__ %N`).
    let mut cur_pane: Option<PaneId> = None;
    let mut cur_buf = String::new();
    for line in tails_sec.lines() {
        if let Some(pid_raw) = line.strip_prefix("__SF_TAIL_BEGIN__ ") {
            cur_pane = Some(PaneId::new(pid_raw.trim()));
            cur_buf.clear();
        } else if let Some(pid_raw) = line.strip_prefix("__SF_TAIL_END__ ") {
            if let Some(p) = cur_pane.take() {
                if p.as_str() == pid_raw.trim() {
                    out.captured_tails
                        .insert(p, trim_and_cap_tail(&cur_buf, 20, MAX_CAPTURE_BYTES));
                }
            }
            cur_buf.clear();
        } else if cur_pane.is_some() {
            if !cur_buf.is_empty() {
                cur_buf.push('\n');
            }
            cur_buf.push_str(line);
        }
    }

    // Parse `__SF_SEC_RESOLVE__`.
    parse_resolve_section(resolve_sec, &mut out);

    // Parse `__SF_SEC_TR_STATS__`.
    for line in tr_stats_sec.lines() {
        if let Some((stat, path)) = parse_stat_and_path(line) {
            out.tr_stats.insert(path, stat);
        }
    }

    Ok(out)
}

fn parse_resolve_section(sec: &str, out: &mut ParsedSshDiscoveryV2) {
    let lines: Vec<&str> = sec.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("CLAUDE_SESS ") {
            let mut cols = rest.split_whitespace();
            let pid_opt = cols.next().and_then(|s| s.parse::<u32>().ok());
            let _ino = cols.next();
            let _sz = cols.next();
            let mtime_s = cols.next().and_then(|s| s.parse::<i64>().ok());
            let json_line = lines.get(i + 1).copied().unwrap_or("");
            i += 2;
            if let Some(pid) = pid_opt {
                if let Ok(v) = serde_json::from_str::<Value>(json_line.trim()) {
                    let rec_pid_ok = v
                        .get("pid")
                        .and_then(Value::as_u64)
                        .is_none_or(|rp| rp == u64::from(pid));
                    if rec_pid_ok {
                        if let Some(sid) = v
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty() && !s.contains('/') && !s.contains(".."))
                        {
                            let cwd = v
                                .get("cwd")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string);
                            let started_at_ms = v
                                .get("startedAt")
                                .and_then(Value::as_i64)
                                .and_then(epoch_i64_to_ms);
                            let file_mtime_ms = mtime_s.and_then(epoch_i64_to_ms);
                            out.claude_sessions.push(RemoteClaudePidInfo {
                                pid,
                                session_id: sid.to_string(),
                                cwd,
                                started_at_ms,
                                file_mtime_ms,
                            });
                        }
                    }
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("AGY_LOCK ") {
            let mut cols = rest.split_whitespace();
            if let Some(pid) = cols.next().and_then(|s| s.parse::<u32>().ok()) {
                let path = after_n_tokens(rest.trim(), 1);
                if let Some(uuid) = extract_presence_lock_uuid(path) {
                    let entry = out.agy_locks_by_pid.entry(pid).or_default();
                    if !entry.iter().any(|x| x == &uuid) {
                        entry.push(uuid);
                    }
                }
            }
            i += 1;
            continue;
        }
        if line.trim() == "AGY_LSOF_BEGIN" {
            let mut block = String::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "AGY_LSOF_END" {
                block.push_str(lines[i]);
                block.push('\n');
                i += 1;
            }
            let parsed = parse_lsof_agy_presence_all(&block);
            for (pid, cids) in parsed {
                let entry = out.agy_locks_by_pid.entry(pid).or_default();
                for c in cids {
                    if !entry.iter().any(|x| x == &c) {
                        entry.push(c);
                    }
                }
            }
            i += 1;
            continue;
        }
        if let Some(sig) = line.strip_prefix("AGY_DB_SIG ") {
            let s = sig.trim();
            if !s.is_empty() {
                out.agy_db_sig = Some(s.to_string());
            }
            i += 1;
            continue;
        }
        if line.trim() == "AGY_DB_ROOTS_BEGIN" {
            let mut rows = Vec::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "AGY_DB_ROOTS_END" {
                let raw = lines[i];
                if !raw.trim().is_empty() {
                    let parts: Vec<&str> = raw.split(starfane_tmux::FIELD_SEP).collect();
                    if parts.len() >= 6 {
                        let cid = parts[0].trim();
                        if !cid.is_empty() && !cid.contains('/') && !cid.contains("..") {
                            rows.push(RemoteAgyRootRow {
                                conversation_id: cid.to_string(),
                                workspace_uris: parts[1].to_string(),
                                status: parts[2].trim().to_string(),
                                last_modified_time: parts[3].trim().to_string(),
                                last_user_input_time: parts[4].trim().to_string(),
                                step_count: parts[5].trim().parse::<i64>().unwrap_or(0),
                            });
                        }
                    }
                }
                i += 1;
            }
            out.agy_roots = Some(rows);
            i += 1;
            continue;
        }
        if line.trim() == "AGY_DB_CHILDREN_BEGIN" {
            let mut rows = Vec::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "AGY_DB_CHILDREN_END" {
                let raw = lines[i];
                if !raw.trim().is_empty() {
                    let parts: Vec<&str> = raw.split(starfane_tmux::FIELD_SEP).collect();
                    if parts.len() >= 5 {
                        let cid = parts[0].trim();
                        if !cid.is_empty() && !cid.contains('/') && !cid.contains("..") {
                            rows.push(RemoteAgyChildRow {
                                conversation_id: cid.to_string(),
                                parent_conversation_id: parts[1].trim().to_string(),
                                agent_name: parts[2].trim().to_string(),
                                status: parts[3].trim().to_string(),
                                killed: parts[4].trim().parse::<i64>().unwrap_or(0),
                            });
                        }
                    }
                }
                i += 1;
            }
            out.agy_children = Some(rows);
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("CODEX_RESUME ") {
            let trimmed = rest.trim();
            if let Some(uuid) = trimmed.split_whitespace().next() {
                let path = after_n_tokens(trimmed, 1).trim();
                if !uuid.is_empty() && !path.is_empty() {
                    out.codex_resume_paths
                        .insert(uuid.to_string(), path.to_string());
                }
            }
            i += 1;
            continue;
        }
        if let Some(sig) = line.strip_prefix("CODEX_DB_SIG ") {
            let s = sig.trim();
            if !s.is_empty() {
                out.codex_db_sig = Some(s.to_string());
            }
            i += 1;
            continue;
        }
        if line.trim() == "CODEX_DB_BEGIN" {
            let mut rows = Vec::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "CODEX_DB_END" {
                let raw = lines[i];
                if !raw.trim().is_empty() {
                    let parts: Vec<&str> = raw.split(starfane_tmux::FIELD_SEP).collect();
                    if parts.len() >= 6 {
                        let id = parts[0].trim();
                        let rp = parts[1].trim();
                        if !id.is_empty() && !rp.is_empty() {
                            let nm = parts[3].trim();
                            rows.push(RemoteCodexThreadRow {
                                id: id.to_string(),
                                rollout_path: rp.to_string(),
                                cwd: parts[2].trim().to_string(),
                                name: (!nm.is_empty()).then(|| nm.to_string()),
                                created_at: parts[4].trim().parse::<i64>().unwrap_or(0),
                                updated_at: parts[5].trim().parse::<i64>().unwrap_or(0),
                            });
                        }
                    }
                }
                i += 1;
            }
            out.codex_threads = Some(rows);
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("CODEX_ROLLOUT ") {
            let hdr_line = lines.get(i + 1).copied().unwrap_or("");
            i += 2;
            if let Some((stat, path)) = parse_stat_and_path(rest) {
                if let Some((cid, cwd, ts_ms)) = parse_codex_session_meta_line(hdr_line) {
                    out.codex_rollouts.push(RemoteCodexRolloutCandidate {
                        id: cid,
                        cwd,
                        timestamp_ms: ts_ms,
                        path,
                        stat,
                    });
                }
            }
            continue;
        }
        i += 1;
    }
}

/// Pure parser for a Codex rollout JSONL line 1 (`session_meta` `{id, timestamp, cwd}`).
fn parse_codex_session_meta_line(line: &str) -> Option<(String, String, Option<u64>)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let obj: Value = serde_json::from_str(trimmed).ok()?;
    let payload = obj.get("payload").unwrap_or(&obj);
    let id = payload
        .get("id")
        .or_else(|| obj.get("id"))
        .and_then(Value::as_str)?
        .trim();
    if id.is_empty() || id.contains('/') || id.contains("..") {
        return None;
    }
    let cwd = payload
        .get("cwd")
        .or_else(|| obj.get("cwd"))
        .and_then(Value::as_str)?
        .trim();
    if cwd.is_empty() {
        return None;
    }
    let ts_ms = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_ms)
        .or_else(|| {
            obj.get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_iso8601_ms)
        });
    Some((id.to_string(), cwd.to_string(), ts_ms))
}

fn same_remote_dir(a: &str, b: &str) -> bool {
    let na = a.trim().trim_end_matches('/');
    let nb = b.trim().trim_end_matches('/');
    !na.is_empty() && !nb.is_empty() && na == nb
}

fn hash_tail(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.contains("← for agents") || t.contains("? for shortcuts") {
            continue;
        }
        t.hash(&mut h);
        b'\n'.hash(&mut h);
    }
    h.finish()
}

/// Apply heuristic tail-hash state transition (`Idle <-> Working -> TurnComplete`,
/// `NeedsPermission` on folder trust prompts, `NeedsInput` on Codex update prompts)
/// for a remote SSH pane, mirroring `core/discover.rs::apply_legacy_tail_hash`.
#[allow(clippy::too_many_arguments)]
fn apply_ssh_pane_tail_watch(
    watch_map: &mut HashMap<PaneId, SshPaneWatch>,
    local_pane: &PaneId,
    qualified_sid: &SessionId,
    qualified_pane: &PaneId,
    agent: AgentKind,
    cwd: Option<&str>,
    model: Option<&str>,
    tail: &str,
    initial_state: SessionState,
    now_ms: u64,
) -> (
    SessionState,
    u64,
    u64,
    Option<String>,
    Option<NormalizedEvent>,
) {
    let h = hash_tail(tail);
    let entry = watch_map
        .entry(local_pane.clone())
        .or_insert_with(|| SshPaneWatch {
            last_hash: h,
            stable_ticks: 0,
            state: initial_state,
            state_since_ms: now_ms,
            last_event_ms: now_ms,
            last_error: None,
        });

    if let Some(reason) = detect_folder_trust_prompt(agent, tail) {
        entry.last_hash = h;
        entry.stable_ticks = 0;
        let changed = entry.state != SessionState::NeedsPermission
            || entry.last_error.as_deref() != Some(reason);
        let ev = if changed {
            entry.state = SessionState::NeedsPermission;
            entry.last_error = Some(reason.to_string());
            entry.state_since_ms = now_ms;
            entry.last_event_ms = now_ms;
            Some(NormalizedEvent {
                v: PROTO_VERSION,
                id: EventId::new(id::new_id(id::prefix::EVENT)),
                ts_ms: now_ms,
                agent,
                kind: EventKind::NeedsPermission,
                confidence: Confidence::Heuristic,
                session_id: qualified_sid.clone(),
                subagent_id: None,
                pane: Some(qualified_pane.clone()),
                cwd: cwd.map(str::to_string),
                model: model.map(str::to_string),
                native_event: "tmux_trust_prompt_heuristic".into(),
                transcript_path: None,
                bind_token: None,
                detail: EventDetail {
                    error: Some(reason.to_string()),
                    ..EventDetail::default()
                },
            })
        } else {
            None
        };
        return (
            entry.state,
            entry.state_since_ms,
            entry.last_event_ms,
            entry.last_error.clone(),
            ev,
        );
    }

    if let Some(reason) = detect_codex_update_prompt(agent, tail) {
        entry.last_hash = h;
        entry.stable_ticks = 0;
        let changed =
            entry.state != SessionState::NeedsInput || entry.last_error.as_deref() != Some(reason);
        let ev = if changed {
            entry.state = SessionState::NeedsInput;
            entry.last_error = Some(reason.to_string());
            entry.state_since_ms = now_ms;
            entry.last_event_ms = now_ms;
            Some(NormalizedEvent {
                v: PROTO_VERSION,
                id: EventId::new(id::new_id(id::prefix::EVENT)),
                ts_ms: now_ms,
                agent,
                kind: EventKind::NeedsInput,
                confidence: Confidence::Heuristic,
                session_id: qualified_sid.clone(),
                subagent_id: None,
                pane: Some(qualified_pane.clone()),
                cwd: cwd.map(str::to_string),
                model: model.map(str::to_string),
                native_event: "tmux_startup_modal_heuristic".into(),
                transcript_path: None,
                bind_token: None,
                detail: EventDetail {
                    error: Some(reason.to_string()),
                    ..EventDetail::default()
                },
            })
        } else {
            None
        };
        return (
            entry.state,
            entry.state_since_ms,
            entry.last_event_ms,
            entry.last_error.clone(),
            ev,
        );
    }

    if entry.last_hash != h {
        entry.last_hash = h;
        entry.stable_ticks = 0;
        if matches!(
            entry.state,
            SessionState::NeedsPermission | SessionState::NeedsInput
        ) {
            entry.last_error = None;
        }
        if entry.state != SessionState::Working {
            entry.state = SessionState::Working;
            entry.state_since_ms = now_ms;
        }
        entry.last_event_ms = now_ms;
        return (
            entry.state,
            entry.state_since_ms,
            entry.last_event_ms,
            entry.last_error.clone(),
            None,
        );
    }

    entry.stable_ticks = entry.stable_ticks.saturating_add(1);
    let mut ev = None;
    if entry.stable_ticks >= LEGACY_STABLE_TICKS {
        if entry.state == SessionState::Working {
            entry.state = SessionState::Idle;
            entry.state_since_ms = now_ms;
            entry.last_event_ms = now_ms;
            if !agent.is_shell() {
                ev = Some(NormalizedEvent {
                    v: PROTO_VERSION,
                    id: EventId::new(id::new_id(id::prefix::EVENT)),
                    ts_ms: now_ms,
                    agent,
                    kind: EventKind::TurnComplete,
                    confidence: Confidence::Heuristic,
                    session_id: qualified_sid.clone(),
                    subagent_id: None,
                    pane: Some(qualified_pane.clone()),
                    cwd: cwd.map(str::to_string),
                    model: model.map(str::to_string),
                    native_event: "tmux_idle_heuristic".into(),
                    transcript_path: None,
                    bind_token: None,
                    detail: EventDetail::default(),
                });
            }
        } else if matches!(
            entry.state,
            SessionState::Starting | SessionState::NeedsPermission | SessionState::NeedsInput
        ) {
            entry.state = SessionState::Idle;
            entry.last_error = None;
            entry.state_since_ms = now_ms;
            entry.last_event_ms = now_ms;
        }
    }
    (
        entry.state,
        entry.state_since_ms,
        entry.last_event_ms,
        entry.last_error.clone(),
        ev,
    )
}

struct SshReconcileResult {
    sessions: Vec<SessionRecord>,
    events: Vec<NormalizedEvent>,
    need_mirror: Vec<(PaneId, AgentKind, String, RemoteFileStat)>,
}

/// Reconcile a parsed `compound_discover_script_v2` snapshot against `SshDiscoveryState`
/// using strictly remote-host clock birth-time math, dead-PID pruning, PID-restart scrubbing,
/// `LEGACY_SPAWN_GRACE_MS` protection, and strict `claimed_external_ids` + `unique_in_cwd` isolation.
fn reconcile_ssh_discovery_v2(
    state: &mut SshDiscoveryState,
    host_id: &HostId,
    display_name: &str,
    parsed: ParsedSshDiscoveryV2,
    include_shell_panes: bool,
    claimed_external_ids: &HashSet<String>,
    now_hub_ms: u64,
) -> SshReconcileResult {
    if !parsed.remote_home.is_empty() {
        state.remote_home = Some(parsed.remote_home.clone());
    }
    let remote_home = state.remote_home.clone().unwrap_or_default();
    let remote_now_ms = parsed.remote_now_ms;

    // Update cached remote SQLite tables when remote DB `(ino, size, mtime)` changed.
    if let Some(sig) = parsed.agy_db_sig {
        state.agy_db_sig = Some(sig);
    }
    if let Some(roots) = parsed.agy_roots {
        state.agy_roots = roots;
    }
    if let Some(children) = parsed.agy_children {
        state.agy_children = children;
    }
    if let Some(sig) = parsed.codex_db_sig {
        state.codex_db_sig = Some(sig);
    }
    if let Some(threads) = parsed.codex_threads {
        state.codex_threads = threads;
    }
    state.latest_tr_stats = parsed.tr_stats;

    // Prune dead remote PIDs so recycled OS PIDs on the remote host never reuse stale birth times.
    state
        .pid_birth_remote_ms
        .retain(|pid, _| parsed.live_remote_pids.contains(pid));
    for (pid, birth_ms) in parsed.pid_birth_remote_ms {
        state.pid_birth_remote_ms.entry(pid).or_insert(birth_ms);
    }

    // Expire old close tombstones.
    state
        .recently_closed_panes
        .retain(|_, ts| now_hub_ms.saturating_sub(*ts) < RECENT_CLOSE_TOMBSTONE_MS);

    let live_panes: HashMap<PaneId, &PaneInfo> = parsed
        .panes
        .iter()
        .filter(|p| !p.dead && !state.recently_closed_panes.contains_key(&p.pane_id))
        .map(|p| (p.pane_id.clone(), p))
        .collect();
    let live_pane_ids: HashSet<PaneId> = live_panes.keys().cloned().collect();

    // Evict dead/disappeared panes from all per-pane caches.
    state
        .pane_agent_pid
        .retain(|p, _| live_pane_ids.contains(p));
    state.pane_agent.retain(|p, _| live_pane_ids.contains(p));
    state
        .pane_first_seen_remote_ms
        .retain(|p, _| live_pane_ids.contains(p));
    state
        .pane_birth_remote_ms
        .retain(|p, _| live_pane_ids.contains(p));
    state.spawned_panes.retain(|p, _| live_pane_ids.contains(p));
    state.tail_cache.retain(|p, _| live_pane_ids.contains(p));
    state.pane_watch.retain(|p, _| live_pane_ids.contains(p));
    state
        .resolved_by_pane
        .retain(|p, _| live_pane_ids.contains(p));

    let discovered: Vec<DiscoveredPane> = classify_panes_with_git_lookup(
        &parsed.panes,
        &parsed.ps_for_classify,
        DiscoverOptions {
            include_shell_panes,
        },
        false,
    )
    .into_iter()
    .filter(|dp| !state.recently_closed_panes.contains_key(&dp.pane_id))
    .collect();

    // Detect PID restarts inside a live pane and scrub stale transcript/metrics/watch state.
    let mut pane_eff_birth_remote: HashMap<PaneId, u64> = HashMap::new();
    for dp in &discovered {
        if dp.pane_dead {
            continue;
        }
        state
            .pane_first_seen_remote_ms
            .entry(dp.pane_id.clone())
            .or_insert(remote_now_ms);
        let eff_pid = dp
            .agent_pid
            .or_else(|| dp.subtree_pids.first().copied())
            .filter(|&p| p > 0);
        let agent_changed = state
            .pane_agent
            .insert(dp.pane_id.clone(), dp.agent)
            .is_some_and(|prev| prev != dp.agent);
        let pid_restarted = eff_pid.is_some_and(|cur_pid| {
            state
                .pane_agent_pid
                .insert(dp.pane_id.clone(), cur_pid)
                .is_some_and(|prev_pid| prev_pid != cur_pid)
        });
        if pid_restarted || agent_changed {
            state.resolved_by_pane.remove(&dp.pane_id);
            state.tail_cache.remove(&dp.pane_id);
            state.pane_watch.remove(&dp.pane_id);
            state.spawned_panes.remove(&dp.pane_id);
            state
                .pane_first_seen_remote_ms
                .insert(dp.pane_id.clone(), remote_now_ms);
        }

        let proc_birth = dp
            .agent_pid
            .and_then(|pid| state.pid_birth_remote_ms.get(&pid).copied())
            .or_else(|| {
                dp.subtree_pids
                    .iter()
                    .filter_map(|pid| state.pid_birth_remote_ms.get(pid).copied())
                    .max()
            });
        let spawned_remote_birth = state
            .spawned_panes
            .get(&dp.pane_id)
            .map(|m| m.remote_spawned_at_ms);
        let first_seen = state
            .pane_first_seen_remote_ms
            .get(&dp.pane_id)
            .copied()
            .unwrap_or(remote_now_ms);
        let eff_birth = match (proc_birth, spawned_remote_birth) {
            (Some(pb), Some(sb)) => pb.max(sb),
            (Some(pb), None) => pb,
            (None, Some(sb)) => sb,
            (None, None) => first_seen,
        };
        pane_eff_birth_remote.insert(dp.pane_id.clone(), eff_birth);
        state
            .pane_birth_remote_ms
            .insert(dp.pane_id.clone(), eff_birth);

        // Update fingerprint tail cache from newly captured tails or increment cache age.
        if let Some(fresh_tail) = parsed.captured_tails.get(&dp.pane_id) {
            state.tail_cache.insert(
                dp.pane_id.clone(),
                SshTailCacheEntry {
                    history_size: dp.history_size,
                    cursor_x: dp.cursor_x,
                    cursor_y: dp.cursor_y,
                    in_mode: dp.pane_in_mode,
                    current_command: dp.pane_current_command.clone(),
                    ticks_cached: 0,
                    tail: fresh_tail.clone(),
                },
            );
        } else if let Some(entry) = state.tail_cache.get_mut(&dp.pane_id) {
            if entry.history_size == dp.history_size
                && entry.cursor_x == dp.cursor_x
                && entry.cursor_y == dp.cursor_y
                && entry.in_mode == dp.pane_in_mode
                && entry.current_command == dp.pane_current_command
            {
                entry.ticks_cached = entry.ticks_cached.saturating_add(1);
            }
        }
    }

    // Build per-pane excluded external IDs (all Hub-claimed IDs + IDs owned by sibling panes).
    let sibling_claimed: HashMap<PaneId, String> = state
        .resolved_by_pane
        .iter()
        .filter_map(|(p, r)| r.external_id.as_ref().map(|eid| (p.clone(), eid.clone())))
        .collect();
    let excluded_for_pane = |pane: &PaneId, newly_claimed: &HashSet<String>| -> HashSet<String> {
        let mut ex = claimed_external_ids.clone();
        // If this pane itself already holds an ID in `sibling_claimed`, keep it valid.
        if let Some(own_eid) = sibling_claimed.get(pane) {
            ex.remove(own_eid);
        }
        for (owner, eid) in &sibling_claimed {
            if owner != pane {
                ex.insert(eid.clone());
            }
        }
        ex.extend(newly_claimed.iter().cloned());
        ex
    };

    // Pass 1: Authoritative resolution across all 3 agents (Claude PID file, AGY presence lock, Codex resume UUID).
    let mut newly_claimed_ids: HashSet<String> = HashSet::new();
    let mut unmatched_codex: Vec<&DiscoveredPane> = Vec::new();
    let mut unmatched_agy: Vec<&DiscoveredPane> = Vec::new();
    let agy_root_id_set: HashSet<String> = state
        .agy_roots
        .iter()
        .map(|r| r.conversation_id.clone())
        .collect();

    for dp in &discovered {
        if dp.pane_dead {
            continue;
        }
        let pids: Vec<u32> = if !dp.subtree_pids.is_empty() {
            dp.subtree_pids.clone()
        } else {
            dp.agent_pid.into_iter().collect()
        };
        let exclude = excluded_for_pane(&dp.pane_id, &newly_claimed_ids);

        match dp.agent {
            AgentKind::Claude => {
                if let Some((sid, path)) = resolve_remote_claude_by_pids(
                    &remote_home,
                    &pids,
                    &parsed.claude_sessions,
                    &state.pid_birth_remote_ms,
                    &state.latest_tr_stats,
                    &exclude,
                ) {
                    newly_claimed_ids.insert(sid.clone());
                    upsert_resolved_transcript(state, &dp.pane_id, path, Some(sid), true);
                }
            }
            AgentKind::Codex => {
                if let Some(resume_id) = dp.codex_resume_id.as_deref() {
                    let resolved_path = parsed
                        .codex_resume_paths
                        .get(resume_id)
                        .cloned()
                        .or_else(|| {
                            state
                                .codex_threads
                                .iter()
                                .find(|r| r.id == resume_id)
                                .map(|r| r.rollout_path.clone())
                        })
                        .or_else(|| {
                            parsed
                                .codex_rollouts
                                .iter()
                                .find(|r| r.id == resume_id)
                                .map(|r| r.path.clone())
                        });
                    if let Some(path) = resolved_path {
                        newly_claimed_ids.insert(resume_id.to_string());
                        upsert_resolved_transcript(
                            state,
                            &dp.pane_id,
                            path,
                            Some(resume_id.to_string()),
                            true,
                        );
                    } else {
                        unmatched_codex.push(dp);
                    }
                } else {
                    unmatched_codex.push(dp);
                }
            }
            AgentKind::Agy => {
                let mut found_lock: Option<String> = None;
                // Prefer known `nesting_depth = 0` root conversations over child locks.
                for pid in &pids {
                    if let Some(cids) = parsed.agy_locks_by_pid.get(pid) {
                        if let Some(root_cid) =
                            cids.iter().find(|c| agy_root_id_set.contains(c.as_str()))
                        {
                            found_lock = Some(root_cid.clone());
                            break;
                        }
                    }
                }
                if found_lock.is_none() {
                    for pid in &pids {
                        if let Some(cids) = parsed.agy_locks_by_pid.get(pid) {
                            if let Some(first) = cids.first() {
                                found_lock = Some(first.clone());
                                break;
                            }
                        }
                    }
                }
                if let Some(conv_id) = found_lock {
                    newly_claimed_ids.insert(conv_id.clone());
                    if let Some(path) =
                        resolve_remote_agy_path(&remote_home, &conv_id, &state.latest_tr_stats)
                    {
                        upsert_resolved_transcript(state, &dp.pane_id, path, Some(conv_id), true);
                    }
                } else {
                    unmatched_agy.push(dp);
                }
            }
            AgentKind::Shell => {}
        }
    }

    // Pass 2: Strict non-authoritative matching for unlocked Codex & AGY panes
    // (`unique_in_cwd && !claimed && post_birth && unambiguous`).
    let mut codex_cwd_counts: HashMap<String, usize> = HashMap::new();
    for dp in &unmatched_codex {
        *codex_cwd_counts
            .entry(dp.cwd.trim_end_matches('/').to_string())
            .or_insert(0) += 1;
    }
    for dp in &unmatched_codex {
        let unique = codex_cwd_counts
            .get(dp.cwd.trim_end_matches('/'))
            .copied()
            .unwrap_or(0)
            == 1;
        if !unique {
            continue;
        }
        let birth_ms = pane_eff_birth_remote
            .get(&dp.pane_id)
            .copied()
            .unwrap_or(remote_now_ms);
        let exclude = excluded_for_pane(&dp.pane_id, &newly_claimed_ids);
        let tail = state
            .tail_cache
            .get(&dp.pane_id)
            .map(|e| e.tail.as_str())
            .unwrap_or("");

        // Try sqlite3 threads match (by status title + cwd + post-birth), then zero-sqlite3 rollout header fallback.
        let matched = match_remote_codex_sqlite(
            &state.codex_threads,
            &state.latest_tr_stats,
            &dp.cwd,
            tail,
            &exclude,
            birth_ms,
        )
        .or_else(|| {
            match_remote_codex_rollout_fallback(&parsed.codex_rollouts, &dp.cwd, &exclude, birth_ms)
        });
        if let Some((cid, path)) = matched {
            newly_claimed_ids.insert(cid.clone());
            upsert_resolved_transcript(state, &dp.pane_id, path, Some(cid), false);
        }
    }

    let mut agy_cwd_counts: HashMap<String, usize> = HashMap::new();
    for dp in &unmatched_agy {
        *agy_cwd_counts
            .entry(dp.cwd.trim_end_matches('/').to_string())
            .or_insert(0) += 1;
    }
    for dp in &unmatched_agy {
        let unique = agy_cwd_counts
            .get(dp.cwd.trim_end_matches('/'))
            .copied()
            .unwrap_or(0)
            == 1;
        if !unique {
            continue;
        }
        let birth_ms = pane_eff_birth_remote
            .get(&dp.pane_id)
            .copied()
            .unwrap_or(remote_now_ms);
        let exclude = excluded_for_pane(&dp.pane_id, &newly_claimed_ids);
        if let Some((cid, path)) = match_remote_agy_unlocked(
            &remote_home,
            &state.agy_roots,
            &state.latest_tr_stats,
            &dp.cwd,
            &exclude,
            birth_ms,
        ) {
            newly_claimed_ids.insert(cid.clone());
            upsert_resolved_transcript(state, &dp.pane_id, path, Some(cid), false);
        }
    }

    // Pass 3: Self-heal any non-authoritative pane holding a pre-birth or sibling-claimed transcript.
    let mut eid_owner_counts: HashMap<String, usize> = HashMap::new();
    for res in state.resolved_by_pane.values() {
        if let Some(eid) = &res.external_id {
            *eid_owner_counts.entry(eid.clone()).or_insert(0) += 1;
        }
    }
    let to_scrub: Vec<PaneId> = state
        .resolved_by_pane
        .iter()
        .filter(|(pane, res)| {
            if res.authoritative {
                return false;
            }
            let claimed_other = res.external_id.as_deref().is_some_and(|eid| {
                let duplicate_live_owner = eid_owner_counts.get(eid).copied().unwrap_or(0) > 1;
                let owned_by_self_prev =
                    sibling_claimed.get(*pane).map(String::as_str) == Some(eid);
                let claimed_externally = !owned_by_self_prev && claimed_external_ids.contains(eid);
                duplicate_live_owner || claimed_externally
            });
            let stat = state.latest_tr_stats.get(&res.path);
            let pre_birth = stat.is_some_and(|st| {
                let mtime_ms = (st.mtime_s.max(0) as u64).saturating_mul(1_000);
                pane_eff_birth_remote
                    .get(*pane)
                    .is_some_and(|&b| !is_post_birth_timestamp(mtime_ms, b))
            });
            claimed_other || pre_birth
        })
        .map(|(p, _)| p.clone())
        .collect();
    for p in to_scrub {
        state.resolved_by_pane.remove(&p);
    }

    // Pass 4: Assemble qualified SessionRecords, heuristic events, AGY Swarm children, and
    // list of transcripts whose remote `(ino, size)` changed and need Hub mirror delta sync.
    let mut sessions = Vec::with_capacity(discovered.len());
    let mut events = Vec::new();
    let mut need_mirror: Vec<(PaneId, AgentKind, String, RemoteFileStat)> = Vec::new();
    let mut discovered_pane_ids: HashSet<PaneId> = HashSet::new();
    state.session_lookup.clear();

    for dp in &discovered {
        discovered_pane_ids.insert(dp.pane_id.clone());
        let qualified_pane = qualify_host_pane(host_id, &dp.pane_id);
        let local_sid = legacy_session_id(&dp.pane_id);
        let qualified_sid = qualify_host_session(host_id, &local_sid);
        let spawned_meta = state.spawned_panes.get(&dp.pane_id).cloned();

        let resolved = state.resolved_by_pane.get(&dp.pane_id).cloned();
        let tr_path = resolved.as_ref().map(|r| r.path.clone());
        let tr_stat = tr_path
            .as_deref()
            .and_then(|p| state.latest_tr_stats.get(p).copied());

        if let (Some(r), Some(st)) = (resolved.as_ref(), tr_stat) {
            if r.last_stat
                .is_none_or(|prev| prev.ino != st.ino || prev.size != st.size)
            {
                need_mirror.push((dp.pane_id.clone(), dp.agent, r.path.clone(), st));
            }
        }

        // Model & effort precedence: verified transcript > CLI args > spawn metadata > AGY model suffix.
        let model = resolved
            .as_ref()
            .and_then(|r| r.model.clone())
            .or_else(|| dp.model.clone())
            .or_else(|| spawned_meta.as_ref().and_then(|m| m.model.clone()));
        let effort = resolved
            .as_ref()
            .and_then(|r| r.effort.clone())
            .or_else(|| dp.effort.clone())
            .or_else(|| spawned_meta.as_ref().and_then(|m| m.effort.clone()))
            .or_else(|| {
                (dp.agent == AgentKind::Agy)
                    .then(|| model.as_deref().and_then(extract_agy_effort_from_model))
                    .flatten()
            });

        let tail_opt = state
            .tail_cache
            .get(&dp.pane_id)
            .map(|e| e.tail.as_str())
            .map(str::to_string);

        // Heuristic state transitions from captured/cached tail.
        let init_state = if dp.pane_dead {
            SessionState::Ended
        } else if spawned_meta
            .as_ref()
            .is_some_and(|m| now_hub_ms.saturating_sub(m.hub_spawned_at_ms) < LEGACY_SPAWN_GRACE_MS)
        {
            SessionState::Starting
        } else {
            SessionState::Idle
        };

        let (sess_state, state_since_ms, last_event_ms, last_error, hev) = if dp.pane_dead {
            (SessionState::Ended, now_hub_ms, now_hub_ms, None, None)
        } else if let Some(ref tail) = tail_opt {
            apply_ssh_pane_tail_watch(
                &mut state.pane_watch,
                &dp.pane_id,
                &qualified_sid,
                &qualified_pane,
                dp.agent,
                Some(&dp.cwd),
                model.as_deref(),
                tail,
                init_state,
                now_hub_ms,
            )
        } else {
            (init_state, now_hub_ms, now_hub_ms, None, None)
        };
        if let Some(ev) = hev {
            events.push(ev);
        }

        // Status-line fallback metrics merged with transcript-derived metrics.
        let st_metrics = tail_opt
            .as_deref()
            .and_then(|t| parse_status_line_metrics(dp.agent, t, now_hub_ms));
        let tr_metrics = resolved.as_ref().and_then(|r| r.metrics.clone());
        let metrics = merge_metrics(tr_metrics, st_metrics);

        // AGY Swarm subagent children attached to this root session & root IDLE hint.
        let mut subagent_count: u32 = 0;
        let mut active_subagents: u32 = 0;
        let mut agy_root_is_idle = false;
        if dp.agent == AgentKind::Agy {
            if let Some(ext_id) = resolved.as_ref().and_then(|r| r.external_id.as_deref()) {
                agy_root_is_idle = state.agy_roots.iter().any(|r| {
                    r.conversation_id == ext_id
                        && r.status.contains("IDLE")
                        && !r.status.contains("RUNNING")
                });
                for child in &state.agy_children {
                    if child.parent_conversation_id != ext_id {
                        continue;
                    }
                    let cid = child.conversation_id.trim();
                    if cid.is_empty() || cid.contains('/') || cid.contains("..") {
                        continue;
                    }
                    let child_tr = format!(
                        "{}/.gemini/antigravity-cli/brain/{cid}/.system_generated/logs/transcript.jsonl",
                        remote_home.trim_end_matches('/')
                    );
                    let child_state = if child.killed != 0 {
                        SessionState::Ended
                    } else if child.status.contains("RUNNING") {
                        SessionState::Working
                    } else {
                        SessionState::Idle
                    };
                    subagent_count += 1;
                    if !matches!(child_state, SessionState::Ended | SessionState::Idle) {
                        active_subagents += 1;
                    }
                    let local_child_sid = SessionId::new(cid);
                    let qualified_child_sid = qualify_host_session(host_id, &local_child_sid);
                    let child_stat = state.latest_tr_stats.get(&child_tr).copied();
                    state.session_lookup.insert(
                        local_child_sid,
                        RemoteSessionLookup {
                            agent: AgentKind::Agy,
                            remote_path: child_tr.clone(),
                            latest_stat: child_stat,
                            metrics: None,
                        },
                    );
                    sessions.push(SessionRecord {
                        session_id: qualified_child_sid,
                        agent: AgentKind::Agy,
                        pane: None,
                        pane_resolution: PaneResolution::Unknown,
                        cwd: Some(dp.cwd.clone()),
                        git_root: spawned_meta.as_ref().and_then(|m| m.git_root.clone()),
                        model: None,
                        effort: None,
                        state: child_state,
                        state_since_ms: now_hub_ms,
                        last_event_id: None,
                        last_event_ms: now_hub_ms,
                        last_message: None,
                        last_error: None,
                        parent_session_id: Some(qualified_sid.clone()),
                        is_subagent: true,
                        subagent_count: 0,
                        active_subagents: 0,
                        stage: None,
                        unread_inbox: 0,
                        confidence: Confidence::Heuristic,
                        tmux_target: None,
                        tmux_label: (!child.agent_name.is_empty())
                            .then(|| child.agent_name.clone()),
                        discovered_only: true,
                        transcript_path: Some(child_tr),
                        metrics: None,
                        host_id: Some(host_id.clone()),
                        host_display_name: Some(display_name.to_string()),
                        mux_kind: None,
                    });
                }
            }
        }

        if let Some(ref tp) = tr_path {
            state.session_lookup.insert(
                local_sid,
                RemoteSessionLookup {
                    agent: dp.agent,
                    remote_path: tp.clone(),
                    latest_stat: tr_stat,
                    metrics: metrics.clone(),
                },
            );
        }

        let effective_state = if dp.agent == AgentKind::Agy
            && agy_root_is_idle
            && active_subagents == 0
            && sess_state == SessionState::Working
            && now_hub_ms.saturating_sub(last_event_ms) >= 10_000
        {
            SessionState::Idle
        } else {
            sess_state
        };

        sessions.push(SessionRecord {
            session_id: qualified_sid,
            agent: dp.agent,
            pane: Some(qualified_pane),
            pane_resolution: PaneResolution::Env,
            cwd: Some(dp.cwd.clone()),
            git_root: spawned_meta
                .as_ref()
                .and_then(|m| m.git_root.clone())
                .or_else(|| dp.git_root.clone()),
            model,
            effort,
            state: effective_state,
            state_since_ms,
            last_event_id: None,
            last_event_ms,
            last_message: None,
            last_error,
            parent_session_id: None,
            is_subagent: false,
            subagent_count,
            active_subagents,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Heuristic,
            tmux_target: Some(dp.tmux_target.clone()),
            tmux_label: Some(dp.tmux_label.clone()),
            discovered_only: true,
            transcript_path: tr_path,
            metrics,
            host_id: Some(host_id.clone()),
            host_display_name: Some(display_name.to_string()),
            mux_kind: Some("tmux".to_string()),
        });
    }

    // Preserve freshly spawned `Starting` panes during `LEGACY_SPAWN_GRACE_MS` while the
    // remote agent CLI is still booting inside the live tmux pane.
    for (local_pane, meta) in &state.spawned_panes {
        if discovered_pane_ids.contains(local_pane) {
            continue;
        }
        if !live_pane_ids.contains(local_pane) {
            continue;
        }
        if now_hub_ms.saturating_sub(meta.hub_spawned_at_ms) >= LEGACY_SPAWN_GRACE_MS {
            continue;
        }
        let qualified_pane = qualify_host_pane(host_id, local_pane);
        let qualified_sid = qualify_host_session(host_id, &legacy_session_id(local_pane));
        sessions.push(SessionRecord {
            session_id: qualified_sid,
            agent: meta.agent,
            pane: Some(qualified_pane),
            pane_resolution: PaneResolution::Env,
            cwd: Some(meta.cwd.clone()),
            git_root: meta.git_root.clone(),
            model: meta.model.clone(),
            effort: meta.effort.clone(),
            state: SessionState::Starting,
            state_since_ms: meta.hub_spawned_at_ms,
            last_event_id: None,
            last_event_ms: meta.hub_spawned_at_ms,
            last_message: None,
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Heuristic,
            tmux_target: Some(meta.tmux_target.clone()),
            tmux_label: Some(meta.tmux_label.clone()),
            discovered_only: true,
            transcript_path: None,
            metrics: None,
            host_id: Some(host_id.clone()),
            host_display_name: Some(display_name.to_string()),
            mux_kind: Some("tmux".to_string()),
        });
    }

    SshReconcileResult {
        sessions,
        events,
        need_mirror,
    }
}

fn upsert_resolved_transcript(
    state: &mut SshDiscoveryState,
    pane: &PaneId,
    path: String,
    external_id: Option<String>,
    authoritative: bool,
) {
    match state.resolved_by_pane.get_mut(pane) {
        Some(existing) if existing.path == path => {
            existing.authoritative |= authoritative;
            if existing.external_id.is_none() {
                existing.external_id = external_id;
            }
        }
        _ => {
            state.resolved_by_pane.insert(
                pane.clone(),
                ResolvedRemoteTranscript {
                    path,
                    external_id,
                    authoritative,
                    last_stat: None,
                    metrics: None,
                    model: None,
                    effort: None,
                },
            );
        }
    }
}

fn resolve_remote_claude_by_pids(
    remote_home: &str,
    subtree_pids: &[u32],
    claude_sessions: &[RemoteClaudePidInfo],
    pid_births: &HashMap<u32, u64>,
    tr_stats: &HashMap<String, RemoteFileStat>,
    claimed_ids: &HashSet<String>,
) -> Option<(String, String)> {
    let home = remote_home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    for &pid in subtree_pids {
        let Some(info) = claude_sessions.iter().find(|c| c.pid == pid) else {
            continue;
        };
        if claimed_ids.contains(&info.session_id) {
            continue;
        }
        // Reject recycled PID leftovers whose `startedAt` diverges >2m from remote OS process birth.
        if let Some(&os_birth_ms) = pid_births.get(&pid) {
            if let Some(started_ms) = info.started_at_ms {
                if started_ms.abs_diff(os_birth_ms) > CLAUDE_PID_START_MAX_SKEW_MS {
                    continue;
                }
            } else if let Some(mtime_ms) = info.file_mtime_ms {
                if mtime_ms.saturating_add(86_400_000) < os_birth_ms {
                    continue;
                }
            }
        }
        let target_suffix = format!("/{}.jsonl", info.session_id);
        if let Some(ref cwd) = info.cwd {
            let enc = encode_claude_project_dir(cwd);
            let direct = format!("{home}/.claude/projects/{enc}/{}.jsonl", info.session_id);
            if tr_stats.contains_key(&direct) {
                return Some((info.session_id.clone(), direct));
            }
        }
        let projects_prefix = format!("{home}/.claude/projects/");
        if let Some(found) = tr_stats
            .keys()
            .find(|p| p.starts_with(&projects_prefix) && p.ends_with(&target_suffix))
        {
            return Some((info.session_id.clone(), found.clone()));
        }
    }
    None
}

fn resolve_remote_agy_path(
    remote_home: &str,
    conv_id: &str,
    tr_stats: &HashMap<String, RemoteFileStat>,
) -> Option<String> {
    let home = remote_home.trim_end_matches('/');
    let cid = conv_id.trim();
    if home.is_empty() || cid.is_empty() || cid.contains('/') || cid.contains("..") {
        return None;
    }
    let compact = format!(
        "{home}/.gemini/antigravity-cli/brain/{cid}/.system_generated/logs/transcript.jsonl"
    );
    if tr_stats.contains_key(&compact) {
        return Some(compact);
    }
    let full = format!(
        "{home}/.gemini/antigravity-cli/brain/{cid}/.system_generated/logs/transcript_full.jsonl"
    );
    if tr_stats.contains_key(&full) {
        return Some(full);
    }
    None
}

fn match_remote_codex_sqlite(
    rows: &[RemoteCodexThreadRow],
    tr_stats: &HashMap<String, RemoteFileStat>,
    cwd: &str,
    tail: &str,
    claimed_ids: &HashSet<String>,
    min_birth_remote_ms: u64,
) -> Option<(String, String)> {
    let hint = extract_codex_status_title(tail)?;
    let hint_clean = hint.trim_end_matches(['…', '.']).trim();
    if hint_clean.is_empty() {
        return None;
    }
    let check_row = |r: &RemoteCodexThreadRow| -> Option<(String, String)> {
        if claimed_ids.contains(r.id.trim()) || !same_remote_dir(&r.cwd, cwd) {
            return None;
        }
        let stat = tr_stats.get(&r.rollout_path);
        let updated_ms = epoch_i64_to_ms(r.updated_at)
            .or_else(|| stat.and_then(|s| epoch_i64_to_ms(s.mtime_s)))?;
        if !is_post_birth_timestamp(updated_ms, min_birth_remote_ms) {
            return None;
        }
        let created_ms = epoch_i64_to_ms(r.created_at)?;
        if !is_post_birth_timestamp(created_ms, min_birth_remote_ms) {
            return None;
        }
        Some((r.id.clone(), r.rollout_path.clone()))
    };

    let mut exact = Vec::new();
    for r in rows {
        let nm = r.name.as_deref().unwrap_or("").trim();
        if !nm.is_empty() && nm == hint {
            if let Some(hit) = check_row(r) {
                exact.push(hit);
                if exact.len() > 1 {
                    return None;
                }
            }
        }
    }
    if exact.len() == 1 {
        return exact.pop();
    }

    if hint_clean.chars().count() >= 8 {
        let mut prefix = Vec::new();
        for r in rows {
            let nm = r.name.as_deref().unwrap_or("").trim();
            if nm.starts_with(hint_clean) {
                if let Some(hit) = check_row(r) {
                    prefix.push(hit);
                    if prefix.len() > 1 {
                        return None;
                    }
                }
            }
        }
        if prefix.len() == 1 {
            return prefix.pop();
        }
    }
    None
}

fn match_remote_codex_rollout_fallback(
    rollouts: &[RemoteCodexRolloutCandidate],
    cwd: &str,
    claimed_ids: &HashSet<String>,
    min_birth_remote_ms: u64,
) -> Option<(String, String)> {
    let mut hits = Vec::new();
    for r in rollouts {
        if claimed_ids.contains(&r.id) || !same_remote_dir(&r.cwd, cwd) {
            continue;
        }
        let Some(ts_ms) = r.timestamp_ms else {
            continue;
        };
        if !is_post_birth_timestamp(ts_ms, min_birth_remote_ms) {
            continue;
        }
        let mtime_ms = (r.stat.mtime_s.max(0) as u64).saturating_mul(1_000);
        if !is_post_birth_timestamp(mtime_ms, min_birth_remote_ms) {
            continue;
        }
        hits.push((r.id.clone(), r.path.clone()));
        if hits.len() > 1 {
            return None;
        }
    }
    if hits.len() == 1 {
        hits.pop()
    } else {
        None
    }
}

fn match_remote_agy_unlocked(
    remote_home: &str,
    rows: &[RemoteAgyRootRow],
    tr_stats: &HashMap<String, RemoteFileStat>,
    cwd: &str,
    claimed_ids: &HashSet<String>,
    min_birth_remote_ms: u64,
) -> Option<(String, String)> {
    let uri = format!("file://{}", cwd.trim_end_matches('/'));
    let quoted = format!("\"{uri}\"");
    let quoted_slash = format!("\"{uri}/\"");
    let mut hits = Vec::new();
    for r in rows {
        let cid = r.conversation_id.trim();
        if cid.is_empty() || cid.contains('/') || cid.contains("..") || claimed_ids.contains(cid) {
            continue;
        }
        if !r.workspace_uris.contains(&quoted) && !r.workspace_uris.contains(&quoted_slash) {
            continue;
        }
        let Some(tr_path) = resolve_remote_agy_path(remote_home, cid, tr_stats) else {
            continue;
        };
        let Some(st) = tr_stats.get(&tr_path) else {
            continue;
        };
        if r.step_count <= 0 && st.size == 0 {
            continue;
        }
        if let Some(mod_ms) = parse_iso8601_ms(&r.last_modified_time) {
            if !is_post_birth_timestamp(mod_ms, min_birth_remote_ms) {
                continue;
            }
        }
        if let Some(in_ms) = parse_iso8601_ms(&r.last_user_input_time) {
            if !is_post_birth_timestamp(in_ms, min_birth_remote_ms) {
                continue;
            }
        }
        let mtime_ms = (st.mtime_s.max(0) as u64).saturating_mul(1_000);
        if !is_post_birth_timestamp(mtime_ms, min_birth_remote_ms) {
            continue;
        }
        hits.push((cid.to_string(), tr_path));
        if hits.len() > 1 {
            return None;
        }
    }
    if hits.len() == 1 {
        hits.pop()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// SshMuxNodeProvider (zero-install ControlMaster)
// ---------------------------------------------------------------------------

pub struct SshMuxNodeProvider {
    host_id: HostId,
    default_display_name: String,
    last_display_name: Mutex<String>,
    ownership: NotifyOwnership,
    include_shell_panes: bool,
    #[allow(dead_code)]
    tmux: Tmux,
    transport: Arc<SshExec>,
    tmux_bin: String,
    tmux_socket: Option<String>,
    claude_bin: String,
    codex_bin: String,
    agy_bin: String,
    mirror: Arc<RemoteTranscriptMirror>,
    transcript_index: Arc<Mutex<TranscriptIndexCache>>,
    /// Bounds background SSH channels (`sync`, `capture_pane`, `fetch_transcript`, `list_tmux_sessions`).
    io_sem: Arc<Semaphore>,
    /// Dedicated interactive SSH channel (`dispatch_interactive`, `spawn_session`, `stage_input_files`, `focus_pane`, `close_pane`, `probe_now`).
    interactive_sem: Arc<Semaphore>,
    circuit: Mutex<HostCircuitBreaker>,
    discovery: Mutex<SshDiscoveryState>,
    staged_cache: Mutex<StagedFileCache>,
}

impl SshMuxNodeProvider {
    pub fn new(cfg: &NodeConfig, ssh_dir: &Path) -> Self {
        let state_dir = ssh_dir.parent().unwrap_or(ssh_dir).to_path_buf();
        Self::with_shared_caches(
            cfg,
            &state_dir,
            Arc::new(RemoteTranscriptMirror::new(state_dir.clone())),
            Arc::new(Mutex::new(TranscriptIndexCache::default())),
        )
    }

    pub fn with_shared_caches(
        cfg: &NodeConfig,
        state_dir: &Path,
        mirror: Arc<RemoteTranscriptMirror>,
        transcript_index: Arc<Mutex<TranscriptIndexCache>>,
    ) -> Self {
        let host_id = HostId::new(cfg.id.trim());
        let display = cfg
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(host_id.as_str())
            .to_string();
        let ssh_dir = state_dir.join("ssh-cm");
        let control_path =
            ssh_dir.join(format!("cm-{}.sock", sanitize_sock_name(host_id.as_str())));
        let transport = Arc::new(
            SshExec::new(cfg.ssh_target.trim(), control_path)
                .with_ssh_bin(cfg.ssh_bin_override())
                .with_connect_timeout_secs(cfg.effective_connect_timeout_secs())
                .with_compression(cfg.compression)
                .with_extra_args(cfg.ssh_args.clone()),
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
            circuit: Mutex::new(HostCircuitBreaker::new(host_id.as_str())),
            host_id,
            last_display_name: Mutex::new(display.clone()),
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            include_shell_panes: cfg.include_shell_panes,
            tmux,
            transport,
            tmux_bin,
            tmux_socket,
            claude_bin: cfg.agent_bin(AgentKind::Claude),
            codex_bin: cfg.agent_bin(AgentKind::Codex),
            agy_bin: cfg.agent_bin(AgentKind::Agy),
            mirror,
            transcript_index,
            io_sem: Arc::new(Semaphore::new(SSH_BG_MAX_CONCURRENT)),
            interactive_sem: Arc::new(Semaphore::new(SSH_INTERACTIVE_MAX_CONCURRENT)),
            discovery: Mutex::new(SshDiscoveryState::default()),
            staged_cache: Mutex::new(StagedFileCache::default()),
        }
    }

    fn tmux_cmd_prefix(&self) -> String {
        match &self.tmux_socket {
            Some(s) => format!(
                "{} -L {}",
                shell_single_quote(&self.tmux_bin),
                shell_single_quote(s)
            ),
            None => shell_single_quote(&self.tmux_bin),
        }
    }

    fn agent_bin(&self, agent: AgentKind) -> &str {
        match agent {
            AgentKind::Claude => &self.claude_bin,
            AgentKind::Codex => &self.codex_bin,
            AgentKind::Agy => &self.agy_bin,
            AgentKind::Shell => "sh",
        }
    }

    /// Build the single-RTT POSIX `/bin/sh -s` compound discovery script (`compound_discover_script_v2`)
    /// emitting all 6 deterministic sections:
    /// `__SF_SEC_META__`, `__SF_SEC_PANES__`, `__SF_SEC_PS__`, `__SF_SEC_TAILS__`,
    /// `__SF_SEC_RESOLVE__`, `__SF_SEC_TR_STATS__`.
    fn compound_discover_script_v2(
        &self,
        cached_tail_fps: &[String],
        prev_agy_db_sig: Option<&str>,
        prev_codex_db_sig: Option<&str>,
        known_tr_paths: &[String],
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let pane_fmt = shell_single_quote(starfane_tmux::PANE_FORMAT);
        let prev_agy = shell_single_quote(prev_agy_db_sig.unwrap_or(""));
        let prev_cx = shell_single_quote(prev_codex_db_sig.unwrap_or(""));

        let mut fp_cases = String::new();
        for fp in cached_tail_fps {
            fp_cases.push_str(&format!("    {}) return 0 ;;\n", shell_single_quote(fp)));
        }

        let mut known_tr_lines = String::new();
        for p in known_tr_paths {
            if !p.contains('\n') && !p.contains('\r') {
                known_tr_lines.push_str(&format!("sf_add_tr_stat {}\n", shell_single_quote(p)));
            }
        }

        format!(
            r#"{bootstrap}{sf_stat}
SF_US=$(printf '\037')
SF_TR_PATHS=""
sf_add_tr_stat() {{
  [ -n "$1" ] || return 0
  case "
$SF_TR_PATHS
" in
    *"
$1
"*) return 0 ;;
  esac
  if [ -z "$SF_TR_PATHS" ]; then
    SF_TR_PATHS="$1"
  else
    SF_TR_PATHS="$SF_TR_PATHS
$1"
  fi
}}
sf_tail_cached() {{
  case "$1" in
{fp_cases}    *) return 1 ;;
  esac
}}

printf '{sec_meta}\n'
printf 'REMOTE_NOW_S=%s\n' "$(date +%s)"
printf 'REMOTE_HOME=%s\n' "$HOME"

printf '\n{sec_panes}\n'
PANES_OUT=$({tmux} list-panes -a -F {pane_fmt} 2>/dev/null || true)
[ -n "$PANES_OUT" ] && printf '%s\n' "$PANES_OUT"

printf '\n{sec_ps}\n'
PS_OUT=$(LC_ALL=C ps -eo pid=,ppid=,etime=,args= 2>/dev/null || true)
[ -n "$PS_OUT" ] && printf '%s\n' "$PS_OUT"

printf '\n{sec_tails}\n'
if [ -n "$PANES_OUT" ]; then
  PANES_US=$(printf '%s\n' "$PANES_OUT" | LC_ALL=C tr '\t' '\037')
  while IFS="$SF_US" read -r _p_id _s_n _w_i _w_x _p_x _p_p _p_cmd _p_cwd _p_a _w_a _s_a _p_mode _p_dead _p_syn _p_alt _p_cx _p_cy _p_wn _p_sg _p_hs _p_ph; do
    case "$_p_id" in %*) ;; *) continue ;; esac
    [ "$_p_dead" = "1" ] && continue
    [ "$_p_mode" = "1" ] && continue
    _fp="${{_p_id}}|${{_p_hs}}|${{_p_cx}}|${{_p_cy}}|${{_p_mode}}|${{_p_cmd}}"
    if sf_tail_cached "$_fp"; then
      continue
    fi
    printf '__SF_TAIL_BEGIN__ %s\n' "$_p_id"
    {tmux} capture-pane -p -t "$_p_id" -S -20 2>/dev/null || true
    printf '\n__SF_TAIL_END__ %s\n' "$_p_id"
  done <<__SF_PANES_EOF__
$PANES_US
__SF_PANES_EOF__
fi

printf '\n{sec_resolve}\n'
# 1. Claude: live-PID (~/.claude/sessions/<pid>.json) + transcript stats
if [ -d "$HOME/.claude/sessions" ]; then
  for _cf in "$HOME"/.claude/sessions/*.json; do
    [ -f "$_cf" ] && [ ! -L "$_cf" ] || continue
    _base=${{_cf##*/}}
    _pid=${{_base%.json}}
    case "$_pid" in ''|*[!0-9]*) continue ;; esac
    kill -0 "$_pid" 2>/dev/null || continue
    _cjson=$(head -c 2048 "$_cf" 2>/dev/null | tr '\n\r' '  ')
    printf 'CLAUDE_SESS %s %s\n%s\n' "$_pid" "$(sf_stat "$_cf")" "$_cjson"
    _sid=$(printf '%s' "$_cjson" | sed -n 's/.*"sessionId"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    case "$_sid" in
      ''|*/*|*..*) ;;
      *)
        for _tp in "$HOME"/.claude/projects/*/"$_sid.jsonl"; do
          [ -f "$_tp" ] && [ ! -L "$_tp" ] && sf_add_tr_stat "$_tp"
        done
        ;;
    esac
  done
fi

# 2. AGY: Linux /proc/$pid/fd fast-path (macOS lsof fallback) + optional sqlite3 conversation_summaries
_agy_pids=""
_cx_resumes=""
if [ -n "$PS_OUT" ]; then
  set -f
  while IFS= read -r _pl; do
    set -- $_pl
    _ps_pid=$1
    _cmd_tok=$4
    _cmd_base=${{_cmd_tok##*/}}
    case "$_cmd_base" in
      starfane|baton) ;;
      agy)
        case "$_ps_pid" in ''|*[!0-9]*) ;; *) _agy_pids="$_agy_pids $_ps_pid" ;; esac
        ;;
      sh|bash|zsh|dash|node|python|python3)
        case "$_pl" in
          *"/agy "*|*"/agy"|*" agy "*|*" agy")
            case "$_pl" in
              *"starfane mcp"*|*"baton mcp"*) ;;
              *) case "$_ps_pid" in ''|*[!0-9]*) ;; *) _agy_pids="$_agy_pids $_ps_pid" ;; esac ;;
            esac
            ;;
        esac
        ;;
    esac
    case "$_pl" in
      *"codex"*"resume "*)
        _seen_res=0
        for _tok in $_pl; do
          if [ "$_seen_res" = "1" ]; then
            _u=$(printf '%s' "$_tok" | tr -d "\"'")
            case "$_u" in
              *[!0-9a-fA-F-]*|'') ;;
              *)
                if [ "${{#_u}}" -ge 16 ]; then
                  _cx_resumes="$_cx_resumes $_u"
                fi
                ;;
            esac
            _seen_res=0
          elif [ "$_tok" = "resume" ]; then
            _seen_res=1
          fi
        done
        ;;
    esac
  done <<__SF_PS_EOF__
$PS_OUT
__SF_PS_EOF__
  set +f
fi

if [ -n "$_agy_pids" ]; then
  if [ -d /proc ]; then
    for _ap in $_agy_pids; do
      [ -d "/proc/$_ap/fd" ] || continue
      for _fd in "/proc/$_ap/fd"/*; do
        _tgt=$(readlink "$_fd" 2>/dev/null || ls -l "$_fd" 2>/dev/null | sed 's/.* -> //')
        case "$_tgt" in
          *"/presence/"*.lock)
            printf 'AGY_LOCK %s %s\n' "$_ap" "$_tgt"
            _ab=${{_tgt##*/}}
            _acid=${{_ab%.lock}}
            case "$_acid" in
              ''|*/*|*..*) ;;
              *)
                _atr="$HOME/.gemini/antigravity-cli/brain/$_acid/.system_generated/logs/transcript.jsonl"
                [ -f "$_atr" ] && [ ! -L "$_atr" ] && sf_add_tr_stat "$_atr"
                _aftr="$HOME/.gemini/antigravity-cli/brain/$_acid/.system_generated/logs/transcript_full.jsonl"
                [ -f "$_aftr" ] && [ ! -L "$_aftr" ] && sf_add_tr_stat "$_aftr"
                ;;
            esac
            ;;
        esac
      done
    done
  elif command -v lsof >/dev/null 2>&1; then
    _csv=$(printf '%s' "$_agy_pids" | tr ' ' ',' | sed 's/^,//;s/,$//;s/,,*/,/g')
    if [ -n "$_csv" ]; then
      _lsof_out=$(lsof -n -P -Fpn -p "$_csv" 2>/dev/null || true)
      if [ -n "$_lsof_out" ]; then
        printf 'AGY_LSOF_BEGIN\n%s\nAGY_LSOF_END\n' "$_lsof_out"
        while IFS= read -r _ll; do
          case "$_ll" in
            n*"/presence/"*.lock)
              _ab=${{_ll##*/}}
              _acid=${{_ab%.lock}}
              case "$_acid" in
                ''|*/*|*..*) ;;
                *)
                  _atr="$HOME/.gemini/antigravity-cli/brain/$_acid/.system_generated/logs/transcript.jsonl"
                  [ -f "$_atr" ] && [ ! -L "$_atr" ] && sf_add_tr_stat "$_atr"
                  _aftr="$HOME/.gemini/antigravity-cli/brain/$_acid/.system_generated/logs/transcript_full.jsonl"
                  [ -f "$_aftr" ] && [ ! -L "$_aftr" ] && sf_add_tr_stat "$_aftr"
                  ;;
              esac
              ;;
          esac
        done <<__SF_LSOF_EOF__
$_lsof_out
__SF_LSOF_EOF__
      fi
    fi
  fi
fi

_agy_db="$HOME/.gemini/antigravity-cli/conversation_summaries.db"
if [ -f "$_agy_db" ] && [ ! -L "$_agy_db" ]; then
  _agy_sig="$(sf_stat "$_agy_db")|$(sf_stat "${{_agy_db}}-wal")"
  printf 'AGY_DB_SIG %s\n' "$_agy_sig"
  if [ "$_agy_sig" != {prev_agy} ] && command -v sqlite3 >/dev/null 2>&1; then
    _agy_sql="SELECT conversation_id, REPLACE(REPLACE(COALESCE(workspace_uris,''),char(10),' '),char(13),' '), COALESCE(status,''), COALESCE(last_modified_time,''), COALESCE(last_user_input_time,''), COALESCE(step_count,0) FROM conversation_summaries WHERE nesting_depth = 0 AND COALESCE(killed,0) = 0 ORDER BY last_modified_time DESC LIMIT 120;"
    _agy_roots=$(sqlite3 -readonly -separator "$SF_US" "$_agy_db" "$_agy_sql" 2>/dev/null || sqlite3 -separator "$SF_US" "file:${{_agy_db}}?immutable=1" "$_agy_sql" 2>/dev/null || true)
    printf 'AGY_DB_ROOTS_BEGIN\n%s\nAGY_DB_ROOTS_END\n' "$_agy_roots"
    _agy_csql="SELECT conversation_id, COALESCE(parent_conversation_id,''), REPLACE(REPLACE(COALESCE(agent_name,''),char(10),' '),char(13),' '), COALESCE(status,''), COALESCE(killed,0) FROM conversation_summaries WHERE nesting_depth = 1 AND parent_conversation_id != '' ORDER BY last_modified_time DESC LIMIT 120;"
    _agy_kids=$(sqlite3 -readonly -separator "$SF_US" "$_agy_db" "$_agy_csql" 2>/dev/null || sqlite3 -separator "$SF_US" "file:${{_agy_db}}?immutable=1" "$_agy_csql" 2>/dev/null || true)
    printf 'AGY_DB_CHILDREN_BEGIN\n%s\nAGY_DB_CHILDREN_END\n' "$_agy_kids"
    _rcnt=0
    while IFS="$SF_US" read -r _rcid _rrest; do
      case "$_rcid" in ''|*/*|*..*) continue ;; esac
      _rcnt=$((_rcnt + 1))
      [ "$_rcnt" -gt 30 ] && break
      _atr="$HOME/.gemini/antigravity-cli/brain/$_rcid/.system_generated/logs/transcript.jsonl"
      [ -f "$_atr" ] && [ ! -L "$_atr" ] && sf_add_tr_stat "$_atr"
    done <<__SF_AGY_R_EOF__
$_agy_roots
__SF_AGY_R_EOF__
    _kcnt=0
    while IFS="$SF_US" read -r _kcid _krest; do
      case "$_kcid" in ''|*/*|*..*) continue ;; esac
      _kcnt=$((_kcnt + 1))
      [ "$_kcnt" -gt 30 ] && break
      _ktr="$HOME/.gemini/antigravity-cli/brain/$_kcid/.system_generated/logs/transcript.jsonl"
      [ -f "$_ktr" ] && [ ! -L "$_ktr" ] && sf_add_tr_stat "$_ktr"
    done <<__SF_AGY_K_EOF__
$_agy_kids
__SF_AGY_K_EOF__
  fi
fi

# 3. Codex: exact resume UUID + optional sqlite3 state_5.sqlite + pure-POSIX zero-sqlite3 fallback
for _ru in $_cx_resumes; do
  for _rf in "$HOME"/.codex/sessions/*/*/*/*"$_ru".jsonl; do
    [ -f "$_rf" ] && [ ! -L "$_rf" ] || continue
    printf 'CODEX_RESUME %s %s\n' "$_ru" "$_rf"
    sf_add_tr_stat "$_rf"
  done
done

_cx_db="$HOME/.codex/state_5.sqlite"
_cx_used_sql=0
if [ -f "$_cx_db" ] && [ ! -L "$_cx_db" ] && command -v sqlite3 >/dev/null 2>&1; then
  _cx_sig="$(sf_stat "$_cx_db")|$(sf_stat "${{_cx_db}}-wal")"
  printf 'CODEX_DB_SIG %s\n' "$_cx_sig"
  _cx_used_sql=1
  if [ "$_cx_sig" != {prev_cx} ]; then
    _cx_sql="SELECT id, COALESCE(rollout_path,''), COALESCE(cwd,''), REPLACE(REPLACE(COALESCE(name,''),char(10),' '),char(13),' '), COALESCE(created_at,0), COALESCE(updated_at,0) FROM threads WHERE rollout_path IS NOT NULL AND rollout_path != '' ORDER BY COALESCE(archived,0) ASC, updated_at DESC LIMIT 200;"
    _cx_rows=$(sqlite3 -readonly -separator "$SF_US" "$_cx_db" "$_cx_sql" 2>/dev/null || true)
    printf 'CODEX_DB_BEGIN\n%s\nCODEX_DB_END\n' "$_cx_rows"
    _ccnt=0
    while IFS="$SF_US" read -r _cid _crp _crest; do
      [ -n "$_crp" ] || continue
      _ccnt=$((_ccnt + 1))
      [ "$_ccnt" -gt 30 ] && break
      [ -f "$_crp" ] && [ ! -L "$_crp" ] && sf_add_tr_stat "$_crp"
    done <<__SF_CX_R_EOF__
$_cx_rows
__SF_CX_R_EOF__
  fi
fi

if [ "$_cx_used_sql" = "0" ] && [ -d "$HOME/.codex/sessions" ]; then
  _day_dirs=$(for _d in "$HOME"/.codex/sessions/*/*/*; do [ -d "$_d" ] && printf '%s\n' "$_d"; done | LC_ALL=C sort -r | head -n 2)
  _rcnt=0
  if [ -n "$_day_dirs" ]; then
    while IFS= read -r _dd; do
      [ -d "$_dd" ] || continue
      _rfiles=$(ls -1t "$_dd"/*.jsonl 2>/dev/null | head -n 20 || true)
      [ -n "$_rfiles" ] || continue
      while IFS= read -r _rf; do
        [ -f "$_rf" ] && [ ! -L "$_rf" ] || continue
        _rcnt=$((_rcnt + 1))
        [ "$_rcnt" -gt 20 ] && break
        _rst=$(sf_stat "$_rf")
        _rhdr=$(head -n 1 "$_rf" 2>/dev/null | head -c 2048 | tr '\n\r' '  ')
        printf 'CODEX_ROLLOUT %s %s\n%s\n' "$_rst" "$_rf" "$_rhdr"
        sf_add_tr_stat "$_rf"
      done <<__SF_CX_F_EOF__
$_rfiles
__SF_CX_F_EOF__
      [ "$_rcnt" -gt 20 ] && break
    done <<__SF_CX_D_EOF__
$_day_dirs
__SF_CX_D_EOF__
  fi
fi

{known_tr_lines}
printf '\n{sec_tr_stats}\n'
if [ -n "$SF_TR_PATHS" ]; then
  while IFS= read -r _tp; do
    [ -f "$_tp" ] && [ ! -L "$_tp" ] || continue
    printf '%s %s\n' "$(sf_stat "$_tp")" "$_tp"
  done <<__SF_TR_EOF__
$SF_TR_PATHS
__SF_TR_EOF__
fi
"#,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            sf_stat = REMOTE_SF_STAT_FN,
            fp_cases = fp_cases,
            sec_meta = SEC_META,
            sec_panes = SEC_PANES,
            sec_ps = SEC_PS,
            sec_tails = SEC_TAILS,
            sec_resolve = SEC_RESOLVE,
            sec_tr_stats = SEC_TR_STATS,
            tmux = tmux_cmd,
            pane_fmt = pane_fmt,
            prev_agy = prev_agy,
            prev_cx = prev_cx,
            known_tr_lines = known_tr_lines,
        )
    }

    fn build_spawn_preflight_script(
        &self,
        agent: AgentKind,
        raw_cwd: &str,
        tmux_session: &str,
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let agent_bin = shell_single_quote(self.agent_bin(agent));
        let raw_cwd_q = shell_single_quote(raw_cwd);
        let sess_q = shell_single_quote(tmux_session);
        // Never read or touch `~/.claude.json` for Claude.
        let agent_cfg_cmd = match agent {
            AgentKind::Codex => {
                r#"[ -f "$HOME/.codex/config.toml" ] && head -c 65536 "$HOME/.codex/config.toml" 2>/dev/null || true"#
            }
            AgentKind::Agy => {
                r#"[ -f "$HOME/.gemini/antigravity-cli/settings.json" ] && head -c 65536 "$HOME/.gemini/antigravity-cli/settings.json" 2>/dev/null || true"#
            }
            AgentKind::Claude | AgentKind::Shell => "true",
        };
        format!(
            r#"{bootstrap}
RAW_CWD={raw_cwd_q}
DEFAULT_DIR="$HOME/.agent_session"
if [ -z "$RAW_CWD" ] || [ "$RAW_CWD" = "~/.agent_session" ] || [ "$RAW_CWD" = "$DEFAULT_DIR" ]; then
  TARGET="$DEFAULT_DIR"
  mkdir -p "$TARGET" 2>/dev/null || {{ printf 'ERR:BAD_CWD:%s\n' "$TARGET"; exit 0; }}
elif [ "$RAW_CWD" = "~" ]; then
  TARGET="$HOME"
else
  case "$RAW_CWD" in
    "~/"*) TARGET="$HOME/${{RAW_CWD#~/}}" ;;
    *) TARGET="$RAW_CWD" ;;
  esac
fi
if [ ! -d "$TARGET" ]; then
  printf 'ERR:BAD_CWD:%s\n' "$TARGET"
  exit 0
fi
LEX_CWD="$TARGET"
CANON_CWD=$(cd "$TARGET" 2>/dev/null && pwd -P) || {{ printf 'ERR:BAD_CWD:%s\n' "$TARGET"; exit 0; }}
GIT_ROOT=""
_g="$CANON_CWD"
while :; do
  if [ -e "$_g/.git" ]; then
    GIT_ROOT="$_g"
    break
  fi
  [ "$_g" = "/" ] || [ -z "$_g" ] && break
  _parent=${{_g%/*}}
  [ -z "$_parent" ] && _parent="/"
  [ "$_parent" = "$_g" ] && break
  _g="$_parent"
done
AGENT_BIN={agent_bin}
RESOLVED_BIN=$(command -v "$AGENT_BIN" 2>/dev/null || true)
if [ -z "$RESOLVED_BIN" ]; then
  printf 'ERR:MISSING_BIN:%s\n' "$AGENT_BIN"
  exit 0
fi
printf '{pf_meta}\n'
printf 'REMOTE_HOME=%s\n' "$HOME"
printf 'LEX_CWD=%s\n' "$LEX_CWD"
printf 'CANON_CWD=%s\n' "$CANON_CWD"
printf 'GIT_ROOT=%s\n' "$GIT_ROOT"
printf 'RESOLVED_BIN=%s\n' "$RESOLVED_BIN"
printf '\n{pf_win}\n'
{tmux} list-windows -t "="{sess_q} -F '#{{window_name}}' 2>/dev/null || true
printf '\n{pf_team}\n'
_team_root="$CANON_CWD"
[ -n "$GIT_ROOT" ] && _team_root="$GIT_ROOT"
[ -f "$_team_root/.starfane/team.toml" ] && head -c 65536 "$_team_root/.starfane/team.toml" 2>/dev/null || true
printf '\n{pf_cfg}\n'
{agent_cfg_cmd}
"#,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            raw_cwd_q = raw_cwd_q,
            agent_bin = agent_bin,
            pf_meta = PF_META,
            pf_win = PF_WINDOWS,
            pf_team = PF_TEAM_TOML,
            pf_cfg = PF_AGENT_CFG,
            tmux = tmux_cmd,
            sess_q = sess_q,
            agent_cfg_cmd = agent_cfg_cmd,
        )
    }

    fn build_atomic_spawn_script(
        &self,
        tmux_session: &str,
        win_label: &str,
        cwd: &str,
        argv: &[String],
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let sess_q = shell_single_quote(tmux_session);
        let win_q = shell_single_quote(win_label);
        let cwd_q = shell_single_quote(cwd);
        let argv_q = argv
            .iter()
            .map(|a| shell_single_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            r##"{bootstrap}
REMOTE_NOW_S=$(date +%s)
REMOTE_SPAWN_MS=$((REMOTE_NOW_S * 1000))
SF_TAB=$(printf '\t')
FMT="#{{pane_id}}${{SF_TAB}}#{{session_name}}:#{{window_index}}.#{{pane_index}}${{SF_TAB}}#{{session_name}}-#{{window_name}}"
SESS={sess_q}
WIN={win_q}
CWD={cwd_q}
if {tmux} has-session -t "=$SESS" 2>/dev/null; then
  OUT=$({tmux} new-window -d -P -F "$FMT" -n "$WIN" -c "$CWD" -e "STARFANE_SPAWNED_AT_MS=$REMOTE_SPAWN_MS" -t "=$SESS:" -- {argv_q} 2>&1) || {{ printf '%s\n' "$OUT" >&2; exit 11; }}
else
  OUT=$({tmux} new-session -d -P -F "$FMT" -s "$SESS" -n "$WIN" -c "$CWD" -e "STARFANE_SPAWNED_AT_MS=$REMOTE_SPAWN_MS" -- {argv_q} 2>&1)
  _rc=$?
  if [ "$_rc" -ne 0 ]; then
    case "$OUT" in
      *"duplicate session"*)
        OUT=$({tmux} new-window -d -P -F "$FMT" -n "$WIN" -c "$CWD" -e "STARFANE_SPAWNED_AT_MS=$REMOTE_SPAWN_MS" -t "=$SESS:" -- {argv_q} 2>&1) || {{ printf '%s\n' "$OUT" >&2; exit 11; }}
        ;;
      *)
        printf '%s\n' "$OUT" >&2
        exit 12
        ;;
    esac
  fi
fi
PANE_ID=$(printf '%s' "$OUT" | cut -d "$SF_TAB" -f 1)
if ! _roe=$({tmux} set-option -p -t "$PANE_ID" remain-on-exit failed 2>&1); then
  case "$_roe" in
    *"no such pane"*|*"can't find pane"*)
      printf 'command exited immediately; pane %s is gone\n' "$PANE_ID" >&2
      exit 13
      ;;
  esac
fi
printf 'SPAWN_MS=%s\n' "$REMOTE_SPAWN_MS"
printf 'SPAWN_OUT=%s\n' "$OUT"
"##,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            sess_q = sess_q,
            win_q = win_q,
            cwd_q = cwd_q,
            tmux = tmux_cmd,
            argv_q = argv_q,
        )
    }

    fn build_capture_pane_script(&self, local_pane: &PaneId, req: &CaptureRequest) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let pane_q = shell_single_quote(local_pane.as_str());
        if let Some(before) = req.before_line {
            let limit = req.lines.clamp(1, MAX_FULL_CAPTURE_LINES);
            format!(
                r##"{bootstrap}
SF_TAB=$(printf '\t')
SF_US=$(printf '\037')
PANE={pane_q}
META=$({tmux} display-message -p -t "$PANE" "#{{history_size}}${{SF_TAB}}#{{pane_height}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{pane_dead}}" 2>&1) || {{ printf 'ERR:TMUX:%s\n' "$META"; exit 0; }}
printf 'META:%s\n' "$META"
META_US=$(printf '%s\n' "$META" | LC_ALL=C tr '\t' '\037')
IFS="$SF_US" read -r HS PH IM PD <<__SF_META_EOF__
$META_US
__SF_META_EOF__
TOT=$((HS + PH))
END_EXCL={before}
[ "$END_EXCL" -gt "$TOT" ] && END_EXCL=$TOT
if [ "$END_EXCL" -le 0 ]; then
  printf 'EMPTY:1\n'
  exit 0
fi
START_IDX=$((END_EXCL - {limit}))
[ "$START_IDX" -lt 0 ] && START_IDX=0
TMUX_S=$((START_IDX - HS))
TMUX_E=$((END_EXCL - 1 - HS))
printf 'START_IDX:%s\n' "$START_IDX"
printf '__SF_CAP_BODY__\n'
{tmux} capture-pane -p -t "$PANE" -S "$TMUX_S" -E "$TMUX_E" || exit 3
"##,
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                pane_q = pane_q,
                tmux = tmux_cmd,
                before = before,
                limit = limit,
            )
        } else {
            let limit = if req.all {
                if req.lines == 0 {
                    MAX_FULL_CAPTURE_LINES
                } else {
                    req.lines.clamp(1, MAX_FULL_CAPTURE_LINES)
                }
            } else {
                req.lines.clamp(1, MAX_FULL_CAPTURE_LINES)
            };
            format!(
                r##"{bootstrap}
SF_TAB=$(printf '\t')
SF_US=$(printf '\037')
PANE={pane_q}
META=$({tmux} display-message -p -t "$PANE" "#{{history_size}}${{SF_TAB}}#{{pane_height}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{pane_dead}}" 2>&1) || {{ printf 'ERR:TMUX:%s\n' "$META"; exit 0; }}
printf 'META:%s\n' "$META"
META_US=$(printf '%s\n' "$META" | LC_ALL=C tr '\t' '\037')
IFS="$SF_US" read -r HS PH IM PD <<__SF_META_EOF__
$META_US
__SF_META_EOF__
TOT=$((HS + PH))
if [ "$TOT" -le {limit} ]; then
  START_IDX=0
  S_ARG="-"
else
  START_IDX=$((TOT - {limit}))
  S_ARG=$((START_IDX - HS))
fi
printf 'START_IDX:%s\n' "$START_IDX"
printf '__SF_CAP_BODY__\n'
{tmux} capture-pane -p -t "$PANE" -S "$S_ARG" || exit 3
"##,
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                pane_q = pane_q,
                tmux = tmux_cmd,
                limit = limit,
            )
        }
    }

    fn build_dispatch_script(
        &self,
        local_pane: &PaneId,
        agent: Option<AgentKind>,
        effective_settle_ms: u64,
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let pane_q = shell_single_quote(local_pane.as_str());
        let settle_cmd = if effective_settle_ms == 0 {
            "true".to_string()
        } else {
            format!(
                "sleep {:.3} 2>/dev/null || sleep 1",
                effective_settle_ms as f64 / 1000.0
            )
        };
        let modal_block_snippet = match agent {
            Some(AgentKind::Shell) => String::new(),
            Some(AgentKind::Codex) => r#"_pre_lc=$(printf '%s' "$PRE" | tr 'A-Z' 'a-z')
_has_chat=0
case "$_pre_lc" in
  *"? for shortcuts"*|*"esc to interrupt"*|*"ctrl+c to interrupt"*|*"context left"*|*"ask codex to do anything"*) _has_chat=1 ;;
esac
if [ "$_has_chat" = "0" ]; then
  case "$_pre_lc" in
    *"trust this folder?"*|*"do you trust the contents of this directory?"*|*"do you trust the contents of this folder?"*|*"folder access"*|*"trust and continue"*|*"update available"*)
      printf 'STATUS:MODAL_BLOCK\n__SF_PRE__\n%s\n__SF_PRE_END__\n' "$PRE"
      exit 0
      ;;
  esac
fi"#
                .to_string(),
            Some(AgentKind::Claude) => r#"_pre_lc=$(printf '%s' "$PRE" | tr 'A-Z' 'a-z')
_has_chat=0
case "$_pre_lc" in
  *"? for shortcuts"*|*"esc to interrupt"*|*"ctrl+c to interrupt"*|*"context left"*) _has_chat=1 ;;
esac
if [ "$_has_chat" = "0" ]; then
  case "$_pre_lc" in
    *"do you trust the files in this folder?"*|*"is this a project you created or one you trust?"*|*"quick safety check"*|*"yes, i trust this folder"*)
      printf 'STATUS:MODAL_BLOCK\n__SF_PRE__\n%s\n__SF_PRE_END__\n' "$PRE"
      exit 0
      ;;
  esac
fi"#
                .to_string(),
            Some(AgentKind::Agy) => r#"_pre_lc=$(printf '%s' "$PRE" | tr 'A-Z' 'a-z')
_has_chat=0
case "$_pre_lc" in
  *"? for shortcuts"*|*"esc to interrupt"*|*"ctrl+c to interrupt"*|*"context left"*) _has_chat=1 ;;
esac
if [ "$_has_chat" = "0" ]; then
  case "$_pre_lc" in
    *"do you trust the contents of this project?"*|*"trust this workspace"*|*"trust this folder"*|*"yes, i trust this folder"*)
      printf 'STATUS:MODAL_BLOCK\n__SF_PRE__\n%s\n__SF_PRE_END__\n' "$PRE"
      exit 0
      ;;
  esac
fi"#
                .to_string(),
            None => String::new(),
        };

        format!(
            r##"{bootstrap}
SF_TAB=$(printf '\t')
SF_US=$(printf '\037')
PANE={pane_q}
META=$({tmux} display-message -p -t "$PANE" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{pane_mode}}${{SF_TAB}}#{{pane_synchronized}}${{SF_TAB}}#{{cursor_y}}${{SF_TAB}}#{{pane_current_command}}" </dev/null 2>&1)
if [ $? -ne 0 ]; then
  printf 'STATUS:MISSING:%s\n' "$META"
  exit 0
fi
META_US=$(printf '%s\n' "$META" | LC_ALL=C tr '\t' '\037')
IFS="$SF_US" read -r P_DEAD P_INMODE P_MODE P_SYNC P_CY P_CMD <<__SF_DM_EOF__
$META_US
__SF_DM_EOF__
if [ "$P_DEAD" = "1" ]; then
  printf 'STATUS:GATE_FAILED:dead\n'
  exit 0
fi
if [ "$P_SYNC" = "1" ]; then
  printf 'STATUS:GATE_FAILED:synchronized\n'
  exit 0
fi
EXITED_COPY=0
if [ "$P_INMODE" = "1" ]; then
  case "$P_MODE" in
    ""|"copy-mode"|"view-mode")
      {tmux} send-keys -t "$PANE" -X cancel </dev/null 2>/dev/null || true
      sleep 0.04 </dev/null 2>/dev/null || true
      P_INMODE=$({tmux} display-message -p -t "$PANE" '#{{pane_in_mode}}' </dev/null 2>/dev/null || printf '1')
      if [ "$P_INMODE" = "1" ]; then
        printf 'STATUS:GATE_FAILED:mode\n'
        exit 0
      fi
      EXITED_COPY=1
      ;;
    *)
      printf 'STATUS:GATE_FAILED:mode\n'
      exit 0
      ;;
  esac
fi
PRE=$({tmux} capture-pane -p -t "$PANE" </dev/null 2>/dev/null || true)
{modal_block}
BUF="sf_$$_$(date +%s)"
if ! {tmux} load-buffer -b "$BUF" - ; then
  printf 'STATUS:LOAD_ERR\n'
  exit 0
fi
if ! {tmux} paste-buffer -d -p -b "$BUF" -t "$PANE" </dev/null ; then
  {tmux} delete-buffer -b "$BUF" </dev/null 2>/dev/null || true
  printf 'STATUS:PASTE_ERR\n'
  exit 0
fi
{settle_cmd}
if ! {tmux} send-keys -t "$PANE" C-m </dev/null ; then
  printf 'STATUS:ENTER_ERR\n'
  exit 0
fi
sleep 0.22 </dev/null 2>/dev/null || true
POST_META=$({tmux} display-message -p -t "$PANE" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{cursor_y}}" </dev/null 2>/dev/null || printf 'MISSING')
POST=$({tmux} capture-pane -p -t "$PANE" </dev/null 2>/dev/null || true)
printf 'STATUS:OK:%s:%s\nPOST_META:%s\n__SF_PRE__\n%s\n__SF_PRE_END__\n__SF_POST__\n%s\n__SF_POST_END__\n' "$EXITED_COPY" "$P_CMD" "$POST_META" "$PRE" "$POST"
"##,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            pane_q = pane_q,
            tmux = tmux_cmd,
            modal_block = modal_block_snippet,
            settle_cmd = settle_cmd,
        )
    }

    fn build_enter_retry_script(
        &self,
        local_pane: &PaneId,
        settle_sec: &str,
        observe_sec: &str,
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let pane_q = shell_single_quote(local_pane.as_str());
        format!(
            r##"{bootstrap}
SF_TAB=$(printf '\t')
PANE={pane_q}
sleep {settle_sec} 2>/dev/null || true
{tmux} send-keys -t "$PANE" C-m || {{ printf 'POST_META:MISSING\n'; exit 0; }}
sleep {observe_sec} 2>/dev/null || true
POST_META=$({tmux} display-message -p -t "$PANE" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{cursor_y}}" 2>/dev/null || printf 'MISSING')
POST=$({tmux} capture-pane -p -t "$PANE" 2>/dev/null || true)
printf 'POST_META:%s\n__SF_POST__\n%s\n__SF_POST_END__\n' "$POST_META" "$POST"
"##,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            pane_q = pane_q,
            settle_sec = settle_sec,
            observe_sec = observe_sec,
            tmux = tmux_cmd,
        )
    }

    async fn run_sync_inner(
        &self,
        display_name: &str,
        claimed_external_ids: &HashSet<String>,
        bypass_backoff: bool,
    ) -> Result<NodeSnapshot, String> {
        *self
            .last_display_name
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = display_name.to_string();

        let now_before = id::now_ms();
        if !bypass_backoff {
            let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(until) = cb.active_backoff_until(now_before) {
                let rem = until.saturating_sub(now_before).div_ceil(1_000);
                return Err(cb
                    .last_error()
                    .map(|e| format!("{e} (backoff {rem}s)"))
                    .unwrap_or_else(|| format!("host {} in backoff ({rem}s)", self.host_id)));
            }
        }

        let _permit = if bypass_backoff {
            self.interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "ssh semaphore closed".to_string())?
        } else {
            self.io_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "ssh semaphore closed".to_string())?
        };

        let (cached_fps, prev_agy_sig, prev_cx_sig, known_tr_paths) = {
            let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            (
                ds.cached_tail_fingerprints(),
                ds.agy_db_sig.clone(),
                ds.codex_db_sig.clone(),
                ds.known_transcript_paths(),
            )
        };

        let script = self.compound_discover_script_v2(
            &cached_fps,
            prev_agy_sig.as_deref(),
            prev_cx_sig.as_deref(),
            &known_tr_paths,
        );
        let transport = self.transport.clone();
        let host_id = self.host_id.clone();

        let exec_res = tokio::task::spawn_blocking(move || transport.run_idempotent_shell(&script))
            .await
            .map_err(|e| format!("ssh task join: {e}"));

        let now_hub = id::now_ms();
        let out = match exec_res {
            Ok(Ok(o)) if o.success => o,
            Ok(Ok(o)) => {
                let msg = format!(
                    "ssh {} failed (exit {:?}): {}",
                    host_id,
                    o.code,
                    o.stderr.trim()
                );
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
            Err(msg) => {
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
        };

        let parsed = match parse_ssh_compound_v2_output(&out.stdout, now_hub) {
            Ok(p) => p,
            Err(msg) => {
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
        };

        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_success();

        let (mut sessions, events, need_mirror, remote_home) = {
            let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            let rec = reconcile_ssh_discovery_v2(
                &mut ds,
                &self.host_id,
                display_name,
                parsed,
                self.include_shell_panes,
                claimed_external_ids,
                now_hub,
            );
            let rh = ds.remote_home.clone().unwrap_or_default();
            (rec.sessions, rec.events, rec.need_mirror, rh)
        };

        // Incrementally mirror active transcripts whose `(ino, size)` grew and compute
        // `SessionMetrics`, `model`, and `effort` via `TranscriptIndexCache`.
        if !need_mirror.is_empty() && !remote_home.is_empty() {
            let mirror = self.mirror.clone();
            let tr_cache = self.transcript_index.clone();
            let ssh = self.transport.clone();
            let hid = self.host_id.clone();
            let rh = remote_home.clone();
            let synced_telemetry = tokio::task::spawn_blocking(move || {
                let mut out = Vec::with_capacity(need_mirror.len());
                for (pane, agent, remote_path, stat) in need_mirror {
                    if RemoteTranscriptMirror::validate_remote_transcript_path(&rh, &remote_path)
                        .is_err()
                    {
                        continue;
                    }
                    match mirror.sync_telemetry(
                        &ssh,
                        &tr_cache,
                        hid.as_str(),
                        &rh,
                        agent,
                        &remote_path,
                        Some(stat),
                        now_hub,
                    ) {
                        Ok((metrics, model, effort, _sync)) => {
                            out.push((pane, agent, remote_path, stat, metrics, model, effort));
                        }
                        Err(e) => {
                            log::debug!(
                                "ssh {} background transcript mirror failed for {}: {e}",
                                hid,
                                remote_path
                            );
                        }
                    }
                }
                out
            })
            .await
            .unwrap_or_default();

            if !synced_telemetry.is_empty() {
                let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                for (pane, agent, remote_path, stat, tr_metrics, tr_model, tr_effort) in
                    synced_telemetry
                {
                    let valid_model = tr_model
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| {
                            !s.is_empty()
                                && *s != "<synthetic>"
                                && !(agent == AgentKind::Agy && *s == "gemini-flash-worker-agy")
                        })
                        .map(str::to_string);
                    let valid_effort = tr_effort.or_else(|| {
                        (agent == AgentKind::Agy)
                            .then(|| {
                                valid_model
                                    .as_deref()
                                    .and_then(extract_agy_effort_from_model)
                            })
                            .flatten()
                    });
                    if let Some(entry) = ds.resolved_by_pane.get_mut(&pane) {
                        if entry.path == remote_path {
                            entry.last_stat = Some(stat);
                            entry.metrics = tr_metrics.clone();
                            if valid_model.is_some() {
                                entry.model = valid_model.clone();
                            }
                            if valid_effort.is_some() {
                                entry.effort = valid_effort.clone();
                            }
                        }
                    }
                    let local_sid = legacy_session_id(&pane);
                    let qualified_sid = qualify_host_session(&self.host_id, &local_sid);
                    if let Some(rec) = sessions.iter_mut().find(|s| s.session_id == qualified_sid) {
                        if let Some(ref m) = valid_model {
                            rec.model = Some(m.clone());
                        }
                        if let Some(ref e) = valid_effort {
                            rec.effort = Some(e.clone());
                        }
                        let st_metrics = rec
                            .metrics
                            .as_ref()
                            .filter(|m| m.source == "status_line")
                            .cloned();
                        let merged = merge_metrics(tr_metrics.clone(), st_metrics);
                        if merged.is_some() {
                            rec.metrics = merged.clone();
                        }
                        if let Some(lk) = ds.session_lookup.get_mut(&local_sid) {
                            lk.latest_stat = Some(stat);
                            lk.metrics = rec.metrics.clone();
                        }
                    }
                }
            }
        }

        let count = sessions
            .iter()
            .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
            .count();
        Ok(NodeSnapshot {
            host: HostRecord {
                id: self.host_id.clone(),
                display_name: display_name.to_string(),
                kind: HostKind::Ssh,
                status: HostStatus::Online,
                active_endpoint: Some(self.transport.target().to_string()),
                last_seen_ms: Some(now_hub),
                error: None,
                session_count: count,
                rtt_ms: self.transport.rtt_ms(),
                next_retry_ms: None,
            },
            sessions,
            events,
        })
    }
}

/// Legacy v1 compound discovery parser kept for backward compatibility with existing callers/tests.
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

#[derive(Debug)]
struct ParsedPreflight {
    remote_home: String,
    lex_cwd: String,
    canon_cwd: String,
    git_root: Option<String>,
    resolved_bin: String,
    existing_windows: Vec<String>,
    team_toml: Option<String>,
    agent_cfg: Option<String>,
}

fn parse_spawn_preflight_output(
    host_id: &HostId,
    stdout: &str,
) -> Result<ParsedPreflight, ApiError> {
    let clean = stdout.replace("\r\n", "\n");
    for line in clean.lines() {
        if let Some(p) = line.strip_prefix("ERR:BAD_CWD:") {
            return Err(ApiError::bad_request(format!(
                "working directory does not exist on {host_id}: {p}"
            )));
        }
        if let Some(b) = line.strip_prefix("ERR:MISSING_BIN:") {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                format!("agent binary {b:?} not found on remote host {host_id}"),
            ));
        }
    }
    let (_, after_meta) = split_marker_line_first(&clean, PF_META).ok_or_else(|| {
        ApiError::new(
            ErrorCode::Unavailable,
            format!("ssh {host_id} spawn preflight missing metadata"),
        )
    })?;
    let (meta_sec, after_win) = split_marker_line_first(after_meta, PF_WINDOWS)
        .ok_or_else(|| ApiError::internal("spawn preflight missing windows section"))?;
    let (win_sec, after_team) = split_marker_line_first(after_win, PF_TEAM_TOML)
        .ok_or_else(|| ApiError::internal("spawn preflight missing team.toml section"))?;
    let (team_sec, cfg_sec) = split_marker_line_first(after_team, PF_AGENT_CFG)
        .ok_or_else(|| ApiError::internal("spawn preflight missing agent config section"))?;

    let mut remote_home = String::new();
    let mut lex_cwd = String::new();
    let mut canon_cwd = String::new();
    let mut git_root = None;
    let mut resolved_bin = String::new();
    for line in meta_sec.lines() {
        if let Some(v) = line.strip_prefix("REMOTE_HOME=") {
            remote_home = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("LEX_CWD=") {
            lex_cwd = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("CANON_CWD=") {
            canon_cwd = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("GIT_ROOT=") {
            let t = v.trim();
            if !t.is_empty() {
                git_root = Some(t.to_string());
            }
        } else if let Some(v) = line.strip_prefix("RESOLVED_BIN=") {
            resolved_bin = v.trim().to_string();
        }
    }
    if lex_cwd.is_empty() || canon_cwd.is_empty() || resolved_bin.is_empty() {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            format!("incomplete spawn preflight from remote host {host_id}"),
        ));
    }
    let existing_windows = win_sec
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let team_toml = (!team_sec.trim().is_empty()).then(|| team_sec.to_string());
    let agent_cfg = (!cfg_sec.trim().is_empty()).then(|| cfg_sec.to_string());
    Ok(ParsedPreflight {
        remote_home,
        lex_cwd,
        canon_cwd,
        git_root,
        resolved_bin,
        existing_windows,
        team_toml,
        agent_cfg,
    })
}

fn resolve_remote_project_arg(
    project_root: Option<&str>,
    known_host_projects: &[String],
) -> Result<String, ApiError> {
    let Some(trimmed) = project_root.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(String::new());
    };
    if trimmed.contains(['\0', '\n', '\r']) {
        return Err(ApiError::bad_request(
            "project_root cannot contain control characters",
        ));
    }
    if trimmed.starts_with('/') || trimmed.starts_with('~') {
        return Ok(trimmed.to_string());
    }
    // Short project name: match against known remote host projects (exact basename or unique match).
    let lower = trimmed.to_ascii_lowercase();
    let exact: Vec<&String> = known_host_projects
        .iter()
        .filter(|p| {
            Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(trimmed))
        })
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    let sub: Vec<&String> = known_host_projects
        .iter()
        .filter(|p| p.to_ascii_lowercase().contains(&lower))
        .collect();
    if sub.len() == 1 {
        return Ok(sub[0].clone());
    }
    Err(ApiError::bad_request(format!(
        "unknown remote project_root {trimmed:?} (pass an absolute path or ~/...)"
    )))
}

fn build_remote_bare_interactive_cmd(
    agent: AgentKind,
    bin: &str,
    cwd: &str,
    model: Option<&str>,
    effort: Option<Effort>,
    name: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut effective_extra = extra.to_vec();
    starfane_adapters::ensure_bypass_permission_arg(agent, &mut effective_extra);
    let mut argv = vec![bin.to_string()];
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    match agent {
        AgentKind::Claude => {
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(m.into());
            }
            if let Some(e) = effort {
                argv.push("--effort".into());
                argv.push(e.as_str().into());
            }
            if let Some(n) = name {
                argv.push("-n".into());
                argv.push(n.into());
            }
            argv.extend(effective_extra);
        }
        AgentKind::Codex => {
            if let Some(m) = model {
                argv.push("-m".into());
                argv.push(m.into());
            }
            if let Some(e) = effort {
                let mapped = match e {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh | Effort::Max => "xhigh",
                };
                argv.push("-c".into());
                argv.push(format!("model_reasoning_effort=\"{mapped}\""));
            }
            argv.push("-C".into());
            argv.push(cwd.into());
            argv.extend(effective_extra);
        }
        AgentKind::Agy => {
            let fixed = model.is_some_and(starfane_adapters::is_fixed_effort_agy_model);
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(starfane_adapters::resolve_agy_model_id(m, effort));
            }
            if let Some(e) = effort {
                if !fixed {
                    argv.push("--effort".into());
                    argv.push(e.as_str().into());
                }
            }
            argv.extend(effective_extra);
        }
        AgentKind::Shell => {}
    }
    argv
}

fn has_codex_mcp_env_vars_arg(extra: &[String]) -> bool {
    let mut it = extra.iter();
    while let Some(arg) = it.next() {
        let a = arg.trim();
        if a == "-c" || a == "--config" {
            if it
                .next()
                .is_some_and(|v| v.trim_start().starts_with("mcp_servers.starfane.env_vars"))
            {
                return true;
            }
        } else if let Some(rest) = a.strip_prefix("--config=") {
            if rest
                .trim_start()
                .starts_with("mcp_servers.starfane.env_vars")
            {
                return true;
            }
        }
    }
    false
}

fn parse_capture_pane_output(
    host_id: &HostId,
    local_pane: &PaneId,
    req: &CaptureRequest,
    stdout: &str,
) -> Result<PanePreview, ApiError> {
    let clean = stdout.replace("\r\n", "\n");
    let first_line = clean.lines().next().unwrap_or("");
    if let Some(err) = first_line.strip_prefix("ERR:TMUX:") {
        let lower = err.to_ascii_lowercase();
        if lower.contains("not found")
            || lower.contains("can't find pane")
            || lower.contains("no such pane")
        {
            return Err(ApiError::not_found(err.trim()));
        }
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            format!("ssh capture {host_id}:{local_pane}: {}", err.trim()),
        ));
    }
    let meta_raw = first_line
        .strip_prefix("META:")
        .ok_or_else(|| ApiError::internal("ssh capture missing META header"))?;
    let parts = starfane_tmux::split_tmux_fields(meta_raw);
    if parts.len() != 4 {
        return Err(ApiError::internal(format!(
            "unexpected ssh capture meta: {meta_raw:?}"
        )));
    }
    let history_size = parts[0].trim().parse::<usize>().unwrap_or(0);
    let pane_height = parts[1].trim().parse::<usize>().unwrap_or(0);
    let in_mode = parts[2].trim() == "1";
    let dead = parts[3].trim() == "1";
    let total_lines = history_size.saturating_add(pane_height);
    let qualified = qualify_host_pane(host_id, local_pane);

    let (limit, max_bytes, trim_trailing) = if req.before_line.is_some() {
        (
            req.lines.clamp(1, MAX_FULL_CAPTURE_LINES),
            MAX_FULL_CAPTURE_BYTES,
            false,
        )
    } else if req.all {
        let lim = if req.lines == 0 {
            MAX_FULL_CAPTURE_LINES
        } else {
            req.lines.clamp(1, MAX_FULL_CAPTURE_LINES)
        };
        (lim, MAX_FULL_CAPTURE_BYTES, true)
    } else {
        let lim = req.lines.clamp(1, MAX_FULL_CAPTURE_LINES);
        let bytes = if lim > MAX_CAPTURE_LINES {
            MAX_FULL_CAPTURE_BYTES
        } else {
            MAX_CAPTURE_BYTES
        };
        (lim, bytes, true)
    };

    if clean.lines().any(|l| l.trim() == "EMPTY:1") {
        return Ok(PanePreview {
            pane_id: qualified,
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

    let (hdr_part, body_part) = split_marker_line_first(&clean, "__SF_CAP_BODY__")
        .ok_or_else(|| ApiError::internal("ssh capture missing body marker"))?;
    let start_idx = hdr_part
        .lines()
        .find_map(|l| l.strip_prefix("START_IDX:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let end_cap = req
        .before_line
        .map(|b| b.min(total_lines))
        .unwrap_or(total_lines);
    let (text, dropped_top, byte_truncated) =
        cap_page_bytes(body_part, limit, max_bytes, trim_trailing);
    let eff_start = start_idx.saturating_add(dropped_top).min(end_cap);
    let has_more = eff_start > 0;
    let truncated = byte_truncated || (req.all && has_more);
    Ok(PanePreview {
        pane_id: qualified,
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

fn extract_marked_block<'a>(src: &'a str, start_marker: &str, end_marker: &str) -> Option<&'a str> {
    let (_, rest) = split_marker_line_first(src, start_marker)?;
    let (body, _) = split_marker_line_last(rest, end_marker)?;
    Some(body)
}

fn infer_agent_from_cmd(cmd: &str) -> Option<AgentKind> {
    let base = cmd
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or(cmd.trim())
        .to_ascii_lowercase();
    match base.as_str() {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "agy" => Some(AgentKind::Agy),
        other => {
            let parts: Vec<&str> = other.split('.').collect();
            if parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            {
                Some(AgentKind::Claude)
            } else {
                None
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedDispatchRtt1 {
    Missing(String),
    GateFailed(GateCheck, String),
    ModalBlock {
        pre_screen: String,
    },
    InjectErr(String),
    Ok {
        exited_copy_mode: bool,
        pane_cmd: String,
        post_meta: String,
        pre_screen: String,
        post_screen: String,
    },
}

fn parse_dispatch_rtt1_output(stdout: &str) -> Result<ParsedDispatchRtt1, String> {
    let clean = stdout.replace("\r\n", "\n");
    let first = clean.lines().next().unwrap_or("");
    if let Some(m) = first.strip_prefix("STATUS:MISSING:") {
        return Ok(ParsedDispatchRtt1::Missing(m.trim().to_string()));
    }
    if let Some(reason) = first.strip_prefix("STATUS:GATE_FAILED:") {
        return Ok(match reason.trim() {
            "dead" => ParsedDispatchRtt1::GateFailed(GateCheck::PaneAlive, "pane is dead".into()),
            "synchronized" => ParsedDispatchRtt1::GateFailed(
                GateCheck::NotSynchronized,
                "window has synchronize-panes enabled".into(),
            ),
            _ => {
                ParsedDispatchRtt1::GateFailed(GateCheck::NotInMode, "pane is in copy-mode".into())
            }
        });
    }
    if first.trim() == "STATUS:MODAL_BLOCK" {
        let pre_screen = extract_marked_block(&clean, "__SF_PRE__", "__SF_PRE_END__")
            .unwrap_or("")
            .to_string();
        return Ok(ParsedDispatchRtt1::ModalBlock { pre_screen });
    }
    if first.trim() == "STATUS:LOAD_ERR" {
        return Ok(ParsedDispatchRtt1::InjectErr(
            "tmux load-buffer failed".into(),
        ));
    }
    if first.trim() == "STATUS:PASTE_ERR" {
        return Ok(ParsedDispatchRtt1::InjectErr(
            "tmux paste-buffer failed".into(),
        ));
    }
    if first.trim() == "STATUS:ENTER_ERR" {
        return Ok(ParsedDispatchRtt1::InjectErr(
            "tmux send-keys C-m failed".into(),
        ));
    }
    if let Some(rest) = first.strip_prefix("STATUS:OK:") {
        let (exited_raw, pane_cmd) = rest.split_once(':').unwrap_or((rest, ""));
        let exited_copy_mode = exited_raw.trim() == "1";
        let post_meta = clean
            .lines()
            .nth(1)
            .and_then(|l| l.strip_prefix("POST_META:"))
            .unwrap_or("MISSING")
            .trim()
            .to_string();
        let pre_screen = extract_marked_block(&clean, "__SF_PRE__", "__SF_PRE_END__")
            .unwrap_or("")
            .to_string();
        let post_screen = extract_marked_block(&clean, "__SF_POST__", "__SF_POST_END__")
            .unwrap_or("")
            .to_string();
        return Ok(ParsedDispatchRtt1::Ok {
            exited_copy_mode,
            pane_cmd: pane_cmd.trim().to_string(),
            post_meta,
            pre_screen,
            post_screen,
        });
    }
    Err(format!("unexpected ssh dispatch output: {first:?}"))
}

fn parse_post_meta(meta: &str) -> Option<(bool, bool, u32)> {
    if meta.trim() == "MISSING" || meta.trim().is_empty() {
        return None;
    }
    let parts = starfane_tmux::split_tmux_fields(meta);
    if parts.len() != 3 {
        return None;
    }
    let dead = parts[0].trim() == "1";
    let in_mode = parts[1].trim() == "1";
    let cursor_y = parts[2].trim().parse::<u32>().unwrap_or(0);
    Some((dead, in_mode, cursor_y))
}

fn validate_tmux_pane_id(pane: &PaneId) -> Result<(), ApiError> {
    let s = pane.as_str();
    if s.len() > 1 && s.starts_with('%') && s[1..].bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "pane id must look like %12, got {s:?}"
        )))
    }
}

fn validate_stage_filename(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(ApiError::bad_request(format!(
            "invalid stage filename {raw:?}"
        )));
    }
    Ok(name.to_string())
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
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

    fn sync_with_claimed<'a>(
        &'a self,
        display_name: &'a str,
        claimed_external_ids: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        Box::pin(async move {
            self.run_sync_inner(display_name, claimed_external_ids, false)
                .await
        })
    }

    fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, id::now_ms())?;
            }
            let _permit = self
                .io_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh semaphore closed"))?;
            let script = format!(
                "{bootstrap}{tmux} list-sessions -F '#{{session_name}}' 2>/dev/null || true\n",
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                tmux = self.tmux_cmd_prefix(),
            );
            let transport = self.transport.clone();
            let out = tokio::task::spawn_blocking(move || transport.run_idempotent_shell(&script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh list_tmux_sessions join: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh list_sessions: {e}"))
                })?;
            if !out.success {
                let msg = format!(
                    "ssh {} list-sessions failed (exit {:?}): {}",
                    self.host_id,
                    out.code,
                    out.stderr.trim()
                );
                if out.code == Some(255) {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), msg.clone());
                }
                return Err(ApiError::new(ErrorCode::Unavailable, msg));
            }
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();
            let mut sessions: Vec<String> = out
                .stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            sessions.sort();
            sessions.dedup();
            Ok(sessions)
        })
    }

    fn spawn_session<'a>(
        &'a self,
        display_name: &'a str,
        req: &'a RemoteSpawnReq,
    ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>> {
        Box::pin(async move {
            if req.agent.is_shell() {
                return Err(ApiError::bad_request(
                    "cannot spawn plain shell via coding-agent launcher",
                ));
            }
            let target_session = {
                let s = req.tmux_session.trim();
                if s.is_empty() {
                    starfane_tmux::DEFAULT_DETACHED_SESSION.to_string()
                } else {
                    if s.contains([':', '.']) || s.chars().any(char::is_control) {
                        return Err(ApiError::bad_request(format!(
                            "invalid tmux session name {s:?}"
                        )));
                    }
                    s.to_string()
                }
            };
            let explicit_win = match req
                .window_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(w) => {
                    if w.chars().any(char::is_control) {
                        return Err(ApiError::bad_request("window name contains control chars"));
                    }
                    Some(w.to_string())
                }
                None => None,
            };
            let raw_cwd =
                resolve_remote_project_arg(req.project_root.as_deref(), &req.known_host_projects)?;

            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;

            // RTT 1: Read-only remote preflight probe (expands remote $HOME, verifies cwd & binary,
            // lists windows in target tmux session, fetches capped team.toml + codex/agy config).
            let pf_script = self.build_spawn_preflight_script(req.agent, &raw_cwd, &target_session);
            let t1 = self.transport.clone();
            let pf_out = tokio::task::spawn_blocking(move || t1.run_idempotent_shell(&pf_script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh spawn preflight join: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh spawn preflight: {e}"))
                })?;
            if !pf_out.success {
                let msg = format!(
                    "ssh {} spawn preflight failed (exit {:?}): {}",
                    self.host_id,
                    pf_out.code,
                    pf_out.stderr.trim()
                );
                if pf_out.code == Some(255) {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), msg.clone());
                }
                return Err(ApiError::new(ErrorCode::Unavailable, msg));
            }
            let pf = parse_spawn_preflight_output(&self.host_id, &pf_out.stdout)?;

            // Hub Pure Rust Assembly (0 ms): trust.rs remote evaluation + TeamConfig + argv builder.
            let win_label = explicit_win
                .unwrap_or_else(|| next_sequential_window_name(&pf.existing_windows, req.agent));
            let mut extra = pf
                .team_toml
                .as_deref()
                .and_then(|t| TeamConfig::parse(t).ok())
                .and_then(|t| t.spawn_args.get(&req.agent).cloned())
                .unwrap_or_default();
            starfane_adapters::ensure_bypass_permission_arg(req.agent, &mut extra);
            starfane_adapters::ensure_ancestor_trust_args_remote(
                req.agent,
                pf.agent_cfg.as_deref(),
                &pf.lex_cwd,
                &pf.canon_cwd,
                (!pf.remote_home.is_empty()).then_some(pf.remote_home.as_str()),
                &mut extra,
            );
            if req.agent == AgentKind::Codex && !has_codex_mcp_env_vars_arg(&extra) {
                if let Some(ov) = pf
                    .agent_cfg
                    .as_deref()
                    .and_then(starfane_adapters::codex_mcp_env_vars_override_from_toml)
                {
                    extra.push("-c".into());
                    extra.push(ov);
                }
            }

            let effective_model = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|m| {
                    if req.agent == AgentKind::Agy {
                        starfane_adapters::resolve_agy_model_id(m, req.effort)
                    } else {
                        m.to_string()
                    }
                });
            let prompt_clean = req
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| sanitize_line(s, 1000));

            let argv = if let Some(p) = prompt_clean {
                starfane_adapters::build_l4_spawn_cmd(&starfane_adapters::SpawnRequest {
                    agent: req.agent,
                    bin: PathBuf::from(&pf.resolved_bin),
                    cwd: PathBuf::from(&pf.lex_cwd),
                    model: effective_model.clone(),
                    effort: req.effort,
                    name: Some(win_label.clone()),
                    prompt: p,
                    extra_args: extra,
                })
                .map_err(|e| ApiError::bad_request(format!("spawn command: {e}")))?
            } else {
                build_remote_bare_interactive_cmd(
                    req.agent,
                    &pf.resolved_bin,
                    &pf.lex_cwd,
                    effective_model.as_deref(),
                    req.effort,
                    Some(&win_label),
                    &extra,
                )
            };

            let initial_effort = req
                .effort
                .map(|e| e.as_str().to_string())
                .or_else(|| {
                    (req.agent == AgentKind::Agy)
                        .then(|| {
                            effective_model
                                .as_deref()
                                .and_then(extract_agy_effort_from_model)
                        })
                        .flatten()
                })
                .or_else(|| {
                    (req.agent == AgentKind::Codex)
                        .then(|| {
                            pf.agent_cfg
                                .as_deref()
                                .and_then(parse_codex_default_effort_toml)
                        })
                        .flatten()
                });

            // RTT 2: Atomic remote tmux spawn (NON-idempotent `run_shell`, never retried).
            let spawn_script =
                self.build_atomic_spawn_script(&target_session, &win_label, &pf.lex_cwd, &argv);
            let t2 = self.transport.clone();
            let sp_out = tokio::task::spawn_blocking(move || t2.run_shell(&spawn_script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh atomic spawn join: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh spawn: {e}"))
                })?;
            if !sp_out.success {
                let msg = format!(
                    "ssh {} spawn failed (exit {:?}): {}",
                    self.host_id,
                    sp_out.code,
                    sp_out.stderr.trim()
                );
                if sp_out.code == Some(255) {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), msg.clone());
                }
                return Err(ApiError::new(ErrorCode::Unavailable, msg));
            }

            let now_hub = id::now_ms();
            let mut remote_spawn_ms = now_hub;
            let mut spawn_line = None;
            for line in sp_out.stdout.lines() {
                if let Some(v) = line.strip_prefix("SPAWN_MS=") {
                    if let Ok(ms) = v.trim().parse::<u64>() {
                        if ms > 0 {
                            remote_spawn_ms = ms;
                        }
                    }
                } else if let Some(v) = line.strip_prefix("SPAWN_OUT=") {
                    spawn_line = Some(v.trim_end_matches(['\r', '\n']).to_string());
                }
            }
            let raw_out = spawn_line
                .ok_or_else(|| ApiError::internal("ssh atomic spawn missing SPAWN_OUT line"))?;
            let cols = starfane_tmux::split_tmux_fields(&raw_out);
            if cols.len() < 3 {
                return Err(ApiError::internal(format!(
                    "unexpected ssh spawn output: {raw_out:?}"
                )));
            }
            let local_pane = PaneId::new(cols[0].trim());
            validate_tmux_pane_id(&local_pane)?;
            let tmux_target = cols[1].trim().to_string();
            let tmux_label = cols[2].trim().to_string();

            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();

            {
                let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                if !pf.remote_home.is_empty() {
                    ds.remote_home = Some(pf.remote_home);
                }
                ds.recently_closed_panes.remove(&local_pane);
                ds.pane_first_seen_remote_ms
                    .insert(local_pane.clone(), remote_spawn_ms);
                ds.pane_birth_remote_ms
                    .insert(local_pane.clone(), remote_spawn_ms);
                ds.pane_agent.insert(local_pane.clone(), req.agent);
                ds.spawned_panes.insert(
                    local_pane.clone(),
                    SshSpawnedPaneMeta {
                        remote_spawned_at_ms: remote_spawn_ms,
                        hub_spawned_at_ms: now_hub,
                        agent: req.agent,
                        cwd: pf.lex_cwd.clone(),
                        git_root: pf.git_root.clone(),
                        tmux_target: tmux_target.clone(),
                        tmux_label: tmux_label.clone(),
                        model: effective_model.clone(),
                        effort: initial_effort.clone(),
                    },
                );
            }

            let qualified_pane = qualify_host_pane(&self.host_id, &local_pane);
            let qualified_sid =
                qualify_host_session(&self.host_id, &legacy_session_id(&local_pane));
            let session = SessionRecord {
                session_id: qualified_sid.clone(),
                agent: req.agent,
                pane: Some(qualified_pane.clone()),
                pane_resolution: PaneResolution::Env,
                cwd: Some(pf.lex_cwd.clone()),
                git_root: pf.git_root,
                model: effective_model.clone(),
                effort: initial_effort.clone(),
                state: SessionState::Starting,
                state_since_ms: now_hub,
                last_event_id: None,
                last_event_ms: now_hub,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: Some(tmux_target.clone()),
                tmux_label: Some(tmux_label.clone()),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: Some(self.host_id.clone()),
                host_display_name: Some(display_name.to_string()),
                mux_kind: Some("tmux".to_string()),
            };
            let response = serde_json::json!({
                "session_id": qualified_sid,
                "pane_id": qualified_pane,
                "host_id": self.host_id,
                "tmux_target": tmux_target,
                "tmux_label": tmux_label,
                "agent": req.agent,
                "model": effective_model,
                "effort": initial_effort,
                "cwd": pf.lex_cwd,
            });
            Ok(RemoteSpawnOutcome { session, response })
        })
    }

    fn stage_input_files<'a>(
        &'a self,
        files: &'a [(String, PathBuf)],
    ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>> {
        Box::pin(async move {
            let mut result = HashMap::with_capacity(files.len());
            if files.is_empty() {
                return Ok(result);
            }
            for (raw_name, local_path) in files {
                let safe_name = validate_stage_filename(raw_name)?;
                let lp = local_path.clone();
                let bytes = tokio::task::spawn_blocking(move || std::fs::read(&lp))
                    .await
                    .map_err(|e| ApiError::internal(format!("read stage file join: {e}")))?
                    .map_err(|e| {
                        ApiError::bad_request(format!(
                            "cannot read input file {}: {e}",
                            local_path.display()
                        ))
                    })?;
                if bytes.is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "input file {} is empty",
                        local_path.display()
                    )));
                }
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Err(ApiError::bad_request(format!(
                        "input file {} exceeds 10 MiB limit ({} bytes)",
                        local_path.display(),
                        bytes.len()
                    )));
                }
                let cache_key = (safe_name.clone(), bytes.len() as u64, fnv1a_64(&bytes));
                if let Some(hit) = self
                    .staged_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&cache_key)
                {
                    result.insert(local_path.clone(), hit.to_string());
                    continue;
                }

                let _permit = self
                    .interactive_sem
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;
                let expected_len = bytes.len();
                let max_plus_one = MAX_IMAGE_BYTES + 1;
                let script = format!(
                    r#"umask 077
DIR="$HOME/.cache/starfane/inputs"
mkdir -p "$DIR" || exit 10
chmod 700 "$DIR" 2>/dev/null || true
TMP="$DIR/{name}.tmp.$$"
DST="$DIR/{name}"
if ! head -c {max_plus_one} > "$TMP"; then
  rm -f "$TMP" 2>/dev/null || true
  exit 11
fi
SZ=$(wc -c < "$TMP" | tr -d ' \t\n\r')
if [ "$SZ" != "{expected_len}" ] || [ "$SZ" -gt {max_bytes} ]; then
  rm -f "$TMP" 2>/dev/null || true
  printf 'ERR:SIZE:%s\n' "$SZ" >&2
  exit 12
fi
chmod 600 "$TMP" 2>/dev/null || true
mv -f "$TMP" "$DST" || {{ rm -f "$TMP" 2>/dev/null || true; exit 13; }}
printf 'STAGED:%s\n' "$DST"
"#,
                    name = safe_name,
                    max_plus_one = max_plus_one,
                    expected_len = expected_len,
                    max_bytes = MAX_IMAGE_BYTES,
                );
                let transport = self.transport.clone();
                let timeout = transport.timeout().max(Duration::from_secs(15));
                let out = tokio::task::spawn_blocking(move || {
                    transport.run_shell_bytes(&script, Some(&bytes), timeout)
                })
                .await
                .map_err(|e| ApiError::internal(format!("ssh stage file join: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh stage file: {e}"))
                })?;
                if !out.success() {
                    let msg = format!(
                        "ssh {} stage file failed (exit {:?}): {}",
                        self.host_id,
                        out.exit_code,
                        out.stderr.trim()
                    );
                    if out.exit_code == Some(255) {
                        self.circuit
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .record_failure(id::now_ms(), msg.clone());
                    }
                    return Err(ApiError::new(ErrorCode::Unavailable, msg));
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                let remote_path = stdout
                    .lines()
                    .find_map(|l| l.strip_prefix("STAGED:"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ApiError::internal("ssh stage file missing STAGED path"))?
                    .to_string();
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_success();
                self.staged_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(cache_key, remote_path.clone());
                result.insert(local_path.clone(), remote_path);
            }
            Ok(result)
        })
    }

    fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>> {
        Box::pin(async move {
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .reset_for_probe();
            let display = self
                .last_display_name
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let empty = HashSet::new();
            match self.run_sync_inner(&display, &empty, true).await {
                Ok(snap) => Ok(HostProbeOutcome {
                    host_id: self.host_id.clone(),
                    status: snap.host.status,
                    rtt_ms: snap.host.rtt_ms,
                    next_retry_ms: snap.host.next_retry_ms,
                    error: None,
                    snapshot: Some(snap),
                }),
                Err(err) => {
                    let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                    Ok(HostProbeOutcome {
                        host_id: self.host_id.clone(),
                        status: cb.status(),
                        rtt_ms: self.transport.rtt_ms(),
                        next_retry_ms: cb.next_retry_ms(),
                        error: Some(err),
                        snapshot: None,
                    })
                }
            }
        })
    }

    fn rtt_ms(&self) -> Option<u64> {
        self.transport.rtt_ms()
    }

    fn next_retry_ms(&self) -> Option<u64> {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_retry_ms()
    }

    fn circuit_status(&self) -> HostStatus {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status()
    }

    fn capture_pane<'a>(
        &'a self,
        local_pane: &'a PaneId,
        req: &'a CaptureRequest,
    ) -> BoxFuture<'a, Result<PanePreview, ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, id::now_ms())?;
            }
            let _permit = self
                .io_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh semaphore closed"))?;
            let script = self.build_capture_pane_script(local_pane, req);
            let transport = self.transport.clone();
            let out = tokio::task::spawn_blocking(move || transport.run_idempotent_shell(&script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh capture task: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh capture: {e}"))
                })?;
            if !out.success {
                let msg = format!(
                    "ssh {} capture failed (exit {:?}): {}",
                    self.host_id,
                    out.code,
                    out.stderr.trim()
                );
                if out.code == Some(255) {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), msg.clone());
                }
                return Err(ApiError::new(ErrorCode::Unavailable, msg));
            }
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();
            parse_capture_pane_output(&self.host_id, local_pane, req, &out.stdout)
        })
    }

    fn fetch_transcript<'a>(
        &'a self,
        remote_session: &'a SessionId,
        query: &'a TranscriptQuery,
    ) -> BoxFuture<'a, Result<TranscriptResponse, ApiError>> {
        let query = *query;
        Box::pin(async move {
            let now_ms = id::now_ms();
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, now_ms)?;
            }
            let (lookup, remote_home) = {
                let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                (
                    ds.session_lookup.get(remote_session).cloned(),
                    ds.remote_home.clone().unwrap_or_default(),
                )
            };
            let Some(info) = lookup else {
                return Ok(TranscriptResponse {
                    source: "none".to_string(),
                    transcript_path: None,
                    messages: Vec::new(),
                    has_more: false,
                    total_messages: 0,
                    start_seq: 0,
                    next_before: None,
                    metrics: None,
                });
            };
            if remote_home.is_empty() {
                return Ok(TranscriptResponse {
                    source: "none".to_string(),
                    transcript_path: Some(info.remote_path),
                    messages: Vec::new(),
                    has_more: false,
                    total_messages: 0,
                    start_seq: 0,
                    next_before: None,
                    metrics: info.metrics,
                });
            }
            RemoteTranscriptMirror::validate_remote_transcript_path(
                &remote_home,
                &info.remote_path,
            )
            .map_err(ApiError::bad_request)?;

            let _permit = self
                .io_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh semaphore closed"))?;

            let mirror = self.mirror.clone();
            let tr_cache = self.transcript_index.clone();
            let ssh = self.transport.clone();
            let hid = self.host_id.clone();
            let remote_path = info.remote_path.clone();
            let agent = info.agent;
            let latest_stat = info.latest_stat;
            let cached_metrics = info.metrics.clone();

            let page_res = tokio::task::spawn_blocking(move || {
                mirror.read_page_and_metrics(
                    &ssh,
                    &tr_cache,
                    hid.as_str(),
                    &remote_home,
                    agent,
                    &remote_path,
                    latest_stat,
                    query,
                    cached_metrics,
                    now_ms,
                )
            })
            .await
            .map_err(|e| ApiError::internal(format!("ssh fetch_transcript join: {e}")))?;

            match page_res {
                Ok(outcome) => {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_success();
                    if outcome.response.metrics.is_some() {
                        let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(lk) = ds.session_lookup.get_mut(remote_session) {
                            lk.metrics = outcome.response.metrics.clone();
                        }
                    }
                    Ok(outcome.response)
                }
                Err(TmuxError::InvalidRequest(m)) => Err(ApiError::bad_request(m)),
                Err(e) => {
                    let is_transport_fail = matches!(
                        e,
                        TmuxError::Timeout { .. }
                            | TmuxError::Io { .. }
                            | TmuxError::Command {
                                code: Some(255),
                                ..
                            }
                    );
                    if is_transport_fail {
                        self.circuit
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .record_failure(id::now_ms(), e.to_string());
                    }
                    Err(ApiError::new(
                        ErrorCode::Unavailable,
                        format!("ssh transcript {}: {e}", self.host_id),
                    ))
                }
            }
        })
    }

    fn dispatch_interactive<'a>(
        &'a self,
        _remote_session: Option<&'a SessionId>,
        local_pane: &'a PaneId,
        text: &'a str,
        settle_ms: u64,
    ) -> BoxFuture<'a, Result<DeliveryReport, ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            let clean_text = text.replace('\r', "");
            validate_interactive_text(&clean_text)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;

            let qualified = qualify_host_pane(&self.host_id, local_pane);
            let known_agent = self
                .discovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pane_agent
                .get(local_pane)
                .copied();
            let effective_settle = adaptive_paste_settle_ms(settle_ms, &clean_text);

            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;

            // RTT 1: Single compound gate + copy-mode cancel + pre-screen modal guard +
            // bracketed paste + adaptive settle + C-m + post-capture (NEVER auto-retried on exit 255).
            let script = self.build_dispatch_script(local_pane, known_agent, effective_settle);
            let transport = self.transport.clone();
            let payload = clean_text.as_bytes().to_vec();
            let timeout = transport.timeout().max(Duration::from_secs(12));
            let out = tokio::task::spawn_blocking(move || {
                transport.run_shell_bytes(&script, Some(&payload), timeout)
            })
            .await
            .map_err(|e| ApiError::internal(format!("ssh dispatch join: {e}")))?
            .map_err(|e| {
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(id::now_ms(), e.to_string());
                ApiError::new(ErrorCode::Unavailable, format!("ssh dispatch: {e}"))
            })?;

            if !out.success() {
                let msg = format!(
                    "ssh {} dispatch failed (exit {:?}): {}",
                    self.host_id,
                    out.exit_code,
                    out.stderr.trim()
                );
                if out.exit_code == Some(255) {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), msg.clone());
                }
                return Err(ApiError::new(ErrorCode::Unavailable, msg));
            }

            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();

            let stdout = String::from_utf8_lossy(&out.stdout);
            let parsed = parse_dispatch_rtt1_output(&stdout).map_err(ApiError::internal)?;
            let base_checks = vec![
                (GateCheck::PaneAlive, true),
                (GateCheck::NotInMode, true),
                (GateCheck::NotSynchronized, true),
            ];

            match parsed {
                ParsedDispatchRtt1::Missing(detail) => Ok(DeliveryReport {
                    level: DeliveryLevel::L2Inject,
                    status: DeliveryStatus::Failed,
                    gate: Some(GateReport::from_checks(
                        vec![(GateCheck::PaneAlive, false)],
                        Some(detail),
                    )),
                    pane: Some(qualified),
                    ack: None,
                }),
                ParsedDispatchRtt1::GateFailed(failed_check, detail) => {
                    let checks = vec![
                        (GateCheck::PaneAlive, failed_check != GateCheck::PaneAlive),
                        (GateCheck::NotInMode, failed_check != GateCheck::NotInMode),
                        (
                            GateCheck::NotSynchronized,
                            failed_check != GateCheck::NotSynchronized,
                        ),
                    ];
                    Ok(DeliveryReport {
                        level: DeliveryLevel::L2Inject,
                        status: DeliveryStatus::Failed,
                        gate: Some(GateReport::from_checks(checks, Some(detail))),
                        pane: Some(qualified),
                        ack: None,
                    })
                }
                ParsedDispatchRtt1::ModalBlock { pre_screen } => {
                    let reason = known_agent
                        .and_then(|a| detect_blocking_startup_prompt(a, &pre_screen))
                        .unwrap_or("remote agent waiting on interactive startup modal");
                    let mut checks = base_checks;
                    checks.push((GateCheck::InputEmpty, false));
                    Ok(DeliveryReport {
                        level: DeliveryLevel::L2Inject,
                        status: DeliveryStatus::Failed,
                        gate: Some(GateReport::from_checks(checks, Some(reason.to_string()))),
                        pane: Some(qualified),
                        ack: None,
                    })
                }
                ParsedDispatchRtt1::InjectErr(detail) => Ok(DeliveryReport {
                    level: DeliveryLevel::L2Inject,
                    status: DeliveryStatus::Failed,
                    gate: Some(GateReport::from_checks(base_checks, Some(detail))),
                    pane: Some(qualified),
                    ack: None,
                }),
                ParsedDispatchRtt1::Ok {
                    exited_copy_mode,
                    pane_cmd,
                    post_meta,
                    pre_screen,
                    mut post_screen,
                } => {
                    let gate = GateReport::from_checks(base_checks.clone(), None);
                    let Some((post_dead, post_in_mode, mut post_cursor_y)) =
                        parse_post_meta(&post_meta)
                    else {
                        return Ok(DeliveryReport {
                            level: DeliveryLevel::L2Inject,
                            status: DeliveryStatus::Failed,
                            gate: Some(GateReport::from_checks(
                                base_checks,
                                Some(format!("pane {qualified} disappeared during injection")),
                            )),
                            pane: Some(qualified),
                            ack: None,
                        });
                    };
                    if post_dead {
                        return Ok(DeliveryReport {
                            level: DeliveryLevel::L2Inject,
                            status: DeliveryStatus::Failed,
                            gate: Some(GateReport::from_checks(
                                base_checks,
                                Some("pane exited during injection".into()),
                            )),
                            pane: Some(qualified),
                            ack: None,
                        });
                    }
                    if post_in_mode {
                        return Ok(DeliveryReport {
                            level: DeliveryLevel::L2Inject,
                            status: DeliveryStatus::Unconfirmed,
                            gate: Some(GateReport::from_checks(
                                base_checks,
                                Some("pane entered copy-mode during injection".into()),
                            )),
                            pane: Some(qualified),
                            ack: Some("copy_mode_race".into()),
                        });
                    }

                    let tui_agent = known_agent
                        .or_else(|| infer_agent_from_cmd(&pane_cmd))
                        .filter(|a| {
                            matches!(a, AgentKind::Claude | AgentKind::Codex | AgentKind::Agy)
                        });

                    if let (Some(a), false) = (tui_agent, pre_screen.trim().is_empty()) {
                        let mut stuck = is_paste_stuck_in_prompt(
                            a,
                            &pre_screen,
                            &post_screen,
                            post_cursor_y,
                            &clean_text,
                        );
                        let mut retried = false;
                        if stuck {
                            const RETRY_SCHEDULE: [(&str, &str); 2] =
                                [("0.25", "0.26"), ("0.35", "0.28")];
                            for (settle_s, observe_s) in RETRY_SCHEDULE {
                                retried = true;
                                let r_script =
                                    self.build_enter_retry_script(local_pane, settle_s, observe_s);
                                let tr = self.transport.clone();
                                let r_out =
                                    tokio::task::spawn_blocking(move || tr.run_shell(&r_script))
                                        .await
                                        .map_err(|e| {
                                            ApiError::internal(format!("ssh retry join: {e}"))
                                        })?
                                        .map_err(|e| {
                                            ApiError::new(
                                                ErrorCode::Unavailable,
                                                format!("ssh C-m retry: {e}"),
                                            )
                                        })?;
                                if !r_out.success {
                                    break;
                                }
                                let r_clean = r_out.stdout.replace("\r\n", "\n");
                                let r_meta = r_clean
                                    .lines()
                                    .find_map(|l| l.strip_prefix("POST_META:"))
                                    .unwrap_or("MISSING");
                                let Some((r_dead, r_mode, r_cy)) = parse_post_meta(r_meta) else {
                                    return Ok(DeliveryReport {
                                        level: DeliveryLevel::L2Inject,
                                        status: DeliveryStatus::Failed,
                                        gate: Some(GateReport::from_checks(
                                            base_checks,
                                            Some(format!(
                                                "pane {qualified} disappeared during retry"
                                            )),
                                        )),
                                        pane: Some(qualified),
                                        ack: None,
                                    });
                                };
                                if r_dead {
                                    return Ok(DeliveryReport {
                                        level: DeliveryLevel::L2Inject,
                                        status: DeliveryStatus::Failed,
                                        gate: Some(GateReport::from_checks(
                                            base_checks,
                                            Some("pane exited during retry".into()),
                                        )),
                                        pane: Some(qualified),
                                        ack: None,
                                    });
                                }
                                if r_mode {
                                    return Ok(DeliveryReport {
                                        level: DeliveryLevel::L2Inject,
                                        status: DeliveryStatus::Unconfirmed,
                                        gate: Some(GateReport::from_checks(
                                            base_checks,
                                            Some("pane entered copy-mode during retry".into()),
                                        )),
                                        pane: Some(qualified),
                                        ack: Some("copy_mode_race".into()),
                                    });
                                }
                                post_cursor_y = r_cy;
                                post_screen = extract_marked_block(
                                    &r_clean,
                                    "__SF_POST__",
                                    "__SF_POST_END__",
                                )
                                .unwrap_or("")
                                .to_string();
                                stuck = is_paste_stuck_in_prompt(
                                    a,
                                    &pre_screen,
                                    &post_screen,
                                    post_cursor_y,
                                    &clean_text,
                                );
                                if !stuck {
                                    break;
                                }
                            }
                        }
                        if stuck {
                            return Ok(DeliveryReport {
                                level: DeliveryLevel::L2Inject,
                                status: DeliveryStatus::Unconfirmed,
                                gate: Some(GateReport::from_checks(
                                    base_checks,
                                    Some(
                                        "message remained in prompt input box after retry; Enter may not have submitted"
                                            .into(),
                                    ),
                                )),
                                pane: Some(qualified),
                                ack: Some("stuck_in_prompt".into()),
                            });
                        }
                        let suffix = if retried {
                            "retry_confirmed"
                        } else if exited_copy_mode {
                            "exited_copy_mode_confirmed"
                        } else {
                            "confirmed"
                        };
                        return Ok(DeliveryReport {
                            level: DeliveryLevel::L2Inject,
                            status: DeliveryStatus::Delivered,
                            gate: Some(gate),
                            pane: Some(qualified),
                            ack: Some(format!("ssh:{}:{suffix}", self.host_id)),
                        });
                    }

                    let suffix = if exited_copy_mode {
                        "exited_copy_mode_confirmed"
                    } else {
                        "confirmed"
                    };
                    Ok(DeliveryReport {
                        level: DeliveryLevel::L2Inject,
                        status: DeliveryStatus::Delivered,
                        gate: Some(gate),
                        pane: Some(qualified),
                        ack: Some(format!("ssh:{}:{suffix}", self.host_id)),
                    })
                }
            }
        })
    }

    fn focus_pane<'a>(
        &'a self,
        _remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;
            let tmux_cmd = self.tmux_cmd_prefix();
            let pane_q = shell_single_quote(local_pane.as_str());
            let script = format!(
                r##"{bootstrap}
SF_TAB=$(printf '\t')
PANE={pane_q}
META=$({tmux} display-message -p -t "$PANE" "#{{session_name}}${{SF_TAB}}#{{session_name}}:#{{window_index}}.#{{pane_index}}" 2>&1) || {{ printf '%s\n' "$META" >&2; exit 2; }}
{tmux} select-window -t "$PANE" 2>/dev/null || true
{tmux} select-pane -t "$PANE" 2>/dev/null || true
printf '%s\n' "$META"
"##,
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                pane_q = pane_q,
                tmux = tmux_cmd,
            );
            let transport = self.transport.clone();
            let out = tokio::task::spawn_blocking(move || transport.run_shell(&script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh focus task: {e}")))?
                .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("ssh focus: {e}")))?;
            if !out.success {
                let err = out.stderr.trim();
                let lower = err.to_ascii_lowercase();
                if out.code != Some(255)
                    && (lower.contains("can't find pane")
                        || lower.contains("no such pane")
                        || lower.contains("pane not found"))
                {
                    return Err(ApiError::not_found(err));
                }
                return Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("ssh focus: {err}"),
                ));
            }
            let line = out
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .trim_end_matches(['\r', '\n']);
            let parts = starfane_tmux::split_tmux_fields(line);
            if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
                return Err(ApiError::internal(format!(
                    "unexpected ssh focus output: {line:?}"
                )));
            }
            let (session_name, tmux_target) = (parts[0].trim(), parts[1].trim());
            let sock_part = match &self.tmux_socket {
                Some(s) => format!("-L {} ", shell_single_quote(s)),
                None => String::new(),
            };
            let remote_attach = format!(
                "{} {}attach -t {}",
                shell_single_quote(&self.tmux_bin),
                sock_part,
                shell_single_quote(session_name)
            );
            let attach_cmd = format!(
                "ssh -t {} {}",
                shell_single_quote(self.transport.target()),
                shell_single_quote(&remote_attach)
            );
            Ok(serde_json::json!({
                "focused": true,
                "session_name": session_name,
                "tmux_target": tmux_target,
                "attach_command": attach_cmd,
            }))
        })
    }

    fn close_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;
            let script = format!(
                "{bootstrap}{tmux} kill-pane -t {pane}\n",
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                tmux = self.tmux_cmd_prefix(),
                pane = shell_single_quote(local_pane.as_str()),
            );
            let transport = self.transport.clone();
            let out = tokio::task::spawn_blocking(move || transport.run_shell(&script))
                .await
                .map_err(|e| ApiError::internal(format!("ssh close task: {e}")))?
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh close: {e}"))
                })?;

            if !out.success {
                let stderr = out.stderr.trim();
                let lower = stderr.to_ascii_lowercase();
                let pane_already_gone = out.code != Some(255)
                    && (lower.contains("can't find pane")
                        || lower.contains("no such pane")
                        || lower.contains("pane not found")
                        || lower.contains("no server running")
                        || lower.contains("failed to connect to server"));
                if !pane_already_gone {
                    let msg = format!("ssh close (exit {:?}): {stderr}", out.code);
                    if out.code == Some(255) {
                        self.circuit
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .record_failure(id::now_ms(), msg.clone());
                    }
                    return Err(ApiError::new(ErrorCode::Unavailable, msg));
                }
            }

            let now_hub = id::now_ms();
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();
            self.discovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .evict_pane(local_pane, now_hub);

            Ok(serde_json::json!({
                "closed": true,
                "session_id": remote_session,
                "pane": local_pane,
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
    last_display_name: Mutex<String>,
    ownership: NotifyOwnership,
    endpoints: Vec<String>,
    token: Option<String>,
    active_endpoint_idx: Mutex<usize>,
    rtt_ewma_ms: AtomicU64,
    circuit: Mutex<HostCircuitBreaker>,
    staged_cache: Mutex<StagedFileCache>,
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
            circuit: Mutex::new(HostCircuitBreaker::new(host_id.as_str())),
            host_id,
            last_display_name: Mutex::new(display.clone()),
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            endpoints,
            token,
            active_endpoint_idx: Mutex::new(0),
            rtt_ewma_ms: AtomicU64::new(RTT_UNSET),
            staged_cache: Mutex::new(StagedFileCache::default()),
        }
    }

    fn record_rtt(&self, elapsed: Duration) {
        let sample = elapsed.as_millis().min((RTT_UNSET - 1) as u128) as u64;
        let mut prev = self.rtt_ewma_ms.load(Ordering::Acquire);
        loop {
            let next = if prev == RTT_UNSET {
                sample
            } else {
                prev.saturating_mul(3)
                    .saturating_add(sample)
                    .saturating_add(2)
                    / 4
            };
            match self.rtt_ewma_ms.compare_exchange_weak(
                prev,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
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
            let started = Instant::now();
            match http_request_json(method, &url, self.token.as_deref(), body).await {
                Ok(val) => {
                    self.record_rtt(started.elapsed());
                    *self
                        .active_endpoint_idx
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = idx;
                    return Ok((base, val));
                }
                Err(PeerHttpError::Http { status, body })
                    if matches!(status, 400 | 404 | 409 | 422) =>
                {
                    self.record_rtt(started.elapsed());
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

    async fn run_sync_inner(
        &self,
        display_name: &str,
        bypass_backoff: bool,
    ) -> Result<NodeSnapshot, String> {
        *self
            .last_display_name
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = display_name.to_string();
        let now_before = id::now_ms();
        if !bypass_backoff {
            let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(until) = cb.active_backoff_until(now_before) {
                let rem = until.saturating_sub(now_before).div_ceil(1_000);
                return Err(cb
                    .last_error()
                    .map(|e| format!("{e} (backoff {rem}s)"))
                    .unwrap_or_else(|| format!("host {} in backoff ({rem}s)", self.host_id)));
            }
        }

        let (active_ep, env) = match self.request_json("GET", "/api/state", None).await {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(id::now_ms(), msg.clone());
                return Err(msg);
            }
        };
        let data = env.get("data").unwrap_or(&env);
        let raw_sessions = data
            .get("sessions")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let parsed: Vec<SessionRecord> = match serde_json::from_value(raw_sessions) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("parse peer sessions: {e}");
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(id::now_ms(), msg.clone());
                return Err(msg);
            }
        };
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_success();

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
            s.host_display_name = Some(display_name.to_string());
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
                display_name: display_name.to_string(),
                kind: HostKind::Peer,
                status: HostStatus::Online,
                active_endpoint: Some(active_ep),
                last_seen_ms: Some(now),
                error: None,
                session_count: count,
                rtt_ms: self.rtt_ms(),
                next_retry_ms: None,
            },
            sessions: qualified,
            events: Vec::new(),
        })
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

    fn sync_with_claimed<'a>(
        &'a self,
        display_name: &'a str,
        _claimed_external_ids: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        Box::pin(async move { self.run_sync_inner(display_name, false).await })
    }

    fn list_tmux_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, id::now_ms())?;
            }
            let (_, env) = self
                .request_json("GET", "/api/tmux/sessions", None)
                .await
                .map_err(|e| e.into_api_error("peer list_tmux_sessions failed"))?;
            let data = env.get("data").unwrap_or(&env);
            let raw = data
                .get("sessions")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            serde_json::from_value(raw)
                .map_err(|e| ApiError::internal(format!("decode peer tmux sessions: {e}")))
        })
    }

    fn spawn_session<'a>(
        &'a self,
        display_name: &'a str,
        req: &'a RemoteSpawnReq,
    ) -> BoxFuture<'a, Result<RemoteSpawnOutcome, ApiError>> {
        Box::pin(async move {
            // Proxy to peer `/api/sessions/spawn` strictly WITHOUT `host_id` to prevent federation loops.
            let mut body = serde_json::Map::new();
            body.insert(
                "agent".into(),
                serde_json::to_value(req.agent)
                    .map_err(|e| ApiError::internal(format!("encode agent: {e}")))?,
            );
            if let Some(pr) = req
                .project_root
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body.insert("project_root".into(), Value::String(pr.to_string()));
            }
            if !req.tmux_session.trim().is_empty() {
                body.insert(
                    "tmux_session".into(),
                    Value::String(req.tmux_session.trim().to_string()),
                );
            }
            if let Some(m) = req
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body.insert("model".into(), Value::String(m.to_string()));
            }
            if let Some(e) = req.effort {
                body.insert("effort".into(), Value::String(e.as_str().to_string()));
            }
            if let Some(p) = req
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body.insert("prompt".into(), Value::String(p.to_string()));
            }
            if let Some(n) = req
                .window_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body.insert("name".into(), Value::String(n.to_string()));
            }

            let (_, env) = self
                .request_json("POST", "/api/sessions/spawn", Some(&Value::Object(body)))
                .await
                .map_err(|e| e.into_api_error("peer spawn failed"))?;
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_success();
            let mut data = env.get("data").cloned().unwrap_or(env);
            let raw_sid = data
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("peer spawn response missing session_id"))?
                .to_string();
            let raw_pane = data
                .get("pane_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let qualified_sid = qualify_host_session(&self.host_id, &SessionId::new(&raw_sid));
            let qualified_pane = raw_pane
                .as_deref()
                .map(|p| qualify_host_pane(&self.host_id, &PaneId::new(p)));

            if let Some(obj) = data.as_object_mut() {
                obj.insert("session_id".into(), Value::String(qualified_sid.0.clone()));
                if let Some(ref qp) = qualified_pane {
                    obj.insert("pane_id".into(), Value::String(qp.0.clone()));
                }
                obj.insert("host_id".into(), Value::String(self.host_id.0.clone()));
            }

            let now = id::now_ms();
            let session = SessionRecord {
                session_id: qualified_sid,
                agent: req.agent,
                pane: qualified_pane,
                pane_resolution: PaneResolution::Env,
                cwd: data.get("cwd").and_then(Value::as_str).map(str::to_string),
                git_root: None,
                model: data
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                effort: data
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: SessionState::Starting,
                state_since_ms: now,
                last_event_id: None,
                last_event_ms: now,
                last_message: None,
                last_error: None,
                parent_session_id: None,
                is_subagent: false,
                subagent_count: 0,
                active_subagents: 0,
                stage: None,
                unread_inbox: 0,
                confidence: Confidence::Heuristic,
                tmux_target: data
                    .get("tmux_target")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                tmux_label: data
                    .get("tmux_label")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                discovered_only: true,
                transcript_path: None,
                metrics: None,
                host_id: Some(self.host_id.clone()),
                host_display_name: Some(display_name.to_string()),
                mux_kind: Some("tmux".to_string()),
            };
            Ok(RemoteSpawnOutcome {
                session,
                response: data,
            })
        })
    }

    fn stage_input_files<'a>(
        &'a self,
        files: &'a [(String, PathBuf)],
    ) -> BoxFuture<'a, Result<HashMap<PathBuf, String>, ApiError>> {
        Box::pin(async move {
            let mut mapped = HashMap::with_capacity(files.len());
            for (raw_name, local_path) in files {
                let safe_name = validate_stage_filename(raw_name)?;
                let lp = local_path.clone();
                let bytes = tokio::task::spawn_blocking(move || std::fs::read(&lp))
                    .await
                    .map_err(|e| ApiError::internal(format!("read peer upload join: {e}")))?
                    .map_err(|e| {
                        ApiError::bad_request(format!(
                            "cannot read input file {}: {e}",
                            local_path.display()
                        ))
                    })?;
                if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                    return Err(ApiError::bad_request(format!(
                        "input file {} has invalid size ({} bytes)",
                        local_path.display(),
                        bytes.len()
                    )));
                }
                let Some((mime, _ext)) = detect_image_magic(&bytes) else {
                    // Peer `/api/uploads/image` handles images; skip non-image files cleanly.
                    continue;
                };
                let cache_key = (safe_name.clone(), bytes.len() as u64, fnv1a_64(&bytes));
                if let Some(hit) = self
                    .staged_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&cache_key)
                {
                    mapped.insert(local_path.clone(), hit.to_string());
                    continue;
                }
                let data_base64 = encode_base64(&bytes);
                let body = serde_json::json!({
                    "filename": safe_name,
                    "mime": mime,
                    "data_base64": data_base64,
                });
                let (_, env) = self
                    .request_json("POST", "/api/uploads/image", Some(&body))
                    .await
                    .map_err(|e| e.into_api_error("peer image upload failed"))?;
                let data = env.get("data").unwrap_or(&env);
                let peer_path = data
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ApiError::internal("peer image upload missing path"))?
                    .to_string();
                self.staged_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(cache_key, peer_path.clone());
                mapped.insert(local_path.clone(), peer_path);
            }
            Ok(mapped)
        })
    }

    fn probe_now(&self) -> BoxFuture<'_, Result<HostProbeOutcome, ApiError>> {
        Box::pin(async move {
            self.circuit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .reset_for_probe();
            let display = self
                .last_display_name
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            match self.run_sync_inner(&display, true).await {
                Ok(snap) => Ok(HostProbeOutcome {
                    host_id: self.host_id.clone(),
                    status: snap.host.status,
                    rtt_ms: snap.host.rtt_ms,
                    next_retry_ms: snap.host.next_retry_ms,
                    error: None,
                    snapshot: Some(snap),
                }),
                Err(err) => {
                    let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                    Ok(HostProbeOutcome {
                        host_id: self.host_id.clone(),
                        status: cb.status(),
                        rtt_ms: self.rtt_ms(),
                        next_retry_ms: cb.next_retry_ms(),
                        error: Some(err),
                        snapshot: None,
                    })
                }
            }
        })
    }

    fn rtt_ms(&self) -> Option<u64> {
        let v = self.rtt_ewma_ms.load(Ordering::Acquire);
        (v != RTT_UNSET).then_some(v)
    }

    fn next_retry_ms(&self) -> Option<u64> {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_retry_ms()
    }

    fn circuit_status(&self) -> HostStatus {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status()
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
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, id::now_ms())?;
            }
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
            {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                cb.fast_fail_if_offline(&self.host_id, id::now_ms())?;
            }
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

    fn close_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        _local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<serde_json::Value, ApiError>> {
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/close");
        Box::pin(async move {
            let (_, env) = self
                .request_json("POST", &path, Some(&serde_json::json!({})))
                .await
                .map_err(|e| e.into_api_error("peer close failed"))?;
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

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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

    fn pane_row(pane_id: &str, pid: u32, cmd: &str, cwd: &str) -> String {
        let sep = starfane_tmux::TMUX_FIELD_SEP.to_string();
        [
            pane_id,
            "dev",
            "@1",
            "1",
            "0",
            &pid.to_string(),
            cmd,
            cwd,
            "1",
            "1",
            "1",
            "0",
            "0",
            "0",
            "0",
            "0",
            "10",
            "main",
            "",
            "500",
            "40",
        ]
        .join(&sep)
    }

    #[test]
    fn ssh_compound_output_parses_and_qualifies_host_prefix() {
        let row = pane_row("%12", 4242, "claude", "/srv/app");
        let ps_row = "4242 1 claude --model opus\n";
        let combined = format!("{row}\n{SSH_SPLIT_MARKER}\n{ps_row}");
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

    #[test]
    fn circuit_breaker_transitions_backoff_jitter_and_fast_fail() {
        let host = HostId::new("gpu-east");
        let mut cb = HostCircuitBreaker::new(host.as_str());
        assert_eq!(cb.status(), HostStatus::Online);
        assert_eq!(cb.next_retry_ms(), None);
        assert!(cb.fast_fail_if_offline(&host, 10_000).is_ok());

        // Failure #1 -> Degraded, +5s
        let (st1, r1) = cb.record_failure(100_000, "timeout 1");
        assert_eq!(st1, HostStatus::Degraded);
        assert_eq!(r1, 105_000);
        // Degraded does NOT fast-fail background reads (only Offline opens the circuit).
        assert!(cb.fast_fail_if_offline(&host, 101_000).is_ok());

        // Failure #2 -> Degraded, +10s
        let (st2, r2) = cb.record_failure(105_000, "timeout 2");
        assert_eq!(st2, HostStatus::Degraded);
        assert_eq!(r2, 115_000);

        // Failure #3 -> Offline, +15s ±10%
        let (st3, r3) = cb.record_failure(115_000, "timeout 3");
        assert_eq!(st3, HostStatus::Offline);
        let d3 = r3 - 115_000;
        assert!(
            (13_500..=16_500).contains(&d3),
            "expected 15s ±10%, got {d3}ms"
        );
        // Fast-fails immediately in Offline cooldown!
        let err = cb.fast_fail_if_offline(&host, 116_000).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("offline"));

        // Failure #4 -> Offline, +30s ±10%
        let (st4, r4) = cb.record_failure(140_000, "timeout 4");
        assert_eq!(st4, HostStatus::Offline);
        let d4 = r4 - 140_000;
        assert!(
            (27_000..=33_000).contains(&d4),
            "expected 30s ±10%, got {d4}ms"
        );

        // Failure #5 -> Offline, +60s ±10% (capped)
        let (st5, r5) = cb.record_failure(180_000, "timeout 5");
        assert_eq!(st5, HostStatus::Offline);
        let d5 = r5 - 180_000;
        assert!(
            (54_000..=66_000).contains(&d5),
            "expected 60s ±10%, got {d5}ms"
        );

        // reset_for_probe opens gate immediately without losing failure count until probe succeeds.
        cb.reset_for_probe();
        assert_eq!(cb.next_retry_ms(), None);
        assert!(cb.active_backoff_until(181_000).is_none());

        // record_success restores Online cleanly.
        cb.record_success();
        assert_eq!(cb.status(), HostStatus::Online);
        assert_eq!(cb.consecutive_failures(), 0);
        assert_eq!(cb.next_retry_ms(), None);
    }

    #[test]
    fn compound_v2_parses_all_sections_even_with_marker_text_in_tail() {
        let sep = starfane_tmux::FIELD_SEP;
        let p_claude = pane_row("%1", 1001, "claude", "/home/u/repo");
        let p_codex = pane_row("%2", 2002, "codex", "/home/u/app");
        let p_agy = pane_row("%3", 3003, "agy", "/home/u/ws");
        let remote_now_s = 1_760_000_000u64;
        let raw = format!(
            "{SEC_META}\n\
REMOTE_NOW_S={remote_now_s}\n\
REMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n\
{p_claude}\n\
{p_codex}\n\
{p_agy}\n\
\n{SEC_PS}\n\
 1001     1    00:30 claude --model opus\n\
 2002     1    00:15 codex -m gpt-6.2-sol\n\
 3003     1    00:20 /home/u/.local/bin/agy --model gemini-4-neon-high\n\
\n{SEC_TAILS}\n\
__SF_TAIL_BEGIN__ %1\n\
editing nodes.rs with literal marker:\n\
{SEC_RESOLVE}\n\
Claude (1M context) 80% context left\n\
__SF_TAIL_END__ %1\n\
__SF_TAIL_BEGIN__ %2\n\
gpt-6.2-sol · ~/app · 90% left\n\
__SF_TAIL_END__ %2\n\
\n{SEC_RESOLVE}\n\
CLAUDE_SESS 1001 11 120 1759999980\n\
{{\"pid\":1001,\"sessionId\":\"cl-sess-1\",\"cwd\":\"/home/u/repo\",\"startedAt\":1759999972000}}\n\
AGY_LOCK 3003 /home/u/.gemini/antigravity-cli/presence/11111111-2222-3333-4444-555555555555.lock\n\
AGY_DB_SIG 99 4096 1759999990|0 -1 0\n\
AGY_DB_ROOTS_BEGIN\n\
11111111-2222-3333-4444-555555555555{sep}[\"file:///home/u/ws\"]{sep}IDLE{sep}2025-10-09T08:53:10Z{sep}2025-10-09T08:53:05Z{sep}4\n\
AGY_DB_ROOTS_END\n\
AGY_DB_CHILDREN_BEGIN\n\
child-conv-000001{sep}11111111-2222-3333-4444-555555555555{sep}reviewer{sep}RUNNING{sep}0\n\
AGY_DB_CHILDREN_END\n\
CODEX_ROLLOUT 201 512 1759999995 /home/u/.codex/sessions/2025/10/09/rollout-2025-10-09-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl\n\
{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\",\"timestamp\":\"2025-10-09T08:53:10Z\",\"cwd\":\"/home/u/app\"}}}}\n\
\n{SEC_TR_STATS}\n\
101 1024 1759999990 /home/u/.claude/projects/-home-u-repo/cl-sess-1.jsonl\n\
201 512 1759999995 /home/u/.codex/sessions/2025/10/09/rollout-2025-10-09-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl\n\
301 2048 1759999992 /home/u/.gemini/antigravity-cli/brain/11111111-2222-3333-4444-555555555555/.system_generated/logs/transcript.jsonl\n"
        );

        let parsed = parse_ssh_compound_v2_output(&raw, 999).unwrap();
        assert_eq!(parsed.remote_now_ms, remote_now_s * 1_000);
        assert_eq!(parsed.remote_home, "/home/u");
        assert_eq!(parsed.panes.len(), 3);
        assert_eq!(parsed.claude_sessions.len(), 1);
        assert_eq!(parsed.codex_rollouts.len(), 1);
        assert_eq!(parsed.agy_locks_by_pid.get(&3003).unwrap().len(), 1);
        assert_eq!(parsed.tr_stats.len(), 3);

        // Reconcile into SshDiscoveryState -> verifies all 3 agents + AGY subagent + status-line metrics.
        let mut ds = SshDiscoveryState::default();
        let host = HostId::new("gpu");
        let rec = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "GPU",
            parsed,
            false,
            &HashSet::new(),
            500_000,
        );
        let sessions = rec.sessions;
        let need_mirror = rec.need_mirror;
        // 3 root panes + 1 AGY Swarm subagent child = 4 sessions
        assert_eq!(sessions.len(), 4);
        assert_eq!(need_mirror.len(), 3);

        let cl = sessions
            .iter()
            .find(|s| s.session_id.as_str() == "gpu:tmux-%1")
            .unwrap();
        assert_eq!(
            cl.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/-home-u-repo/cl-sess-1.jsonl")
        );
        // Status line fallback picked up 20% used & 1M window!
        assert_eq!(
            cl.metrics.as_ref().and_then(|m| m.context_used_pct),
            Some(20.0)
        );
        assert_eq!(
            cl.metrics.as_ref().and_then(|m| m.context_window_tokens),
            Some(1_000_000)
        );

        let cx = sessions
            .iter()
            .find(|s| s.session_id.as_str() == "gpu:tmux-%2")
            .unwrap();
        assert!(cx
            .transcript_path
            .as_deref()
            .unwrap()
            .contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));

        let agy = sessions
            .iter()
            .find(|s| s.session_id.as_str() == "gpu:tmux-%3")
            .unwrap();
        assert_eq!(agy.subagent_count, 1);
        assert_eq!(agy.active_subagents, 1);
        assert_eq!(agy.effort.as_deref(), Some("high"));

        let kid = sessions.iter().find(|s| s.is_subagent).unwrap();
        assert_eq!(kid.session_id.as_str(), "gpu:child-conv-000001");
        assert_eq!(
            kid.parent_session_id.as_ref().unwrap().as_str(),
            "gpu:tmux-%3"
        );
        assert_eq!(kid.state, SessionState::Working);
    }

    #[test]
    fn remote_clock_birth_isolation_blocks_skew_hijack_and_sibling_steal() {
        let host = HostId::new("remote-gpu");
        let mut ds = SshDiscoveryState::default();
        // Remote host clock is 1_760_000_000s, while Hub clock is arbitrarily 45 seconds behind!
        let remote_now_s = 1_760_000_000u64;
        let hub_now_ms = (remote_now_s - 45) * 1_000;

        // Register a freshly spawned Codex pane at remote_now_s - 2s (hub_now_ms - 2s).
        ds.spawned_panes.insert(
            PaneId::new("%10"),
            SshSpawnedPaneMeta {
                remote_spawned_at_ms: (remote_now_s - 2) * 1_000,
                hub_spawned_at_ms: hub_now_ms - 2_000,
                agent: AgentKind::Codex,
                cwd: "/home/u/.agent_session".into(),
                git_root: None,
                tmux_target: "agent-session:1.0".into(),
                tmux_label: "agent-session-codex-1".into(),
                model: Some("gpt-6.2-sol".into()),
                effort: Some("high".into()),
            },
        );

        // Historical rollout created 60 seconds BEFORE remote spawn (still AFTER Hub's skewed clock!).
        // If Hub clock were mistakenly used, this historical rollout would appear "post-birth"!
        let p10 = pane_row("%10", 5001, "codex", "/home/u/.agent_session");
        let raw_single = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p10}\n\
\n{SEC_PS}\n 5001 1 00:02 codex -m gpt-6.2-sol\n\
\n{SEC_TAILS}\n\
\n{SEC_RESOLVE}\n\
CODEX_ROLLOUT 55 900 {} /home/u/.codex/sessions/2025/10/09/rollout-old-00000000-0000-0000-0000-000000000001.jsonl\n\
{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"00000000-0000-0000-0000-000000000001\",\"timestamp\":\"2025-10-09T08:52:20Z\",\"cwd\":\"/home/u/.agent_session\"}}}}\n\
\n{SEC_TR_STATS}\n\
55 900 {} /home/u/.codex/sessions/2025/10/09/rollout-old-00000000-0000-0000-0000-000000000001.jsonl\n",
            remote_now_s - 60,
            remote_now_s - 60,
        );
        let parsed1 = parse_ssh_compound_v2_output(&raw_single, hub_now_ms).unwrap();
        let sessions1 = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "Remote GPU",
            parsed1,
            false,
            &HashSet::new(),
            hub_now_ms,
        )
        .sessions;
        assert_eq!(sessions1.len(), 1);
        assert_eq!(
            sessions1[0].transcript_path, None,
            "pre-birth historical rollout must NEVER be hijacked despite 45s Hub/Remote clock skew"
        );
        assert_eq!(sessions1[0].model.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(sessions1[0].effort.as_deref(), Some("high"));

        // Two sibling Codex panes in the SAME cwd (`/home/u/.agent_session`) with a post-birth rollout:
        // `unique_in_cwd == false` => NEITHER pane may guess/steal the rollout!
        let p11 = pane_row("%11", 5002, "codex", "/home/u/.agent_session");
        let raw_two = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p10}\n{p11}\n\
\n{SEC_PS}\n 5001 1 00:02 codex -m gpt-6.2-sol\n 5002 1 00:01 codex -m gpt-6.2-sol\n\
\n{SEC_TAILS}\n\
\n{SEC_RESOLVE}\n\
CODEX_ROLLOUT 56 900 {remote_now_s} /home/u/.codex/sessions/2025/10/09/rollout-new-00000000-0000-0000-0000-000000000002.jsonl\n\
{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"00000000-0000-0000-0000-000000000002\",\"timestamp\":\"2025-10-09T08:53:20Z\",\"cwd\":\"/home/u/.agent_session\"}}}}\n\
\n{SEC_TR_STATS}\n\
56 900 {remote_now_s} /home/u/.codex/sessions/2025/10/09/rollout-new-00000000-0000-0000-0000-000000000002.jsonl\n",
        );
        let parsed2 = parse_ssh_compound_v2_output(&raw_two, hub_now_ms + 3_000).unwrap();
        let sessions2 = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "Remote GPU",
            parsed2,
            false,
            &HashSet::new(),
            hub_now_ms + 3_000,
        )
        .sessions;
        assert_eq!(sessions2.len(), 2);
        assert!(
            sessions2.iter().all(|s| s.transcript_path.is_none()),
            "sibling unlocked Codex panes in same cwd must not hijack each other's rollout"
        );
    }

    #[test]
    fn remote_heuristic_tail_state_transitions_trust_update_and_turn_complete() {
        let mut watch = HashMap::new();
        let pane = PaneId::new("%7");
        let qsid = SessionId::new("gpu:tmux-%7");
        let qpane = PaneId::new("gpu:%7");

        // 1. Folder trust prompt -> NeedsPermission + heuristic event
        let (st, _, _, err, ev) = apply_ssh_pane_tail_watch(
            &mut watch,
            &pane,
            &qsid,
            &qpane,
            AgentKind::Codex,
            Some("/home/u/repo"),
            None,
            "Folder access\nTrust this folder?\n› 1. Trust and continue",
            SessionState::Starting,
            1_000,
        );
        assert_eq!(st, SessionState::NeedsPermission);
        assert!(err.is_some());
        assert_eq!(ev.unwrap().kind, EventKind::NeedsPermission);

        // 2. Output changes -> Working
        let (st, _, _, err, ev) = apply_ssh_pane_tail_watch(
            &mut watch,
            &pane,
            &qsid,
            &qpane,
            AgentKind::Codex,
            Some("/home/u/repo"),
            None,
            "Working on your request...\nThinking...",
            SessionState::Idle,
            4_000,
        );
        assert_eq!(st, SessionState::Working);
        assert_eq!(err, None);
        assert!(ev.is_none());

        // 3. Three unchanged ticks -> Idle + TurnComplete heuristic event
        for tick in 1..=2 {
            let (st, _, _, _, ev) = apply_ssh_pane_tail_watch(
                &mut watch,
                &pane,
                &qsid,
                &qpane,
                AgentKind::Codex,
                Some("/home/u/repo"),
                None,
                "Working on your request...\nThinking...",
                SessionState::Idle,
                4_000 + tick * 3_000,
            );
            assert_eq!(st, SessionState::Working);
            assert!(ev.is_none());
        }
        let (st, _, _, _, ev) = apply_ssh_pane_tail_watch(
            &mut watch,
            &pane,
            &qsid,
            &qpane,
            AgentKind::Codex,
            Some("/home/u/repo"),
            None,
            "Working on your request...\nThinking...",
            SessionState::Idle,
            13_000,
        );
        assert_eq!(st, SessionState::Idle);
        assert_eq!(ev.unwrap().kind, EventKind::TurnComplete);
    }

    #[test]
    fn parse_capture_and_dispatch_rtt1_helpers() {
        let host = HostId::new("gpu");
        let pane = PaneId::new("%5");
        for sep in ["\t", "\x1f", "\\037"] {
            let cap_raw = format!(
                "META:100{sep}24{sep}0{sep}0\nSTART_IDX:120\n__SF_CAP_BODY__\nline1\nline2\nline3\n"
            );
            let pv = parse_capture_pane_output(&host, &pane, &CaptureRequest::tail(4), &cap_raw)
                .unwrap();
            assert_eq!(pv.pane_id.as_str(), "gpu:%5");
            assert_eq!(pv.total_lines, 124);
            assert_eq!(pv.start_line, 120);
            assert!(pv.has_more);
            assert_eq!(pv.next_before_line, Some(120));
            assert_eq!(pv.text, "line1\nline2\nline3");

            let disp_raw = format!(
                "STATUS:OK:1:codex\nPOST_META:0{sep}0{sep}10\n__SF_PRE__\n› \n__SF_PRE_END__\n__SF_POST__\nWorking...\n__SF_POST_END__\n"
            );
            match parse_dispatch_rtt1_output(&disp_raw).unwrap() {
                ParsedDispatchRtt1::Ok {
                    exited_copy_mode,
                    pane_cmd,
                    post_meta,
                    pre_screen,
                    post_screen,
                } => {
                    assert!(exited_copy_mode);
                    assert_eq!(pane_cmd, "codex");
                    assert_eq!(parse_post_meta(&post_meta), Some((false, false, 10)));
                    assert_eq!(pre_screen.trim(), "›");
                    assert_eq!(post_screen.trim(), "Working...");
                }
                other => panic!("unexpected parse result: {other:?}"),
            }
        }

        // Base64 roundtrip matches uploads::decode_base64.
        let sample = b"\x89PNG\r\n\x1a\nhello-world-123";
        let b64 = encode_base64(sample);
        assert_eq!(crate::core::uploads::decode_base64(&b64).unwrap(), sample);
    }

    #[test]
    fn compound_v2_parses_tab_and_tmux35a_octal_037_panes() {
        let tmux35a_line = "%1\\037ai-codex\\037@1\\0370\\0370\\0379436\\037zsh\\037/Users/xiling\\0371\\0370\\0370\\0370\\0370\\0370\\0370\\0378\\03746\\037arxiv-chrome-exten\\037\\037774\\03748";
        let tab_line = tmux35a_line.replace("\\037", "\t");
        for pane_line in [&tab_line, &tmux35a_line.to_string()] {
            let raw = format!(
                "{SEC_META}\nREMOTE_NOW_S=1760000000\nREMOTE_HOME=/Users/xiling\n\n\
                 {SEC_PANES}\n{pane_line}\n\n\
                 {SEC_PS}\n 9436 1 05:00 -zsh\n\n\
                 {SEC_TAILS}\n\n{SEC_RESOLVE}\n\n{SEC_TR_STATS}\n"
            );
            let parsed = parse_ssh_compound_v2_output(&raw, 999).unwrap();
            assert_eq!(parsed.panes.len(), 1);
            let p = &parsed.panes[0];
            assert_eq!(p.pane_id.as_str(), "%1");
            assert_eq!(p.session_name, "ai-codex");
            assert_eq!(p.window_name, "arxiv-chrome-exten");
            assert_eq!(p.session_group, "");
            assert_eq!(p.history_size, 774);
            assert_eq!(p.pane_height, 48);
        }
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
        assert!(provider.rtt_ms().is_some());
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

    #[tokio::test]
    async fn peer_provider_proxies_tmux_sessions_spawn_and_image_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let body = if first_line.starts_with("GET /api/tmux/sessions") {
                    serde_json::json!({
                        "ok": true,
                        "data": { "sessions": ["agent-session", "work"] }
                    })
                } else if first_line.starts_with("POST /api/sessions/spawn") {
                    // Verify `host_id` is NOT forwarded to peer (prevents federation loops!).
                    let json_body = req.split("\r\n\r\n").nth(1).unwrap_or("{}");
                    let parsed: Value = serde_json::from_str(json_body).unwrap();
                    assert!(parsed.get("host_id").is_none());
                    serde_json::json!({
                        "ok": true,
                        "data": {
                            "session_id": "tmux-%8",
                            "pane_id": "%8",
                            "tmux_target": "work:2.0",
                            "tmux_label": "work-codex-1",
                            "agent": "codex",
                            "model": "gpt-6.2-sol",
                            "effort": "high",
                            "cwd": "/srv/peer/.agent_session"
                        }
                    })
                } else if first_line.starts_with("POST /api/uploads/image") {
                    serde_json::json!({
                        "ok": true,
                        "data": {
                            "id": "img-1",
                            "path": "/srv/peer/.starfane/inputs/img-1.png",
                            "filename": "shot.png",
                            "mime": "image/png",
                            "size_bytes": 67,
                            "preview_url": "/api/uploads/img-1"
                        }
                    })
                } else {
                    serde_json::json!({ "ok": false, "error": "unexpected path" })
                }
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let cfg = NodeConfig {
            id: "peer-a".into(),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec![format!("http://{live_addr}")],
            ..NodeConfig::default()
        };
        let provider = PeerDaemonNodeProvider::new(&cfg);

        let sessions = provider.list_tmux_sessions().await.unwrap();
        assert_eq!(sessions, vec!["agent-session", "work"]);

        let outcome = provider
            .spawn_session(
                "Peer A",
                &RemoteSpawnReq {
                    agent: AgentKind::Codex,
                    project_root: None,
                    tmux_session: "work".into(),
                    window_name: None,
                    model: Some("gpt-6.2-sol".into()),
                    effort: Some(Effort::High),
                    prompt: None,
                    known_host_projects: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.session.session_id.as_str(), "peer-a:tmux-%8");
        assert_eq!(outcome.session.pane.as_ref().unwrap().as_str(), "peer-a:%8");
        assert_eq!(outcome.response["session_id"], "peer-a:tmux-%8");
        assert_eq!(outcome.response["pane_id"], "peer-a:%8");
        assert_eq!(outcome.response["host_id"], "peer-a");

        // Stage a real 1x1 PNG file to peer `/api/uploads/image`.
        let tmp_img = std::env::temp_dir().join(format!("sf-peer-stage-{}.png", id::new_id("T")));
        let png_bytes = crate::core::uploads::decode_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        )
        .unwrap();
        std::fs::write(&tmp_img, &png_bytes).unwrap();
        let staged = provider
            .stage_input_files(&[("shot.png".into(), tmp_img.clone())])
            .await
            .unwrap();
        assert_eq!(
            staged.get(&tmp_img).map(String::as_str),
            Some("/srv/peer/.starfane/inputs/img-1.png")
        );
        let _ = std::fs::remove_file(&tmp_img);
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

    #[tokio::test]
    async fn ssh_provider_stages_files_atomically_and_mirrors_remote_transcript_with_zero_rtt_cache(
    ) {
        let root = std::env::temp_dir().join(format!("sf-nodes-ssh-test-{}", id::new_id("T")));
        let state_dir = root.join("state");
        let remote_home = root.join("remote_home");
        let fake_ssh = root.join("fake-ssh");
        let calls_log = root.join("ssh_calls.log");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(remote_home.join(".claude/projects/-home-u-demo")).unwrap();

        let tr_path = remote_home.join(".claude/projects/-home-u-demo/cl-e2e-1.jsonl");
        let tr_body = concat!(
            r#"{"type":"user","timestamp":"2026-11-25T10:00:00Z","message":{"role":"user","content":"hello remote ssh"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-25T10:00:05Z","message":{"id":"m1","model":"claude-opus-4-6","stop_reason":"end_turn","content":[{"type":"text","text":"remote reply ok"}],"usage":{"input_tokens":1200,"cache_read_input_tokens":800,"cache_creation_input_tokens":0,"output_tokens":300}}}"#,
            "\n"
        );
        std::fs::write(&tr_path, tr_body).unwrap();

        let script_body = format!(
            r#"#!/bin/sh
printf 'CALL\n' >> "{log}"
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  exit 0
fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then
    shift
    break
  fi
  shift
done
export HOME="{rhome}"
exec /bin/sh -c "$1"
"#,
            log = calls_log.display(),
            rhome = remote_home.display()
        );
        std::fs::write(&fake_ssh, script_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = NodeConfig {
            id: "ssh-box".into(),
            display_name: Some("SSH Box".into()),
            kind: NodeKindConfig::Ssh,
            enabled: true,
            ssh_target: "demo@ssh-box".into(),
            ssh_bin: fake_ssh.to_string_lossy().into_owned(),
            ..NodeConfig::default()
        };
        let provider = SshMuxNodeProvider::with_shared_caches(
            &cfg,
            &state_dir,
            Arc::new(RemoteTranscriptMirror::new(state_dir.clone())),
            Arc::new(Mutex::new(TranscriptIndexCache::default())),
        );

        // 1. Stage a local file via ControlMaster stdin -> 0600 atomic write + dedup cache hit.
        let local_input = root.join("paste-1.md");
        std::fs::write(&local_input, b"hello remote staged paste").unwrap();
        let staged1 = provider
            .stage_input_files(&[("paste-1.md".into(), local_input.clone())])
            .await
            .unwrap();
        let remote_dst = staged1.get(&local_input).unwrap();
        assert_eq!(
            remote_dst,
            &format!(
                "{}/.cache/starfane/inputs/paste-1.md",
                remote_home.display()
            )
        );
        assert_eq!(
            std::fs::read(remote_dst).unwrap(),
            b"hello remote staged paste"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(remote_dst).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let calls_after_stage1 = std::fs::read_to_string(&calls_log).unwrap().lines().count();
        assert_eq!(calls_after_stage1, 1);

        // Second stage of the exact same file hits `staged_cache` -> 0 SSH calls!
        let staged2 = provider
            .stage_input_files(&[("paste-1.md".into(), local_input.clone())])
            .await
            .unwrap();
        assert_eq!(staged2.get(&local_input).unwrap(), remote_dst);
        let calls_after_stage2 = std::fs::read_to_string(&calls_log).unwrap().lines().count();
        assert_eq!(calls_after_stage2, 1);

        // 2. Seed SshDiscoveryState session_lookup with the remote Claude transcript and call
        //    `fetch_transcript`: first call mirrors in 1 SSH RTT; second call with matching stat uses 0 SSH RTTs!
        #[cfg(unix)]
        let tr_stat = {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&tr_path).unwrap();
            RemoteFileStat {
                ino: meta.ino(),
                size: meta.len(),
                mtime_s: meta.mtime(),
            }
        };
        #[cfg(not(unix))]
        let tr_stat = RemoteFileStat {
            ino: 1,
            size: tr_body.len() as u64,
            mtime_s: 1_760_000_000,
        };
        {
            let mut ds = provider.discovery.lock().unwrap();
            ds.remote_home = Some(remote_home.to_string_lossy().into_owned());
            ds.session_lookup.insert(
                SessionId::new("tmux-%9"),
                RemoteSessionLookup {
                    agent: AgentKind::Claude,
                    remote_path: tr_path.to_string_lossy().into_owned(),
                    latest_stat: Some(tr_stat),
                    metrics: None,
                },
            );
        }

        let resp1 = provider
            .fetch_transcript(&SessionId::new("tmux-%9"), &TranscriptQuery::tail(20))
            .await
            .unwrap();
        assert_eq!(resp1.source, "transcript");
        assert_eq!(resp1.messages.len(), 2);
        assert_eq!(resp1.messages[0].text, "hello remote ssh");
        assert_eq!(resp1.messages[1].text, "remote reply ok");
        assert_eq!(
            resp1.metrics.as_ref().and_then(|m| m.context_used_tokens),
            Some(2_000)
        );
        let calls_after_tr1 = std::fs::read_to_string(&calls_log).unwrap().lines().count();

        let resp2 = provider
            .fetch_transcript(&SessionId::new("tmux-%9"), &TranscriptQuery::tail(20))
            .await
            .unwrap();
        assert_eq!(resp2.messages.len(), 2);
        let calls_after_tr2 = std::fs::read_to_string(&calls_log).unwrap().lines().count();
        assert_eq!(
            calls_after_tr2, calls_after_tr1,
            "unchanged remote transcript must hit 0-RTT cache"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_pid_restart_recycled_claude_pid_and_spawn_grace_protection() {
        let host = HostId::new("gpu");
        let mut ds = SshDiscoveryState::default();
        let remote_now_s = 1_760_000_000u64;
        let hub_now_ms = 900_000u64;

        // 1. Freshly spawned pane running `bash` while CLI is still initializing:
        //    LEGACY_SPAWN_GRACE_MS (15s on hub_spawned_at_ms) preserves `Starting` session.
        ds.spawned_panes.insert(
            PaneId::new("%20"),
            SshSpawnedPaneMeta {
                remote_spawned_at_ms: remote_now_s * 1_000,
                hub_spawned_at_ms: hub_now_ms - 3_000,
                agent: AgentKind::Claude,
                cwd: "/home/u/.agent_session".into(),
                git_root: None,
                tmux_target: "agent-session:1.0".into(),
                tmux_label: "agent-session-claude-1".into(),
                model: Some("opus".into()),
                effort: Some("high".into()),
            },
        );
        let p20_shell = pane_row("%20", 8001, "bash", "/home/u/.agent_session");
        let raw_grace = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p20_shell}\n\
\n{SEC_PS}\n 8001 1 00:03 bash\n\
\n{SEC_TAILS}\n\n{SEC_RESOLVE}\n\n{SEC_TR_STATS}\n"
        );
        let parsed_grace = parse_ssh_compound_v2_output(&raw_grace, hub_now_ms).unwrap();
        let res_grace = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "GPU",
            parsed_grace,
            false,
            &HashSet::new(),
            hub_now_ms,
        );
        assert_eq!(res_grace.sessions.len(), 1);
        assert_eq!(res_grace.sessions[0].state, SessionState::Starting);
        assert_eq!(res_grace.sessions[0].model.as_deref(), Some("opus"));

        // 2. Recycled Claude PID: `<pid>.json` has `startedAt` 10 minutes older than OS pid birth
        //    (> 120s skew) -> MUST be rejected so stale dead Claude transcript is not attached!
        let p21 = pane_row("%21", 9001, "claude", "/home/u/repo");
        let stale_started_ms = (remote_now_s - 600) * 1_000;
        let raw_recycled = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p21}\n\
\n{SEC_PS}\n 9001 1 00:05 claude --model opus\n\
\n{SEC_TAILS}\n\
\n{SEC_RESOLVE}\n\
CLAUDE_SESS 9001 12 100 {}\n\
{{\"pid\":9001,\"sessionId\":\"stale-cl\",\"cwd\":\"/home/u/repo\",\"startedAt\":{stale_started_ms}}}\n\
\n{SEC_TR_STATS}\n\
77 500 {} /home/u/.claude/projects/-home-u-repo/stale-cl.jsonl\n",
            remote_now_s - 600,
            remote_now_s - 600,
        );
        let parsed_recycled =
            parse_ssh_compound_v2_output(&raw_recycled, hub_now_ms + 2_000).unwrap();
        let res_recycled = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "GPU",
            parsed_recycled,
            false,
            &HashSet::new(),
            hub_now_ms + 2_000,
        );
        let s21 = res_recycled
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == "gpu:tmux-%21")
            .unwrap();
        assert_eq!(
            s21.transcript_path, None,
            "recycled PID stale Claude session file must be rejected"
        );

        // 3. Valid Claude session attached to %21 (PID 9001), then PID restarts to 9002 in same pane
        //    -> old transcript & metrics MUST be scrubbed!
        let valid_started_ms = (remote_now_s - 5) * 1_000;
        let raw_valid = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p21}\n\
\n{SEC_PS}\n 9001 1 00:05 claude --model opus\n\
\n{SEC_TAILS}\n\
\n{SEC_RESOLVE}\n\
CLAUDE_SESS 9001 13 100 {remote_now_s}\n\
{{\"pid\":9001,\"sessionId\":\"live-cl\",\"cwd\":\"/home/u/repo\",\"startedAt\":{valid_started_ms}}}\n\
\n{SEC_TR_STATS}\n\
78 500 {remote_now_s} /home/u/.claude/projects/-home-u-repo/live-cl.jsonl\n",
        );
        let parsed_valid = parse_ssh_compound_v2_output(&raw_valid, hub_now_ms + 4_000).unwrap();
        let res_valid = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "GPU",
            parsed_valid,
            false,
            &HashSet::new(),
            hub_now_ms + 4_000,
        );
        assert_eq!(
            res_valid.sessions[0].transcript_path.as_deref(),
            Some("/home/u/.claude/projects/-home-u-repo/live-cl.jsonl")
        );

        let p21_restarted = pane_row("%21", 9002, "claude", "/home/u/repo");
        let raw_restarted = format!(
            "{SEC_META}\nREMOTE_NOW_S={}\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p21_restarted}\n\
\n{SEC_PS}\n 9002 1 00:01 claude --model sonnet\n\
\n{SEC_TAILS}\n\n{SEC_RESOLVE}\n\n{SEC_TR_STATS}\n",
            remote_now_s + 10
        );
        let parsed_restarted =
            parse_ssh_compound_v2_output(&raw_restarted, hub_now_ms + 14_000).unwrap();
        let res_restarted = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "GPU",
            parsed_restarted,
            false,
            &HashSet::new(),
            hub_now_ms + 14_000,
        );
        assert_eq!(
            res_restarted.sessions[0].transcript_path, None,
            "PID restart in same pane must scrub stale transcript"
        );
        assert_eq!(res_restarted.sessions[0].model.as_deref(), Some("sonnet"));
    }
}
