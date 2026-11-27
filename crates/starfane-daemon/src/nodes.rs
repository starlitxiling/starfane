//! Local multi-mux pool (`tmux` + `zellij`) and multi-host federation (`NodeProvider`).
//!
//! Supports mixing zero-install SSH ControlMaster hosts (`SshMuxNodeProvider`) and
//! full peer `starfaned` federation (`PeerDaemonNodeProvider` with multi-candidate
//! outbound endpoint failover & Split Ownership) on the same hub.

use crate::config::{NodeConfig, NodeKindConfig, NotifyOwnership, TeamConfig};
use crate::core::discover::{legacy_session_id, next_sequential_window_name};
use crate::core::metrics::{
    agy_step_timings_to_metrics, encode_claude_project_dir, epoch_i64_to_ms,
    extract_agy_effort_from_model, extract_codex_status_title,
    extract_session_id_from_transcript_path, is_post_birth_timestamp, merge_metrics,
    parse_agy_step_timing_tsv, parse_codex_status_line_model_effort, parse_lsof_agy_presence_all,
    parse_ps_etime_secs, parse_status_line_metrics, AgyStepTiming,
};
use crate::core::transcript::{
    parse_iso8601_ms, TranscriptIndexCache, TranscriptMessage, TranscriptQuery, TranscriptResponse,
    TranscriptWatermark,
};
use crate::core::uploads::{decode_base64, detect_image_magic, MAX_IMAGE_BYTES};
use crate::core::util::sanitize_line;
use crate::notice::BoxFuture;
use crate::peer_client::{
    PeerHttpError, PeerKeepAlivePool, PeerSseEndpoint, PeerSseSignal, PeerSseSubscriber,
    SshLoopbackTunnel,
};
use crate::remote_mirror::{
    parse_codex_default_effort_toml, parse_sf_stat_line, RemoteFileStat, RemoteTranscriptMirror,
    MAX_INLINE_DELTA_BYTES,
};
use serde::Serialize;
use serde_json::Value;
use starfane_proto::{
    id, AgentKind, ApiError, Confidence, DaemonEvent, DeliveryLevel, DeliveryReport,
    DeliveryStatus, Effort, ErrorCode, EventDetail, EventId, EventKind, GateCheck, GateReport,
    HostId, HostKind, HostRecord, HostStatus, NormalizedEvent, PaneId, PaneResolution, SessionId,
    SessionMetrics, SessionRecord, SessionState, LOCAL_HOST_ID, PROTO_VERSION,
};
use starfane_tmux::{
    adaptive_paste_settle_ms, cap_page_bytes, classify_panes_with_git_lookup,
    detect_blocking_startup_prompt, detect_codex_update_prompt, detect_folder_trust_prompt,
    is_paste_stuck_in_prompt, parse_panes, parse_zellij_pane_id, qualify_host_pane,
    qualify_host_session, shell_single_quote, trim_and_cap_tail, validate_interactive_key,
    validate_interactive_text, CaptureRequest, ClientInfo, ConduitLane, DiscoverOptions,
    DiscoveredPane, ExecTransport, FocusOutcome, GateConfig, PaneInfo, PanePreview, PaneStatus,
    SshConduitPool, SshExec, TerminalMux, Tmux, TmuxError, ZellijMux, MAX_CAPTURE_BYTES,
    MAX_CAPTURE_LINES, MAX_FULL_CAPTURE_BYTES, MAX_FULL_CAPTURE_LINES,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

const SSH_SPLIT_MARKER: &str = "__SF_SSH_PS_SPLIT__";

const SEC_META: &str = "__SF_SEC_META__";
const SEC_PANES: &str = "__SF_SEC_PANES__";
const SEC_PS: &str = "__SF_SEC_PS__";
const SEC_TAILS: &str = "__SF_SEC_TAILS__";
const SEC_RESOLVE: &str = "__SF_SEC_RESOLVE__";
const SEC_TR_STATS: &str = "__SF_SEC_TR_STATS__";
const SEC_HOT_DELTAS: &str = "__SF_SEC_HOT_DELTAS__";

const PF_META: &str = "__SF_PF_META__";
const PF_WINDOWS: &str = "__SF_PF_WINDOWS__";
const PF_TEAM_TOML: &str = "__SF_PF_TEAM_TOML__";
const PF_AGENT_CFG: &str = "__SF_PF_AGENT_CFG__";

const SSH_BG_MAX_CONCURRENT: usize = 4;
const SSH_INTERACTIVE_MAX_CONCURRENT: usize = 1;
const LEGACY_STABLE_TICKS: u32 = 3;
const LEGACY_SPAWN_GRACE_MS: u64 = 15_000;
const CLAUDE_PID_START_MAX_SKEW_MS: u64 = 120_000;
const RECENT_CLOSE_TOMBSTONE_MS: u64 = 5_000;
const MAX_STAGED_CACHE_ENTRIES: usize = 256;
const RTT_UNSET: u64 = u64::MAX;
const SSH_PING_INTERVAL_MS: u64 = 15_000;
const SSH_PING_TIMEOUT: Duration = Duration::from_secs(3);
const SSH_STAGE_CONDUIT_MAX_BYTES: usize = 64 * 1024;
const SSH_HOT_CURSOR_MAX: usize = 6;
const SSH_WARM_TOP_N: usize = 8;
const SSH_INLINE_OUTPUT_MAX_BYTES: usize = 32 * 1024;
const SSH_INLINE_OUTPUT_MAX_TURNS: usize = 32;
const WATCH_DEBOUNCE_MS: u64 = 75;
const WATCH_DIGEST_ACTIVE_MS: u64 = 1_000;
const WATCH_DIGEST_IDLE_MS: u64 = 3_000;
const WATCH_MAX_LINE_KEEP_BYTES: usize = 4 * 1024;

/// Remote non-interactive `$PATH` bootstrap prepended to SSH POSIX `/bin/sh` scripts so
/// user-installed `claude`, `codex`, `agy`, and `tmux` binaries are found even when login
/// profiles are skipped.
const REMOTE_PATH_BOOTSTRAP: &str = r#"for _d in "$HOME/.local/bin" "$HOME/.cargo/bin" "$HOME/.npm-global/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
  [ -d "$_d" ] && case ":$PATH:" in *":$_d:"*) ;; *) PATH="$PATH:$_d" ;; esac
done
export PATH
"#;

/// Portable POSIX `stat` wrapper honoring `SF_STAT_MODE` (`bsd` / `gnu`) detected once by
/// `PersistentSshConduit` (falling back cleanly to auto-detect on one-shot `SshExec`).
const REMOTE_SF_STAT_FN: &str = r#"if [ -z "${SF_STAT_MODE:-}" ]; then
  if stat -c '%s' / >/dev/null 2>&1; then SF_STAT_MODE=gnu
  elif stat -f '%z' / >/dev/null 2>&1; then SF_STAT_MODE=bsd
  else SF_STAT_MODE=unknown; fi
fi
sf_stat() {
  case "$SF_STAT_MODE" in
    gnu) stat -c '%i %s %Y' "$1" 2>/dev/null || printf '0 -1 0\n' ;;
    bsd) stat -f '%i %z %m' "$1" 2>/dev/null || printf '0 -1 0\n' ;;
    *) stat -c '%i %s %Y' "$1" 2>/dev/null || stat -f '%i %z %m' "$1" 2>/dev/null || printf '0 -1 0\n' ;;
  esac
}
sf_batch_stat() {
  [ "$#" -gt 0 ] || return 0
  case "$SF_STAT_MODE" in
    bsd) stat -f '%i %z %m %N' "$@" 2>/dev/null || true ;;
    gnu) stat -c '%i %s %Y %n' "$@" 2>/dev/null || true ;;
    *)
      for _sp in "$@"; do
        printf '%s %s\n' "$(sf_stat "$_sp")" "$_sp"
      done
      ;;
  esac
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

    pub fn send_key(&self, pane: &PaneId, key: &str) -> Result<(), TmuxError> {
        if Self::is_zellij_pane(pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.send_key(pane, key)
        } else {
            self.tmux.send_key(pane, key)
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

    pub fn suspend_pane(&self, req: &starfane_tmux::SuspendPaneReq) -> Result<PaneId, TmuxError> {
        if Self::is_zellij_pane(&req.pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.suspend_pane(req)
        } else {
            self.tmux.suspend_pane(req)
        }
    }

    pub fn wake_suspended_pane(
        &self,
        req: &starfane_tmux::WakePaneReq,
    ) -> Result<PaneId, TmuxError> {
        if Self::is_zellij_pane(&req.pane) {
            let zj = self.zellij.as_ref().ok_or_else(|| {
                TmuxError::InvalidRequest("zellij mux is not enabled or unavailable".into())
            })?;
            zj.wake_suspended_pane(req)
        } else {
            self.tmux.wake_suspended_pane(req)
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

/// Asynchronous push signal emitted by a remote host's push watcher (`tmux -C` on SSH or
/// `/api/events` SSE subscriber on Peer) to wake `Core`'s per-host supervisor immediately.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum HostPushSignal {
    DirtyPanes(Vec<PaneId>),
    FullSync,
    PeerSnapshot(Box<NodeSnapshot>),
    PeerEvent(DaemonEvent),
    Reconnect,
}

#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub host: HostRecord,
    pub sessions: Vec<SessionRecord>,
    /// Live host-qualified pane IDs currently present on this remote host (when full discovery ran).
    pub live_pane_ids: HashSet<PaneId>,
    /// Heuristic events produced during SSH discovery (`TurnComplete`, `NeedsPermission`, `NeedsInput`).
    pub events: Vec<NormalizedEvent>,
    /// Incremental `DaemonEvent::Output` payloads emitted when remote transcripts or live tails advance.
    pub output_deltas: Vec<DaemonEvent>,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_paused: bool,
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
    retry_paused: bool,
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
            retry_paused: false,
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

    pub fn is_retry_paused(&self) -> bool {
        self.retry_paused
    }

    pub fn pause_retry(&mut self) {
        self.retry_paused = true;
        self.next_retry_ms = None;
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.next_retry_ms = None;
        self.last_error = None;
        self.retry_paused = false;
    }

    pub fn record_failure(&mut self, now_ms: u64, err: impl Into<String>) -> (HostStatus, u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(err.into());
        if self.retry_paused {
            self.next_retry_ms = None;
            return (self.status(), now_ms);
        }
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
        self.retry_paused = false;
        self.next_retry_ms = None;
    }

    /// True if a scheduled background sync should be skipped because the backoff window is active.
    pub fn active_backoff_until(&self, now_ms: u64) -> Option<u64> {
        self.next_retry_ms.filter(|&until| now_ms < until)
    }

    /// Fast-fail silent background reads (`capture_pane`, `fetch_transcript`, `list_tmux_sessions`)
    /// in 0ms when the host circuit is paused or `Offline` inside its cooldown window.
    pub fn fast_fail_if_offline(&self, host_id: &HostId, now_ms: u64) -> Result<(), ApiError> {
        if self.retry_paused {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                format!("host {host_id} auto-retry is paused — click Reconnect to resume"),
            ));
        }
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

    fn sync_ms(&self) -> Option<u64> {
        None
    }

    fn next_retry_ms(&self) -> Option<u64>;

    fn circuit_status(&self) -> HostStatus;

    fn is_retry_paused(&self) -> bool {
        false
    }

    fn set_retry_paused(&self, _paused: bool) {}

    fn sync_dirty_panes<'a>(
        &'a self,
        display_name: &'a str,
        dirty_panes: &'a [PaneId],
        claimed_external_ids: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        let _ = dirty_panes;
        self.sync_with_claimed(display_name, claimed_external_ids)
    }

    fn spawn_push_watcher(
        &self,
        _tx: tokio::sync::mpsc::UnboundedSender<HostPushSignal>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }

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

    fn suspend_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
        cwd: Option<&'a str>,
        waiter_script: &'a str,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        let _ = (remote_session, local_pane, cwd, waiter_script);
        Box::pin(async {
            Err(ApiError::bad_request(
                "remote host does not support suspending agent sessions",
            ))
        })
    }

    fn wake_suspended_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        let _ = (remote_session, local_pane);
        Box::pin(async {
            Err(ApiError::bad_request(
                "remote host does not support waking suspended sessions",
            ))
        })
    }

    fn send_key<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
        key: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        let _ = (remote_session, local_pane, key);
        Box::pin(async {
            Err(ApiError::bad_request(
                "remote host does not support interactive keys",
            ))
        })
    }
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
            NodeKindConfig::Peer => {
                Arc::new(PeerDaemonNodeProvider::with_state_dir(cfg, state_dir))
            }
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
    agent_pid: Option<u32>,
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
    /// Non-`None` when bound via Codex live bottom status-bar title (`extract_codex_status_title`).
    /// Used to detect interactive `/resume` thread switches or `/new` blank prompts on later ticks.
    title_hint: Option<String>,
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
    #[allow(dead_code)]
    created_at: i64,
    #[allow(dead_code)]
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
    /// Monotonic discovery tick counter used to stagger active-pane verification captures.
    discover_tick: u64,
    /// OS process birth timestamps strictly in the **Remote Host Clock Domain** (`remote_now_ms - etime`).
    pid_birth_remote_ms: HashMap<u32, u64>,
    /// Cached AGY presence locks keyed by live remote `pid -> (birth_remote_s, lock_uuids)` so
    /// macOS `lsof -p` runs once per AGY process lifetime (invalidated on PID death or birth skew).
    resolved_agy_locks: HashMap<u32, (u64, Vec<String>)>,
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
    /// Latest complete session snapshot for fast targeted `sync_dirty_panes` patching.
    last_sessions: Vec<SessionRecord>,
    /// Last emitted transcript watermark per qualified session id (for `DaemonEvent::Output` deltas).
    last_emitted_watermarks: HashMap<SessionId, TranscriptWatermark>,
    /// Last emitted live-tail content hash per local pane id (for `DaemonEvent::Output` `live_tail` updates).
    last_emitted_tail_hash: HashMap<PaneId, u64>,
}

fn pane_numeric_id(pane: &PaneId) -> u64 {
    pane.as_str()
        .strip_prefix('%')
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
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
        self.last_emitted_tail_hash.remove(local_pane);
        let leg = legacy_session_id(local_pane);
        self.session_lookup.remove(&leg);
        let pane_suffix = format!(":{}", local_pane.as_str());
        let leg_suffix = format!(":{}", leg.as_str());
        let mut evicted_sids: HashSet<SessionId> = HashSet::new();
        for s in &self.last_sessions {
            if s.pane
                .as_ref()
                .is_some_and(|p| p.as_str().ends_with(&pane_suffix))
                || s.session_id == leg
                || s.session_id.as_str().ends_with(&leg_suffix)
            {
                evicted_sids.insert(s.session_id.clone());
            }
        }
        for s in &self.last_sessions {
            if s.parent_session_id
                .as_ref()
                .is_some_and(|p| evicted_sids.contains(p))
            {
                evicted_sids.insert(s.session_id.clone());
            }
        }
        self.last_emitted_watermarks.retain(|sid, _| {
            !evicted_sids.contains(sid) && sid != &leg && !sid.as_str().ends_with(&leg_suffix)
        });
        self.last_sessions
            .retain(|s| !evicted_sids.contains(&s.session_id));
        self.recently_closed_panes
            .insert(local_pane.clone(), now_hub_ms);
    }

    /// Return pane tail fingerprints that may skip `tmux capture-pane` on this tick:
    /// - `Idle` / `Ended` panes with unchanged `(history_size, cursor_x, cursor_y, in_mode, cmd, agent_pid)`
    ///   never suffer a synchronized `TAIL_CACHE_MAX_TICKS` expiry cliff (**0 captures when idle**).
    /// - Active (`Working`/`Starting`/`Needs*`) panes stagger verification via
    ///   `(pane_num + discover_tick) % 4 == 0` so at most 1/4 verify on any given tick.
    fn cached_tail_fingerprints(&self, tick: u64) -> Vec<String> {
        self.tail_cache
            .iter()
            .filter(|(pane, entry)| {
                // If agent_pid changed since cache entry, never treat as cached.
                if self.pane_agent_pid.get(*pane).copied() != entry.agent_pid {
                    return false;
                }
                let is_active = self.pane_watch.get(*pane).is_some_and(|w| {
                    matches!(
                        w.state,
                        SessionState::Working
                            | SessionState::Starting
                            | SessionState::NeedsPermission
                            | SessionState::NeedsInput
                    )
                });
                !(is_active && (pane_numeric_id(pane).wrapping_add(tick)) % 4 == 0)
            })
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

    /// Already-resolved live AGY `(pid, birth_remote_s)` pairs so `lsof -p` runs once per AGY PID.
    fn resolved_agy_pid_births(&self) -> Vec<(u32, u64)> {
        let mut out: Vec<(u32, u64)> = self
            .resolved_agy_locks
            .iter()
            .filter(|(_, (_, locks))| !locks.is_empty())
            .map(|(&pid, &(birth_s, _))| (pid, birth_s))
            .collect();
        out.sort_unstable();
        out
    }

    /// Panes that should still be tail-captured even if `include_shell_panes == false`
    /// (e.g. freshly spawned panes within startup grace).
    fn force_capture_panes(&self, now_hub_ms: u64) -> Vec<String> {
        self.spawned_panes
            .iter()
            .filter(|(_, m)| now_hub_ms.saturating_sub(m.hub_spawned_at_ms) < LEGACY_SPAWN_GRACE_MS)
            .map(|(p, _)| p.as_str().to_string())
            .collect()
    }

    /// Select up to `SSH_HOT_CURSOR_MAX` (6) active/`Working` sessions' `(remote_path, ino, synced_offset)`
    /// for inline `<= 64 KiB` hot delta delivery inside `compound_discover_script_v3`.
    fn hot_transcript_cursors(
        &self,
        host_id: &HostId,
        mirror: &RemoteTranscriptMirror,
    ) -> Vec<(String, u64, u64)> {
        let mut ranked: Vec<(u8, u64, String, u64, u64)> = Vec::new();
        for (pane, res) in &self.resolved_by_pane {
            let Some(&agent) = self.pane_agent.get(pane) else {
                continue;
            };
            if agent.is_shell() {
                continue;
            }
            let Some((ino, synced_off)) =
                mirror.cached_sync_cursor(host_id.as_str(), agent, &res.path)
            else {
                continue;
            };
            if ino == 0 {
                continue;
            }
            let (priority, last_ms) = match self.pane_watch.get(pane) {
                Some(w) if w.state == SessionState::Working => (3, w.last_event_ms),
                Some(w)
                    if matches!(
                        w.state,
                        SessionState::Starting
                            | SessionState::NeedsPermission
                            | SessionState::NeedsInput
                    ) =>
                {
                    (2, w.last_event_ms)
                }
                Some(w) => (1, w.last_event_ms),
                None => (0, 0),
            };
            ranked.push((priority, last_ms, res.path.clone(), ino, synced_off));
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(SSH_HOT_CURSOR_MAX);
        for (_, _, path, ino, off) in ranked {
            if seen.insert(path.clone()) {
                out.push((path, ino, off));
                if out.len() >= SSH_HOT_CURSOR_MAX {
                    break;
                }
            }
        }
        out
    }

    fn has_active_session(&self) -> bool {
        self.pane_watch.values().any(|w| {
            matches!(
                w.state,
                SessionState::Working
                    | SessionState::Starting
                    | SessionState::NeedsPermission
                    | SessionState::NeedsInput
            )
        })
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

/// Inline `<= 64 KiB` transcript delta piggybacked inside `compound_discover_script_v3` or
/// `sync_dirty_panes` (`__SF_HOT_DELTA__`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineHotDelta {
    pub stat: RemoteFileStat,
    pub base_offset: u64,
    pub bytes: Vec<u8>,
}

/// Parsed structured output of `compound_discover_script_v2` / `compound_discover_script_v3`.
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
    pub hot_deltas: HashMap<String, InlineHotDelta>,
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

    // Optional `__SF_SEC_HOT_DELTAS__` section (emitted by `compound_discover_script_v3`).
    let (real_tr_stats_sec, hot_deltas_sec) =
        match split_marker_line_last(tr_stats_sec, SEC_HOT_DELTAS) {
            Some((stats_part, deltas_part)) => (stats_part, deltas_part),
            None => (tr_stats_sec, ""),
        };

    // Parse `__SF_SEC_TR_STATS__`.
    for line in real_tr_stats_sec.lines() {
        if let Some((stat, path)) = parse_stat_and_path(line) {
            out.tr_stats.insert(path, stat);
        }
    }

    // Parse `__SF_SEC_HOT_DELTAS__` (`__SF_HOT_DELTA__ <ino> <size> <mtime_s> <base_off> <delta_len> <path>\n<b64>`).
    parse_hot_deltas_section(hot_deltas_sec, &mut out.hot_deltas);

    Ok(out)
}

/// Alias entry point for parsing `compound_discover_script_v3` output (superset of v2).
pub fn parse_ssh_compound_v3_output(
    stdout: &str,
    fallback_now_ms: u64,
) -> Result<ParsedSshDiscoveryV2, String> {
    parse_ssh_compound_v2_output(stdout, fallback_now_ms)
}

fn parse_hot_deltas_section(sec: &str, out: &mut HashMap<String, InlineHotDelta>) {
    let lines: Vec<&str> = sec.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("__SF_HOT_DELTA__ ") {
            let b64_line = lines.get(i + 1).copied().unwrap_or("").trim();
            i += 2;
            let trimmed = rest.trim();
            let mut cols = trimmed.split_whitespace();
            let (Some(ino_s), Some(size_s), Some(mt_s), Some(off_s), Some(dlen_s)) = (
                cols.next(),
                cols.next(),
                cols.next(),
                cols.next(),
                cols.next(),
            ) else {
                continue;
            };
            let path = after_n_tokens(trimmed, 5).trim();
            if path.is_empty() {
                continue;
            }
            let (Ok(ino), Ok(size), Ok(mtime_s), Ok(base_offset), Ok(delta_len)) = (
                ino_s.parse::<u64>(),
                size_s.parse::<u64>(),
                mt_s.parse::<i64>(),
                off_s.parse::<u64>(),
                dlen_s.parse::<usize>(),
            ) else {
                continue;
            };
            if delta_len == 0 || delta_len > MAX_INLINE_DELTA_BYTES {
                continue;
            }
            if let Ok(bytes) = decode_base64(b64_line) {
                if bytes.len() == delta_len {
                    out.insert(
                        path.to_string(),
                        InlineHotDelta {
                            stat: RemoteFileStat { ino, size, mtime_s },
                            base_offset,
                            bytes,
                        },
                    );
                }
            }
            continue;
        }
        i += 1;
    }
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
    live_pane_ids: HashSet<PaneId>,
    events: Vec<NormalizedEvent>,
    need_mirror: Vec<(PaneId, AgentKind, String, Option<RemoteFileStat>)>,
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

    // Prune dead or recycled AGY PID lock cache entries and merge newly discovered locks.
    state.resolved_agy_locks.retain(|pid, (birth_s, _)| {
        if !parsed.live_remote_pids.contains(pid) {
            return false;
        }
        state.pid_birth_remote_ms.get(pid).is_none_or(|&b_ms| {
            let cur_s = b_ms / 1_000;
            cur_s.abs_diff(*birth_s) <= 5
        })
    });
    for (pid, cids) in &parsed.agy_locks_by_pid {
        let birth_s = state
            .pid_birth_remote_ms
            .get(pid)
            .copied()
            .unwrap_or(remote_now_ms)
            / 1_000;
        let entry = state
            .resolved_agy_locks
            .entry(*pid)
            .or_insert_with(|| (birth_s, Vec::new()));
        entry.0 = birth_s;
        for c in cids {
            if !entry.1.iter().any(|x| x == c) {
                entry.1.push(c.clone());
            }
        }
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
            let qsid = qualify_host_session(host_id, &legacy_session_id(&dp.pane_id));
            state.last_emitted_watermarks.remove(&qsid);
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

        // Update fingerprint tail cache from newly captured tails.
        if let Some(fresh_tail) = parsed.captured_tails.get(&dp.pane_id) {
            state.tail_cache.insert(
                dp.pane_id.clone(),
                SshTailCacheEntry {
                    history_size: dp.history_size,
                    cursor_x: dp.cursor_x,
                    cursor_y: dp.cursor_y,
                    in_mode: dp.pane_in_mode,
                    current_command: dp.pane_current_command.clone(),
                    agent_pid: eff_pid,
                    tail: fresh_tail.clone(),
                },
            );
        }
    }

    // Dynamic invalidation for Codex status-bar title bindings: if a non-CLI-resume Codex pane's
    // live bottom status bar changed to a different thread title (`/resume`) or became blank
    // (`/new`), drop its old title binding cleanly before building `sibling_claimed`.
    for dp in &discovered {
        if dp.pane_dead || dp.agent != AgentKind::Codex || dp.codex_resume_id.is_some() {
            continue;
        }
        let prev_title = state
            .resolved_by_pane
            .get(&dp.pane_id)
            .and_then(|r| r.title_hint.as_deref());
        let Some(prev_title) = prev_title else {
            continue;
        };
        if let Some(tail_entry) = state.tail_cache.get(&dp.pane_id) {
            let cur_title = extract_codex_status_title(&tail_entry.tail);
            if cur_title.as_deref() != Some(prev_title) {
                state.resolved_by_pane.remove(&dp.pane_id);
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
    let live_excluded_for_pane =
        |pane: &PaneId, newly_claimed: &HashSet<String>| -> HashSet<String> {
            let mut ex = HashSet::new();
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
                    upsert_resolved_transcript(state, &dp.pane_id, path, Some(sid), true, None);
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
                            None,
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
                // Prefer known `nesting_depth = 0` root conversations over child locks
                // (consulting both fresh locks and PID-birth-cached locks).
                for pid in &pids {
                    let locks_opt = parsed
                        .agy_locks_by_pid
                        .get(pid)
                        .map(Vec::as_slice)
                        .or_else(|| state.resolved_agy_locks.get(pid).map(|(_, v)| v.as_slice()));
                    if let Some(cids) = locks_opt {
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
                        let locks_opt = parsed
                            .agy_locks_by_pid
                            .get(pid)
                            .map(Vec::as_slice)
                            .or_else(|| {
                                state.resolved_agy_locks.get(pid).map(|(_, v)| v.as_slice())
                            });
                        if let Some(cids) = locks_opt {
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
                        upsert_resolved_transcript(
                            state,
                            &dp.pane_id,
                            path,
                            Some(conv_id),
                            true,
                            None,
                        );
                    }
                } else {
                    unmatched_agy.push(dp);
                }
            }
            AgentKind::Shell => {}
        }
    }

    // Pass 1b: Explicit Codex status-bar title matching (`extract_codex_status_title(tail) == Some(hint)`).
    // Does NOT require `unique_in_cwd` or `created_at/updated_at >= birth_ms`, so multiple Codex panes
    // sharing a CWD (including blank siblings) AND interactive TUI `/resume` threads resolve cleanly.
    // Strictly enforces: matching `cwd`, unique live `(cwd, title)` across panes, `!live_claimed`, and
    // unambiguous single hit in `state_5.sqlite`. Treated as authoritative while live tail shows that title.
    let mut codex_title_counts: HashMap<(String, String), usize> = HashMap::new();
    for dp in &unmatched_codex {
        let tail = state
            .tail_cache
            .get(&dp.pane_id)
            .map(|e| e.tail.as_str())
            .unwrap_or("");
        if let Some(title) = extract_codex_status_title(tail) {
            *codex_title_counts
                .entry((dp.cwd.trim_end_matches('/').to_string(), title))
                .or_insert(0) += 1;
        }
    }
    let mut titleless_codex: Vec<&DiscoveredPane> = Vec::new();
    for dp in &unmatched_codex {
        let tail = state
            .tail_cache
            .get(&dp.pane_id)
            .map(|e| e.tail.as_str())
            .unwrap_or("");
        if let Some(title) = extract_codex_status_title(tail) {
            let unique_title = codex_title_counts
                .get(&(dp.cwd.trim_end_matches('/').to_string(), title.clone()))
                .copied()
                .unwrap_or(0)
                == 1;
            if unique_title {
                let live_exclude = live_excluded_for_pane(&dp.pane_id, &newly_claimed_ids);
                if let Some((cid, path)) =
                    match_remote_codex_sqlite(&state.codex_threads, &dp.cwd, tail, &live_exclude)
                {
                    newly_claimed_ids.insert(cid.clone());
                    upsert_resolved_transcript(
                        state,
                        &dp.pane_id,
                        path,
                        Some(cid),
                        true,
                        Some(title),
                    );
                    continue;
                }
            }
        }
        titleless_codex.push(dp);
    }

    // Pass 2: Strict non-authoritative title-less CWD fallback for unlocked Codex & AGY panes
    // (`unique_in_cwd && !claimed && post_birth && unambiguous`).
    let mut codex_cwd_counts: HashMap<String, usize> = HashMap::new();
    for dp in &titleless_codex {
        *codex_cwd_counts
            .entry(dp.cwd.trim_end_matches('/').to_string())
            .or_insert(0) += 1;
    }
    for dp in &titleless_codex {
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
        if let Some((cid, path)) =
            match_remote_codex_rollout_fallback(&parsed.codex_rollouts, &dp.cwd, &exclude, birth_ms)
        {
            newly_claimed_ids.insert(cid.clone());
            upsert_resolved_transcript(state, &dp.pane_id, path, Some(cid), false, None);
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
            upsert_resolved_transcript(state, &dp.pane_id, path, Some(cid), false, None);
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
    let mut need_mirror: Vec<(PaneId, AgentKind, String, Option<RemoteFileStat>)> = Vec::new();
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

        if let Some(r) = resolved.as_ref() {
            let needs_sync = match tr_stat {
                Some(st) => r.last_stat.is_none_or(|prev| {
                    prev.ino != st.ino || prev.size != st.size || prev.mtime_s != st.mtime_s
                }),
                // Newly resolved transcript not yet in `latest_tr_stats` (e.g. a resumed thread
                // ranked below the top-30 SQLite rows): mirror immediately on tick 1!
                None => r.last_stat.is_none(),
            };
            if needs_sync {
                need_mirror.push((dp.pane_id.clone(), dp.agent, r.path.clone(), tr_stat));
            }
        }

        let tail_opt = state
            .tail_cache
            .get(&dp.pane_id)
            .map(|e| e.tail.as_str())
            .map(str::to_string);
        let (st_model, st_effort) = if dp.agent == AgentKind::Codex {
            tail_opt
                .as_deref()
                .map(parse_codex_status_line_model_effort)
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

        // Model & effort precedence: verified transcript > CLI args > spawn metadata > Codex status bar > AGY model suffix.
        let model = resolved
            .as_ref()
            .and_then(|r| r.model.clone())
            .or_else(|| dp.model.clone())
            .or_else(|| spawned_meta.as_ref().and_then(|m| m.model.clone()))
            .or(st_model);
        let effort = resolved
            .as_ref()
            .and_then(|r| r.effort.clone())
            .or_else(|| dp.effort.clone())
            .or_else(|| spawned_meta.as_ref().and_then(|m| m.effort.clone()))
            .or(st_effort)
            .or_else(|| {
                (dp.agent == AgentKind::Agy)
                    .then(|| model.as_deref().and_then(extract_agy_effort_from_model))
                    .flatten()
            });

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
                        resume_id: None,
                        suspended_at_ms: None,
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
            resume_id: None,
            suspended_at_ms: None,
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
            resume_id: None,
            suspended_at_ms: None,
        });
    }

    let qualified_live_panes: HashSet<PaneId> = live_pane_ids
        .iter()
        .map(|p| qualify_host_pane(host_id, p))
        .collect();

    SshReconcileResult {
        sessions,
        live_pane_ids: qualified_live_panes,
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
    title_hint: Option<String>,
) {
    match state.resolved_by_pane.get_mut(pane) {
        Some(existing) if existing.path == path => {
            existing.authoritative |= authoritative;
            if existing.external_id.is_none() {
                existing.external_id = external_id;
            }
            if title_hint.is_some() {
                existing.title_hint = title_hint;
            }
        }
        _ => {
            state.resolved_by_pane.insert(
                pane.clone(),
                ResolvedRemoteTranscript {
                    path,
                    external_id,
                    authoritative,
                    title_hint,
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
    cwd: &str,
    tail: &str,
    claimed_ids: &HashSet<String>,
) -> Option<(String, String)> {
    let hint = extract_codex_status_title(tail)?;
    let hint_clean = hint.trim_end_matches(['…', '.']).trim();
    if hint_clean.is_empty() {
        return None;
    }
    let check_row = |r: &RemoteCodexThreadRow| -> Option<(String, String)> {
        let id = r.id.trim();
        let path = r.rollout_path.trim();
        if id.is_empty()
            || path.is_empty()
            || claimed_ids.contains(id)
            || !same_remote_dir(&r.cwd, cwd)
        {
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
    push_watch: bool,
    #[allow(dead_code)]
    tmux: Tmux,
    transport: Arc<SshExec>,
    conduit_pool: Arc<SshConduitPool>,
    tmux_bin: String,
    tmux_socket: Option<String>,
    claude_bin: String,
    codex_bin: String,
    agy_bin: String,
    mirror: Arc<RemoteTranscriptMirror>,
    transcript_index: Arc<Mutex<TranscriptIndexCache>>,
    /// Bounds background SSH channels and >64 KiB one-shot `stage_input_files` uploads.
    io_sem: Arc<Semaphore>,
    /// Dedicated interactive SSH lane guard (`dispatch_interactive`, `spawn_session`, `focus_pane`, `close_pane`, `probe_now`).
    interactive_sem: Arc<Semaphore>,
    circuit: Arc<Mutex<HostCircuitBreaker>>,
    discovery: Arc<Mutex<SshDiscoveryState>>,
    staged_cache: Mutex<StagedFileCache>,
    last_sync_ms: AtomicU64,
    last_ping_ms: AtomicU64,
    warm_in_flight: Arc<AtomicBool>,
}

fn is_retryable_conduit_err(err: &TmuxError) -> bool {
    match err {
        TmuxError::Parse(msg) => msg.contains("unexpected EOF"),
        TmuxError::Io { source, .. } => matches!(
            source.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
        ),
        TmuxError::Command {
            code: Some(255),
            stderr,
            ..
        } => {
            let lower = stderr.to_ascii_lowercase();
            lower.contains("broken pipe")
                || lower.contains("controlsocket")
                || lower.contains("control socket")
                || lower.contains("mux_client")
                || lower.contains("unexpected eof")
        }
        _ => false,
    }
}

/// Adaptive fast-ack settle & post-check timing for `SshMuxNodeProvider::dispatch_interactive`.
///
/// Short single-line prompts (`<= 160` chars, no `\n`) use a fast `30 ms` paste settle and
/// `50 ms` post-check (cutting LAN dispatch from ~600 ms to ~90 ms), while longer or multi-line
/// prompts retain full adaptive paste-burst settling (`120 ms` post-check + stuck-paste retry).
pub(crate) fn fast_ssh_dispatch_timings(configured_settle_ms: u64, text: &str) -> (u64, u64) {
    let is_short_single_line = !text.contains('\n') && text.chars().count() <= 160;
    if configured_settle_ms == 0 {
        return (0, 50);
    }
    if is_short_single_line {
        (configured_settle_ms.min(30), 50)
    } else {
        (adaptive_paste_settle_ms(configured_settle_ms, text), 120)
    }
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
        let ssh_exec = SshExec::new(cfg.ssh_target.trim(), control_path)
            .with_ssh_bin(cfg.ssh_bin_override())
            .with_connect_timeout_secs(cfg.effective_connect_timeout_secs())
            .with_compression(cfg.compression)
            .with_extra_args(cfg.ssh_args.clone());
        let conduit_pool = Arc::new(SshConduitPool::new(ssh_exec.clone()));
        let transport = Arc::new(ssh_exec);
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
            circuit: Arc::new(Mutex::new(HostCircuitBreaker::new(host_id.as_str()))),
            host_id,
            last_display_name: Mutex::new(display.clone()),
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            include_shell_panes: cfg.include_shell_panes,
            push_watch: cfg.push_watch,
            tmux,
            transport,
            conduit_pool,
            tmux_bin,
            tmux_socket,
            claude_bin: cfg.agent_bin(AgentKind::Claude),
            codex_bin: cfg.agent_bin(AgentKind::Codex),
            agy_bin: cfg.agent_bin(AgentKind::Agy),
            mirror,
            transcript_index,
            io_sem: Arc::new(Semaphore::new(SSH_BG_MAX_CONCURRENT)),
            interactive_sem: Arc::new(Semaphore::new(SSH_INTERACTIVE_MAX_CONCURRENT)),
            discovery: Arc::new(Mutex::new(SshDiscoveryState::default())),
            staged_cache: Mutex::new(StagedFileCache::default()),
            last_sync_ms: AtomicU64::new(RTT_UNSET),
            last_ping_ms: AtomicU64::new(0),
            warm_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn conduit_pool(&self) -> &Arc<SshConduitPool> {
        &self.conduit_pool
    }

    async fn exec_conduit_idempotent(
        &self,
        lane: ConduitLane,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<starfane_tmux::ExecBytesOutput, TmuxError> {
        match self
            .conduit_pool
            .exec_bytes(lane, script, stdin, timeout)
            .await
        {
            Ok(out) => Ok(out),
            Err(err) if is_retryable_conduit_err(&err) => {
                self.conduit_pool
                    .exec_bytes(lane, script, stdin, timeout)
                    .await
            }
            Err(err) => Err(err),
        }
    }

    async fn maybe_refresh_rtt(&self, force: bool) {
        let now_ms = id::now_ms();
        if !force && self.transport.rtt_ms().is_some() {
            let last = self.last_ping_ms.load(Ordering::Acquire);
            if now_ms.saturating_sub(last) < SSH_PING_INTERVAL_MS {
                return;
            }
        }
        match self.conduit_pool.ping(SSH_PING_TIMEOUT).await {
            Ok(_) => {
                self.last_ping_ms.store(id::now_ms(), Ordering::Release);
            }
            Err(e) => {
                log::debug!("ssh {} conduit ping skipped/failed: {e}", self.host_id);
            }
        }
    }

    fn extra_agent_bin_names(&self) -> String {
        let mut names = Vec::new();
        for raw in [&self.claude_bin, &self.codex_bin, &self.agy_bin] {
            let base = Path::new(raw)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(raw)
                .trim()
                .to_ascii_lowercase();
            if !base.is_empty()
                && !matches!(base.as_str(), "claude" | "codex" | "agy" | "sh")
                && base
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
                && !names.contains(&base)
            {
                names.push(base);
            }
        }
        names.join(" ")
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

    /// Build the ultra-lean single-RTT POSIX `/bin/sh` v3 compound discovery script
    /// (`360ms / 322KB -> ~35–55ms / <6KB`):
    /// 1. Single-pass wrap-safe fixed-point `awk` process-tree filter seeded with tmux pane PIDs
    ///    (handling OS PID wrap-around where `child_pid < parent_pid`), emitting ONLY pane
    ///    descendants + agent processes plus `AGENT_PANES`, `AGY_PIDS`, and `CX_RESUMES`.
    /// 2. Skips `tmux capture-pane` for non-agent shell panes when `include_shell_panes == false`,
    ///    and skips unchanged cached panes with zero synchronized `TAIL_CACHE_MAX_TICKS` cliff.
    /// 3. Single batched OS-native `stat` (`SF_STAT_MODE`: `stat -f` on BSD/macOS, `stat -c` on GNU).
    /// 4. Skips macOS `lsof -p` for already-resolved `(pid, birth_s)` AGY processes.
    /// 5. Appends inline `<= 64 KiB` hot transcript deltas (`__SF_HOT_DELTA__`) for up to 3 active
    ///    sessions in `__SF_SEC_HOT_DELTAS__` (0 extra SSH round-trips during live streaming).
    #[allow(clippy::too_many_arguments)]
    fn compound_discover_script_v3(
        &self,
        cached_tail_fps: &[String],
        prev_agy_db_sig: Option<&str>,
        prev_codex_db_sig: Option<&str>,
        known_tr_paths: &[String],
        resolved_agy_pids: &[(u32, u64)],
        force_capture_panes: &[String],
        hot_cursors: &[(String, u64, u64)],
    ) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let pane_fmt = shell_single_quote(starfane_tmux::PANE_FORMAT);
        let prev_agy = shell_single_quote(prev_agy_db_sig.unwrap_or(""));
        let prev_cx = shell_single_quote(prev_codex_db_sig.unwrap_or(""));
        let include_shells_flag = if self.include_shell_panes { "1" } else { "0" };
        let extra_bins_q = shell_single_quote(&self.extra_agent_bin_names());

        let skip_agy_str = resolved_agy_pids
            .iter()
            .map(|(pid, birth_s)| format!("{pid}:{birth_s}"))
            .collect::<Vec<_>>()
            .join(" ");
        let skip_agy_q = shell_single_quote(&skip_agy_str);

        let force_panes_str = force_capture_panes
            .iter()
            .filter(|p| p.starts_with('%') && p[1..].bytes().all(|b| b.is_ascii_digit()))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let force_panes_q = shell_single_quote(&force_panes_str);

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

        let mut hot_delta_lines = String::new();
        for (path, ino, off) in hot_cursors.iter().take(SSH_HOT_CURSOR_MAX) {
            if !path.contains('\n') && !path.contains('\r') {
                hot_delta_lines.push_str(&format!(
                    "sf_hot_delta {} {} {}\n",
                    shell_single_quote(path),
                    ino,
                    off
                ));
            }
        }

        format!(
            r#"{bootstrap}{sf_stat}
SF_TAB=$(printf '\t')
SF_US=$(printf '\037')
SF_INCLUDE_SHELLS={include_shells}
SF_FORCE_PANES={force_panes}
SF_SKIP_AGY={skip_agy}
SF_EXTRA_BINS={extra_bins}
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
sf_hot_delta() {{
  _hp="$1"; _hino="$2"; _hoff="$3"
  [ -f "$_hp" ] && [ ! -L "$_hp" ] || return 0
  set -- $(sf_stat "$_hp")
  _cino="$1"; _csz="$2"; _cmt="$3"
  [ -n "$_csz" ] && [ "$_csz" -ge 0 ] 2>/dev/null || return 0
  if [ "$_hino" -ne 0 ] && [ "$_cino" -ne "$_hino" ]; then return 0; fi
  _dlen=$((_csz - _hoff))
  if [ "$_dlen" -gt 0 ] && [ "$_dlen" -le {max_inline} ]; then
    _b64=$(tail -c +"$((_hoff + 1))" "$_hp" 2>/dev/null | head -c "$_dlen" | base64 2>/dev/null | tr -d '\r\n')
    if [ -n "$_b64" ]; then
      printf '__SF_HOT_DELTA__ %s %s %s %s %s %s\n%s\n' "$_cino" "$_csz" "$_cmt" "$_hoff" "$_dlen" "$_hp" "$_b64"
    fi
  fi
}}

REMOTE_NOW_S=$(date +%s)
printf '{sec_meta}\n'
printf 'REMOTE_NOW_S=%s\n' "$REMOTE_NOW_S"
printf 'REMOTE_HOME=%s\n' "$HOME"

printf '\n{sec_panes}\n'
PANES_OUT=$({tmux} list-panes -a -F {pane_fmt} 2>/dev/null || true)
[ -n "$PANES_OUT" ] && printf '%s\n' "$PANES_OUT"

_agent_panes=""
_agy_pids=""
_cx_resumes=""
AWK_OUT=$(
  {{
    [ -n "$PANES_OUT" ] && printf '%s\n' "$PANES_OUT"
    printf '__SF_AWK_PS__\n'
    LC_ALL=C ps -eo pid=,ppid=,etime=,args= 2>/dev/null || true
  }} | LC_ALL=C awk -v NOW_S="$REMOTE_NOW_S" -v SKIP_AGY="$SF_SKIP_AGY" -v EXTRA_BINS="$SF_EXTRA_BINS" '
  function etime_secs(s,    d, parts, n, rest) {{
    d = 0
    rest = s
    if (index(rest, "-") > 0) {{
      n = split(rest, parts, "-")
      d = parts[1] + 0
      rest = parts[2]
    }}
    n = split(rest, parts, ":")
    if (n == 2) return d * 86400 + (parts[1] + 0) * 60 + (parts[2] + 0)
    if (n == 3) return d * 86400 + (parts[1] + 0) * 3600 + (parts[2] + 0) * 60 + (parts[3] + 0)
    return -1
  }}
  BEGIN {{
    mode = 0
    ns = split(SKIP_AGY, sa, /[ \t]+/)
    for (i = 1; i <= ns; i++) {{
      if (split(sa[i], kv, ":") == 2) {{
        sp = kv[1] + 0
        if (sp > 0) {{
          has_skip_agy[sp] = 1
          skip_agy_birth[sp] = kv[2] + 0
        }}
      }}
    }}
    ne = split(EXTRA_BINS, eb, /[ \t]+/)
    for (i = 1; i <= ne; i++) {{
      if (eb[i] != "") extra_bin[tolower(eb[i])] = 1
    }}
  }}
  $0 == "__SF_AWK_PS__" {{ mode = 1; next }}
  mode == 0 {{
    if ($0 == "") next
    n = split($0, f, "\t")
    if (n < 7) n = split($0, f, "\037")
    if (n < 7) {{
      s = $0
      gsub(/\\037/, "\t", s)
      n = split(s, f, "\t")
    }}
    if (n >= 7 && substr(f[1], 1, 1) == "%") {{
      pid = f[6] + 0
      if (pid > 0) {{
        pane_root[pid] = f[1]
        pane_cmd[f[1]] = f[7]
        in_tree[pid] = 1
        owner[pid] = f[1]
      }}
    }}
    next
  }}
  {{
    line = $0
    sub(/^[ \t]+/, "", line)
    if (line == "") next
    p = $1 + 0
    pp = $2 + 0
    if (p <= 0) next
    pids[++cnt] = p
    ppid[p] = pp
    etime_raw[p] = $3
    cmd[p] = $4
    raw[p] = line
  }}
  END {{
    changed = 1
    while (changed) {{
      changed = 0
      for (i = 1; i <= cnt; i++) {{
        p = pids[i]
        if (!in_tree[p] && in_tree[ppid[p]]) {{
          in_tree[p] = 1
          owner[p] = owner[ppid[p]]
          changed = 1
        }}
      }}
    }}
    for (i = 1; i <= cnt; i++) {{
      p = pids[i]
      line = raw[p]
      b = cmd[p]
      sub(/^.*\//, "", b)
      bl = tolower(b)
      ll = tolower(line)
      is_agent = 0
      is_agy = 0
      if (bl != "starfane" && bl != "baton" && index(ll, "starfane mcp") == 0 && index(ll, "baton mcp") == 0) {{
        if (bl == "claude" || bl == "codex" || bl == "agy" || (bl in extra_bin) || bl ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) {{
          is_agent = 1
          if (bl == "agy") is_agy = 1
        }} else if (bl == "node" || bl == "sh" || bl == "bash" || bl == "zsh" || bl == "dash" || bl == "python" || bl == "python3" || bl == "bun" || bl == "deno") {{
          if (ll ~ /(^|[ \t\/])claude([ \t]|$)/ || ll ~ /(^|[ \t\/])codex([ \t]|$)/) {{
            is_agent = 1
          }}
          if (ll ~ /(^|[ \t\/])agy([ \t]|$)/) {{
            is_agent = 1
            is_agy = 1
          }}
        }}
      }}
      if (in_tree[p] || is_agent) {{
        print "PS\t" line
      }}
      if (in_tree[p] && is_agent && owner[p] != "") {{
        agent_panes[owner[p]] = 1
      }}
      if (is_agy) {{
        skip = 0
        if (has_skip_agy[p] && NOW_S > 0) {{
          es = etime_secs(etime_raw[p])
          if (es >= 0) {{
            est_b = NOW_S - es
            diff = est_b - skip_agy_birth[p]
            if (diff < 0) diff = -diff
            if (diff <= 5) skip = 1
          }}
        }}
        if (!skip) agy_pids[p] = 1
      }}
      if (index(line, "codex") > 0 && index(line, "resume ") > 0) {{
        nt = split(line, toks, /[ \t]+/)
        for (k = 1; k < nt; k++) {{
          if (toks[k] == "resume") {{
            u = toks[k + 1]
            gsub(/["\047]/, "", u)
            if (length(u) >= 16 && u ~ /^[0-9A-Fa-f-]+$/) {{
              cx_res[u] = 1
            }}
          }}
        }}
      }}
    }}
    for (rpid in pane_root) {{
      pid_str = pane_root[rpid]
      pc = tolower(pane_cmd[pid_str])
      sub(/^.*\//, "", pc)
      if (pc == "claude" || pc == "codex" || pc == "agy" || (pc in extra_bin) || pc ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) {{
        agent_panes[pid_str] = 1
      }}
    }}
    ap = ""
    for (pid_str in agent_panes) ap = (ap == "" ? pid_str : ap " " pid_str)
    print "AGENT_PANES\t" ap
    ag = ""
    for (p in agy_pids) ag = (ag == "" ? p : ag " " p)
    print "AGY_PIDS\t" ag
    cx = ""
    for (u in cx_res) cx = (cx == "" ? u : cx " " u)
    print "CX_RESUMES\t" cx
  }}
  '
)

printf '\n{sec_ps}\n'
if [ -n "$AWK_OUT" ]; then
  while IFS="$SF_TAB" read -r _tag _rest; do
    case "$_tag" in
      PS) printf '%s\n' "$_rest" ;;
      AGENT_PANES) _agent_panes="$_rest" ;;
      AGY_PIDS) _agy_pids="$_rest" ;;
      CX_RESUMES) _cx_resumes="$_rest" ;;
    esac
  done <<__SF_AWK_EOF__
$AWK_OUT
__SF_AWK_EOF__
fi

printf '\n{sec_tails}\n'
if [ -n "$PANES_OUT" ]; then
  _want_panes=" $_agent_panes $SF_FORCE_PANES "
  PANES_US=$(printf '%s\n' "$PANES_OUT" | LC_ALL=C tr '\t' '\037')
  while IFS="$SF_US" read -r _p_id _s_n _w_i _w_x _p_x _p_p _p_cmd _p_cwd _p_a _w_a _s_a _p_mode _p_dead _p_syn _p_alt _p_cx _p_cy _p_wn _p_sg _p_hs _p_ph; do
    case "$_p_id" in %*) ;; *) continue ;; esac
    [ "$_p_dead" = "1" ] && continue
    [ "$_p_mode" = "1" ] && continue
    if [ "$SF_INCLUDE_SHELLS" = "0" ]; then
      case "$_want_panes" in
        *" $_p_id "*) ;;
        *) continue ;;
      esac
    fi
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

# 2. AGY: Linux /proc/$pid/fd fast-path (macOS lsof fallback, skipped for already-resolved PIDs) + optional sqlite3
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
    _cx_sql="SELECT id, COALESCE(rollout_path,''), COALESCE(cwd,''), REPLACE(REPLACE(COALESCE(name,''),char(10),' '),char(13),' '), COALESCE(created_at,0), COALESCE(updated_at,0) FROM threads WHERE rollout_path IS NOT NULL AND rollout_path != '' ORDER BY COALESCE(archived,0) ASC, updated_at DESC LIMIT 500;"
    _cx_rows=$(sqlite3 -readonly -separator "$SF_US" "$_cx_db" "$_cx_sql" 2>/dev/null || sqlite3 -separator "$SF_US" "file:${{_cx_db}}?immutable=1" "$_cx_sql" 2>/dev/null || true)
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
  _valid_tr=""
  while IFS= read -r _tp; do
    [ -n "$_tp" ] && [ -f "$_tp" ] && [ ! -L "$_tp" ] || continue
    if [ -z "$_valid_tr" ]; then
      _valid_tr="$_tp"
    else
      _valid_tr="$_valid_tr
$_tp"
    fi
  done <<__SF_TR_EOF__
$SF_TR_PATHS
__SF_TR_EOF__
  if [ -n "$_valid_tr" ]; then
    _old_ifs="$IFS"
    IFS='
'
    set -f
    set -- $_valid_tr
    set +f
    IFS="$_old_ifs"
    sf_batch_stat "$@"
  fi
fi

printf '\n{sec_hot_deltas}\n'
{hot_delta_lines}"#,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            sf_stat = REMOTE_SF_STAT_FN,
            include_shells = include_shells_flag,
            force_panes = force_panes_q,
            skip_agy = skip_agy_q,
            extra_bins = extra_bins_q,
            fp_cases = fp_cases,
            max_inline = MAX_INLINE_DELTA_BYTES,
            sec_meta = SEC_META,
            sec_panes = SEC_PANES,
            sec_ps = SEC_PS,
            sec_tails = SEC_TAILS,
            sec_resolve = SEC_RESOLVE,
            sec_tr_stats = SEC_TR_STATS,
            sec_hot_deltas = SEC_HOT_DELTAS,
            tmux = tmux_cmd,
            pane_fmt = pane_fmt,
            prev_agy = prev_agy,
            prev_cx = prev_cx,
            known_tr_lines = known_tr_lines,
            hot_delta_lines = hot_delta_lines,
        )
    }

    /// Build the targeted ~18ms single-RTT script for `sync_dirty_panes`:
    /// captures ONLY the specified dirty panes' metadata + 20-line tails + optional inline `<= 64 KiB`
    /// hot transcript deltas (skipping full `ps`/`sqlite3`/`lsof`).
    fn build_dirty_panes_script(&self, targets: &[DirtyPaneTarget]) -> String {
        let tmux_cmd = self.tmux_cmd_prefix();
        let mut pane_cmds = String::new();
        let mut delta_cmds = String::new();
        for target in targets {
            let pq = shell_single_quote(target.pane.as_str());
            pane_cmds.push_str(&format!(
                r##"P={pq}
M=$({tmux} display-message -p -t "$P" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{history_size}}${{SF_TAB}}#{{cursor_x}}${{SF_TAB}}#{{cursor_y}}${{SF_TAB}}#{{pane_current_command}}" 2>/dev/null)
if [ $? -ne 0 ] || [ -z "$M" ]; then
  printf '__SF_DIRTY_MISSING__ %s\n' "$P"
else
  printf '__SF_DIRTY_META__ %s\t%s\n' "$P" "$M"
  printf '__SF_TAIL_BEGIN__ %s\n' "$P"
  {tmux} capture-pane -p -t "$P" -S -20 2>/dev/null || true
  printf '\n__SF_TAIL_END__ %s\n' "$P"
fi
"##,
                pq = pq,
                tmux = tmux_cmd,
            ));
            if let Some(ref hot) = target.hot {
                if !hot.path.contains('\n') && !hot.path.contains('\r') {
                    delta_cmds.push_str(&format!(
                        "sf_hot_delta {} {} {}\n",
                        shell_single_quote(&hot.path),
                        hot.ino,
                        hot.offset
                    ));
                }
            }
        }
        format!(
            r#"{bootstrap}{sf_stat}
SF_TAB=$(printf '\t')
sf_hot_delta() {{
  _hp="$1"; _hino="$2"; _hoff="$3"
  [ -f "$_hp" ] && [ ! -L "$_hp" ] || return 0
  set -- $(sf_stat "$_hp")
  _cino="$1"; _csz="$2"; _cmt="$3"
  [ -n "$_csz" ] && [ "$_csz" -ge 0 ] 2>/dev/null || return 0
  printf '__SF_DIRTY_STAT__ %s %s %s %s\n' "$_cino" "$_csz" "$_cmt" "$_hp"
  if [ "$_hino" -ne 0 ] && [ "$_cino" -ne "$_hino" ]; then return 0; fi
  _dlen=$((_csz - _hoff))
  if [ "$_dlen" -gt 0 ] && [ "$_dlen" -le {max_inline} ]; then
    _b64=$(tail -c +"$((_hoff + 1))" "$_hp" 2>/dev/null | head -c "$_dlen" | base64 2>/dev/null | tr -d '\r\n')
    if [ -n "$_b64" ]; then
      printf '__SF_HOT_DELTA__ %s %s %s %s %s %s\n%s\n' "$_cino" "$_csz" "$_cmt" "$_hoff" "$_dlen" "$_hp" "$_b64"
    fi
  fi
}}
{pane_cmds}
printf '\n{sec_hot_deltas}\n'
{delta_cmds}"#,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            sf_stat = REMOTE_SF_STAT_FN,
            max_inline = MAX_INLINE_DELTA_BYTES,
            pane_cmds = pane_cmds,
            sec_hot_deltas = SEC_HOT_DELTAS,
            delta_cmds = delta_cmds,
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
        post_check_ms: u64,
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
        let post_sleep_cmd = if post_check_ms == 0 {
            "true".to_string()
        } else {
            format!(
                "sleep {:.3} </dev/null 2>/dev/null || true",
                post_check_ms as f64 / 1000.0
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
{post_sleep_cmd}
POST_META=$({tmux} display-message -p -t "$PANE" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{cursor_y}}" </dev/null 2>/dev/null || printf 'MISSING')
POST=$({tmux} capture-pane -p -t "$PANE" </dev/null 2>/dev/null || true)
printf 'STATUS:OK:%s:%s\nPOST_META:%s\n__SF_PRE__\n%s\n__SF_PRE_END__\n__SF_POST__\n%s\n__SF_POST_END__\n' "$EXITED_COPY" "$P_CMD" "$POST_META" "$PRE" "$POST"
"##,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            pane_q = pane_q,
            tmux = tmux_cmd,
            modal_block = modal_block_snippet,
            settle_cmd = settle_cmd,
            post_sleep_cmd = post_sleep_cmd,
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

        // Pure transport RTT measurement:
        // - Cold connect / explicit probe: ping FIRST on `lane_interactive` (warms ControlMaster
        //   without concurrent race and populates `rtt_ms`).
        // - Warm periodic 15s refresh: run concurrently with discovery below (0ms added latency).
        let is_cold_or_probe = bypass_backoff || self.transport.rtt_ms().is_none();
        let periodic_ping_due = !is_cold_or_probe
            && now_before.saturating_sub(self.last_ping_ms.load(Ordering::Acquire))
                >= SSH_PING_INTERVAL_MS;
        if is_cold_or_probe {
            self.maybe_refresh_rtt(true).await;
        }

        let sync_started = Instant::now();
        let (
            tick,
            cached_fps,
            prev_agy_sig,
            prev_cx_sig,
            known_tr_paths,
            resolved_agy_pids,
            force_panes,
            hot_cursors,
        ) = {
            let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            ds.discover_tick = ds.discover_tick.saturating_add(1);
            let tick = ds.discover_tick;
            (
                tick,
                ds.cached_tail_fingerprints(tick),
                ds.agy_db_sig.clone(),
                ds.codex_db_sig.clone(),
                ds.known_transcript_paths(),
                ds.resolved_agy_pid_births(),
                ds.force_capture_panes(now_before),
                ds.hot_transcript_cursors(&self.host_id, &self.mirror),
            )
        };

        let script = self.compound_discover_script_v3(
            &cached_fps,
            prev_agy_sig.as_deref(),
            prev_cx_sig.as_deref(),
            &known_tr_paths,
            &resolved_agy_pids,
            &force_panes,
            &hot_cursors,
        );
        let lane = if bypass_backoff {
            ConduitLane::Interactive
        } else {
            ConduitLane::Background
        };
        let timeout = self.transport.timeout();
        let exec_res = if periodic_ping_due {
            let (res, _) = tokio::join!(
                self.exec_conduit_idempotent(lane, &script, None, timeout),
                self.maybe_refresh_rtt(false),
            );
            res
        } else {
            self.exec_conduit_idempotent(lane, &script, None, timeout)
                .await
        };

        let now_hub = id::now_ms();
        let out = match exec_res {
            Ok(o) if o.success() => o.into_utf8_lossy(),
            Ok(o) => {
                let msg = format!(
                    "ssh {} failed (exit {:?}): {}",
                    self.host_id,
                    o.exit_code,
                    o.stderr.trim()
                );
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
            Err(e) => {
                let msg = e.to_string();
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
        };

        let mut parsed = match parse_ssh_compound_v3_output(&out.stdout, now_hub) {
            Ok(p) => p,
            Err(msg) => {
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(now_hub, msg.clone());
                return Err(msg);
            }
        };
        let hot_deltas = std::mem::take(&mut parsed.hot_deltas);

        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_success();

        let (mut sessions, live_pane_ids, events, need_mirror, remote_home, tail_map) = {
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
            let tails: HashMap<PaneId, String> = ds
                .tail_cache
                .iter()
                .map(|(p, e)| (p.clone(), e.tail.clone()))
                .collect();
            (
                rec.sessions,
                rec.live_pane_ids,
                rec.events,
                rec.need_mirror,
                rh,
                tails,
            )
        };

        let mut output_deltas: Vec<DaemonEvent> = Vec::new();
        let mut emitted_sids: HashSet<SessionId> = HashSet::new();

        // Step 1: Ingest inline `<= 64 KiB` hot deltas (0 extra SSH round-trips!) and fall back
        // to persistent conduit mirroring (`ConduitLane::Background`) for any remaining updates.
        if !need_mirror.is_empty() && !remote_home.is_empty() {
            let mut remaining_mirror = Vec::new();
            let mut inline_synced = Vec::new();

            for (pane, agent, remote_path, stat_opt) in need_mirror {
                if RemoteTranscriptMirror::validate_remote_transcript_path(
                    &remote_home,
                    &remote_path,
                )
                .is_err()
                {
                    continue;
                }
                if let (Some(stat), Some(hd)) = (stat_opt, hot_deltas.get(&remote_path)) {
                    if hd.stat.ino == stat.ino && hd.stat.size == stat.size {
                        match self.mirror.ingest_inline_delta(
                            self.host_id.as_str(),
                            &remote_home,
                            agent,
                            &remote_path,
                            hd.stat,
                            hd.base_offset,
                            &hd.bytes,
                        ) {
                            Ok(sync_res) => {
                                let (metrics, model, effort) = {
                                    let mut guard = self
                                        .transcript_index
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner());
                                    guard.sync_and_telemetry(agent, &sync_res.local_path, now_hub)
                                };
                                inline_synced.push((
                                    pane,
                                    agent,
                                    remote_path,
                                    Some(stat),
                                    sync_res.local_path,
                                    metrics,
                                    model,
                                    effort,
                                ));
                                continue;
                            }
                            Err(e) => {
                                log::debug!(
                                    "ssh {} inline delta fallback for {}: {e}",
                                    self.host_id,
                                    remote_path
                                );
                            }
                        }
                    }
                }
                remaining_mirror.push((pane, agent, remote_path, stat_opt));
            }

            // Cap synchronous per-sync cold/delta mirror pulls at top-8 active sessions;
            // older dormant sessions still mirror on-demand via `fetch_transcript`.
            if remaining_mirror.len() > SSH_WARM_TOP_N {
                let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                remaining_mirror.sort_by(|a, b| {
                    let rank = |p: &PaneId| -> (u8, u64) {
                        match ds.pane_watch.get(p) {
                            Some(w) if w.state == SessionState::Working => (2, w.last_event_ms),
                            Some(w) => (1, w.last_event_ms),
                            None => (0, 0),
                        }
                    };
                    rank(&b.0).cmp(&rank(&a.0))
                });
                remaining_mirror.truncate(SSH_WARM_TOP_N);
            }

            let mut conduit_synced = Vec::with_capacity(remaining_mirror.len());
            for (pane, agent, remote_path, stat_opt) in remaining_mirror {
                match self
                    .mirror
                    .sync_telemetry_conduit(
                        &self.conduit_pool,
                        lane,
                        &self.transcript_index,
                        self.host_id.as_str(),
                        &remote_home,
                        agent,
                        &remote_path,
                        stat_opt,
                        now_hub,
                    )
                    .await
                {
                    Ok((metrics, model, effort, sync_res)) => {
                        let resolved_stat = stat_opt.or_else(|| {
                            self.mirror.cached_remote_stat(
                                self.host_id.as_str(),
                                agent,
                                &remote_path,
                            )
                        });
                        conduit_synced.push((
                            pane,
                            agent,
                            remote_path,
                            resolved_stat,
                            sync_res.local_path,
                            metrics,
                            model,
                            effort,
                        ));
                    }
                    Err(e) => {
                        log::debug!(
                            "ssh {} conduit transcript mirror failed for {}: {e}",
                            self.host_id,
                            remote_path
                        );
                    }
                }
            }

            inline_synced.extend(conduit_synced);
            if !inline_synced.is_empty() {
                let cached_models: HashMap<PaneId, String> = {
                    let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                    ds.resolved_by_pane
                        .iter()
                        .filter_map(|(p, r)| r.model.as_ref().map(|m| (p.clone(), m.clone())))
                        .collect()
                };
                for (pane, agent, remote_path, _, _, tr_metrics, tr_model, _) in &mut inline_synced
                {
                    if *agent != AgentKind::Agy {
                        continue;
                    }
                    let Some(conv_id) = extract_session_id_from_transcript_path(
                        AgentKind::Agy,
                        Path::new(remote_path),
                    ) else {
                        continue;
                    };
                    let timings = self
                        .fetch_remote_agy_step_timings(lane, &remote_home, &conv_id)
                        .await;
                    if !timings.is_empty() {
                        let model_hint = tr_model
                            .as_deref()
                            .or_else(|| cached_models.get(pane).map(String::as_str));
                        if let Some(enriched) = agy_step_timings_to_metrics(
                            &timings,
                            model_hint,
                            tr_metrics.clone(),
                            now_hub,
                        ) {
                            *tr_metrics = Some(enriched);
                        }
                    }
                }
                let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                for (
                    pane,
                    agent,
                    remote_path,
                    stat_opt,
                    local_path,
                    tr_metrics,
                    tr_model,
                    tr_effort,
                ) in inline_synced
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
                    if let Some(st) = stat_opt {
                        ds.latest_tr_stats.insert(remote_path.clone(), st);
                    }
                    if let Some(entry) = ds.resolved_by_pane.get_mut(&pane) {
                        if entry.path == remote_path {
                            if let Some(st) = stat_opt {
                                entry.last_stat = Some(st);
                            }
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
                    let mut final_metrics = tr_metrics.clone();
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
                            final_metrics = merged;
                        }
                        if let Some(lk) = ds.session_lookup.get_mut(&local_sid) {
                            if let Some(st) = stat_opt {
                                lk.latest_stat = Some(st);
                            }
                            lk.metrics = rec.metrics.clone();
                        }
                    }
                    let live_tail = tail_map.get(&pane).cloned();
                    if let Some(ev) = self.build_transcript_output_delta(
                        &mut ds,
                        &qualified_sid,
                        agent,
                        &local_path,
                        tick > 1,
                        live_tail,
                        final_metrics,
                        now_hub,
                    ) {
                        emitted_sids.insert(qualified_sid);
                        output_deltas.push(ev);
                    }
                }
            }
        }

        // Emit `live_tail` `DaemonEvent::Output` for active panes whose terminal tail changed
        // without already emitting a transcript delta above.
        {
            let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            for rec in &sessions {
                if rec.is_subagent || rec.state == SessionState::Ended {
                    continue;
                }
                if emitted_sids.contains(&rec.session_id) {
                    continue;
                }
                let Some(local_pane) = rec.pane.as_ref().map(|p| {
                    let (_, raw) = starfane_tmux::split_host_prefix(p.as_str());
                    PaneId::new(raw)
                }) else {
                    continue;
                };
                let Some(tail) = tail_map.get(&local_pane) else {
                    continue;
                };
                let h = hash_tail(tail);
                let prev_h = ds.last_emitted_tail_hash.insert(local_pane, h);
                if tick > 1 && prev_h.is_some_and(|ph| ph != h) {
                    output_deltas.push(DaemonEvent::Output {
                        seq: 0,
                        session_id: rec.session_id.clone(),
                        start_seq: None,
                        total_messages: None,
                        appended_messages: Vec::new(),
                        live_tail: Some(tail.clone()),
                        metrics: rec.metrics.clone(),
                        dirty: false,
                    });
                }
            }
            let live_sids: HashSet<&SessionId> = sessions
                .iter()
                .filter(|s| s.state != SessionState::Ended)
                .map(|s| &s.session_id)
                .collect();
            ds.last_emitted_watermarks
                .retain(|sid, _| live_sids.contains(sid));
            ds.last_sessions = sessions.clone();
        }

        // Proactively pre-warm Hub RAM `TranscriptIndexCache` (`tail(60)`) for top 8 active
        // transcripts in the background so first click in browser hits RAM in <1ms.
        self.spawn_warm_top_transcripts(&sessions, &remote_home, now_hub);

        let elapsed_sync_ms = sync_started
            .elapsed()
            .as_millis()
            .min((RTT_UNSET - 1) as u128) as u64;
        self.last_sync_ms.store(elapsed_sync_ms, Ordering::Release);

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
                sync_ms: Some(elapsed_sync_ms),
                next_retry_ms: None,
                retry_paused: false,
            },
            sessions,
            live_pane_ids,
            events,
            output_deltas,
        })
    }

    async fn fetch_remote_agy_step_timings(
        &self,
        lane: ConduitLane,
        remote_home: &str,
        conv_id: &str,
    ) -> Vec<AgyStepTiming> {
        let rh = remote_home.trim_end_matches('/');
        if rh.is_empty()
            || conv_id.is_empty()
            || conv_id.contains('/')
            || conv_id.contains("..")
            || !conv_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Vec::new();
        }
        let db_path = format!("{rh}/.gemini/antigravity-cli/conversations/{conv_id}.db");
        let db_q = shell_single_quote(&db_path);
        let script = format!(
            r#"{bootstrap}_db={db_q}
if [ -f "$_db" ] && [ ! -L "$_db" ] && command -v sqlite3 >/dev/null 2>&1; then
  _tab=$(printf '\t')
  _sql='SELECT idx, hex(metadata) FROM steps WHERE step_type=15 AND status=3 AND metadata IS NOT NULL ORDER BY idx DESC LIMIT 20;'
  sqlite3 -readonly -separator "$_tab" "$_db" "$_sql" 2>/dev/null || sqlite3 -separator "$_tab" "file:${{_db}}?immutable=1" "$_sql" 2>/dev/null || true
fi
"#,
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            db_q = db_q,
        );
        let Ok(out) = self
            .exec_conduit_idempotent(lane, &script, None, self.transport.timeout())
            .await
        else {
            return Vec::new();
        };
        if !out.success() {
            return Vec::new();
        }
        let stdout = out.into_utf8_lossy().stdout;
        parse_agy_step_timing_tsv(&stdout)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_transcript_output_delta(
        &self,
        ds: &mut SshDiscoveryState,
        qualified_sid: &SessionId,
        agent: AgentKind,
        local_path: &Path,
        allow_emit: bool,
        live_tail: Option<String>,
        metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> Option<DaemonEvent> {
        let mut cache = self
            .transcript_index
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let wm = cache.watermark_for_path(agent, local_path)?;
        let prev_opt = ds.last_emitted_watermarks.insert(qualified_sid.clone(), wm);
        if !allow_emit {
            return None;
        }
        let prev = prev_opt.unwrap_or_default();

        let (start_from, count) = if wm.total_messages > prev.total_messages {
            let prev_last_mutated = prev.total_messages > 0
                && (prev.has_pending_tools
                    || cache.turn_hash_at_for_path(agent, local_path, prev.total_messages - 1)
                        != Some(prev.last_turn_hash));
            let sf = if prev_last_mutated {
                prev.total_messages - 1
            } else {
                prev.total_messages
            };
            (sf, wm.total_messages - sf)
        } else if wm.total_messages == prev.total_messages
            && wm.total_messages > 0
            && (wm.last_turn_hash != prev.last_turn_hash
                || wm.has_pending_tools != prev.has_pending_tools)
        {
            (wm.total_messages - 1, 1)
        } else if wm.total_messages < prev.total_messages {
            return Some(DaemonEvent::Output {
                seq: 0,
                session_id: qualified_sid.clone(),
                start_seq: None,
                total_messages: Some(wm.total_messages),
                appended_messages: Vec::new(),
                live_tail,
                metrics,
                dirty: true,
            });
        } else {
            return None;
        };

        if count <= SSH_INLINE_OUTPUT_MAX_TURNS {
            if let Some(resp) = cache.get_page_or_sync(
                agent,
                local_path,
                TranscriptQuery::tail(count),
                metrics.clone(),
                now_ms,
            ) {
                if resp.start_seq == start_from {
                    if let Some(values) = Self::serialize_inline_output_messages(
                        &resp.messages,
                        SSH_INLINE_OUTPUT_MAX_BYTES,
                    ) {
                        return Some(DaemonEvent::Output {
                            seq: 0,
                            session_id: qualified_sid.clone(),
                            start_seq: Some(resp.start_seq),
                            total_messages: Some(wm.total_messages),
                            appended_messages: values,
                            live_tail,
                            metrics,
                            dirty: false,
                        });
                    }
                }
            }
        }
        Some(DaemonEvent::Output {
            seq: 0,
            session_id: qualified_sid.clone(),
            start_seq: Some(start_from),
            total_messages: Some(wm.total_messages),
            appended_messages: Vec::new(),
            live_tail,
            metrics,
            dirty: true,
        })
    }

    fn serialize_inline_output_messages(
        messages: &[TranscriptMessage],
        max_bytes: usize,
    ) -> Option<Vec<serde_json::Value>> {
        if messages.is_empty() {
            return None;
        }
        let mut values = Vec::with_capacity(messages.len());
        let mut total_bytes = 0usize;
        for m in messages {
            let v = serde_json::to_value(m).ok()?;
            total_bytes = total_bytes.saturating_add(v.to_string().len());
            if total_bytes > max_bytes {
                return None;
            }
            values.push(v);
        }
        (!values.is_empty()).then_some(values)
    }

    fn spawn_warm_top_transcripts(
        &self,
        sessions: &[SessionRecord],
        remote_home: &str,
        now_ms: u64,
    ) {
        if remote_home.is_empty()
            || self
                .warm_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let mut candidates: Vec<(u8, u64, AgentKind, String, Option<RemoteFileStat>)> = {
            let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            sessions
                .iter()
                .filter(|s| !s.is_subagent && s.state != SessionState::Ended && !s.agent.is_shell())
                .filter_map(|s| {
                    let path = s.transcript_path.as_ref()?.clone();
                    let stat = ds.latest_tr_stats.get(&path).copied();
                    let prio = match s.state {
                        SessionState::Working => 3,
                        SessionState::Starting
                        | SessionState::NeedsPermission
                        | SessionState::NeedsInput => 2,
                        _ => 1,
                    };
                    Some((prio, s.last_event_ms, s.agent, path, stat))
                })
                .collect()
        };
        if candidates.is_empty() {
            self.warm_in_flight.store(false, Ordering::Release);
            return;
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        candidates.truncate(SSH_WARM_TOP_N);

        let pool = self.conduit_pool.clone();
        let mirror = self.mirror.clone();
        let tr_cache = self.transcript_index.clone();
        let host_id = self.host_id.clone();
        let rhome = remote_home.to_string();
        let flag = self.warm_in_flight.clone();

        tokio::spawn(async move {
            for (_, _, agent, remote_path, stat) in candidates {
                if RemoteTranscriptMirror::validate_remote_transcript_path(&rhome, &remote_path)
                    .is_err()
                {
                    continue;
                }
                if let Ok(sync_res) = mirror
                    .ensure_fresh_conduit(
                        &pool,
                        ConduitLane::Background,
                        host_id.as_str(),
                        &rhome,
                        agent,
                        &remote_path,
                        stat,
                        false,
                    )
                    .await
                {
                    let mut guard = tr_cache.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = guard.get_page_or_sync(
                        agent,
                        &sync_res.local_path,
                        TranscriptQuery::tail(60),
                        None,
                        now_ms,
                    );
                }
            }
            flag.store(false, Ordering::Release);
        });
    }

    /// Fast targeted sync (~18ms single-RTT on `ConduitLane::Interactive`) for dirty panes signaled
    /// by `SshTmuxControlWatcher`, skipping full `ps`/`sqlite3`/`lsof` unless pane topology changed.
    async fn run_sync_dirty_panes_inner(
        &self,
        display_name: &str,
        dirty_panes: &[PaneId],
        claimed_external_ids: &HashSet<String>,
    ) -> Result<NodeSnapshot, String> {
        if dirty_panes.is_empty() {
            return self
                .run_sync_inner(display_name, claimed_external_ids, false)
                .await;
        }

        let (remote_home, has_baseline, unknown_pane, targets, cached_sessions) = {
            let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            let rh = ds.remote_home.clone().unwrap_or_default();
            let has_base = !rh.is_empty() && !ds.last_sessions.is_empty();
            let mut seen = HashSet::new();
            let mut unknown = false;
            let mut tgts: Vec<DirtyPaneTarget> = Vec::new();
            for p in dirty_panes {
                if validate_tmux_pane_id(p).is_err() || !seen.insert(p.clone()) {
                    continue;
                }
                let Some(&agent) = ds.pane_agent.get(p) else {
                    // Unknown pane -> topology may have changed, trigger full sync!
                    unknown = true;
                    break;
                };
                if agent.is_shell() && !self.include_shell_panes {
                    continue;
                }
                let hot = ds.resolved_by_pane.get(p).map(|res| {
                    let (ino, offset) = self
                        .mirror
                        .cached_sync_cursor(self.host_id.as_str(), agent, &res.path)
                        .unwrap_or((0, 0));
                    HotCursorSpec {
                        path: res.path.clone(),
                        ino,
                        offset,
                    }
                });
                tgts.push(DirtyPaneTarget {
                    pane: p.clone(),
                    agent,
                    hot,
                });
            }
            let cached = if !unknown && has_base && tgts.is_empty() {
                Some(ds.last_sessions.clone())
            } else {
                None
            };
            (rh, has_base, unknown, tgts, cached)
        };

        if unknown_pane || !has_baseline {
            return self
                .run_sync_inner(display_name, claimed_external_ids, false)
                .await;
        }
        if targets.is_empty() {
            let sessions = cached_sessions.unwrap_or_default();
            let count = sessions
                .iter()
                .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
                .count();
            return Ok(NodeSnapshot {
                host: HostRecord {
                    id: self.host_id.clone(),
                    display_name: display_name.to_string(),
                    kind: HostKind::Ssh,
                    status: HostStatus::Online,
                    active_endpoint: Some(self.transport.target().to_string()),
                    last_seen_ms: Some(id::now_ms()),
                    error: None,
                    session_count: count,
                    rtt_ms: self.transport.rtt_ms(),
                    sync_ms: self.sync_ms(),
                    next_retry_ms: None,
                    retry_paused: false,
                },
                sessions,
                live_pane_ids: HashSet::new(),
                events: Vec::new(),
                output_deltas: Vec::new(),
            });
        }

        let script = self.build_dirty_panes_script(&targets);
        let started = Instant::now();
        let out = match self
            .exec_conduit_idempotent(
                ConduitLane::Interactive,
                &script,
                None,
                self.transport.timeout(),
            )
            .await
        {
            Ok(o) if o.success() => o.into_utf8_lossy(),
            Ok(o) => {
                let msg = format!(
                    "ssh {} dirty sync failed (exit {:?}): {}",
                    self.host_id,
                    o.exit_code,
                    o.stderr.trim()
                );
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(id::now_ms(), msg.clone());
                return Err(msg);
            }
            Err(e) => {
                let msg = e.to_string();
                self.circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .record_failure(id::now_ms(), msg.clone());
                return Err(msg);
            }
        };

        let parsed = parse_dirty_panes_output(&out.stdout);
        // If any dirty pane disappeared, died, switched foreground command, or is a Codex pane
        // whose live status-bar thread title changed (`/resume` or `/new`), fall back to full sync.
        let need_full_fallback = !parsed.missing_panes.is_empty() || {
            let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            let meta_changed = parsed.pane_metas.iter().any(|(pane, meta)| {
                meta.dead
                    || ds
                        .tail_cache
                        .get(pane)
                        .is_some_and(|e| e.current_command != meta.current_command)
            });
            let codex_title_changed = targets.iter().any(|t| {
                if t.agent != AgentKind::Codex {
                    return false;
                }
                let Some(new_tail) = parsed.captured_tails.get(&t.pane) else {
                    return false;
                };
                let new_title = extract_codex_status_title(new_tail);
                let prev_title = ds
                    .resolved_by_pane
                    .get(&t.pane)
                    .and_then(|r| r.title_hint.as_deref())
                    .map(str::to_string)
                    .or_else(|| {
                        ds.tail_cache
                            .get(&t.pane)
                            .and_then(|e| extract_codex_status_title(&e.tail))
                    });
                new_title != prev_title
                    || (new_title.is_some() && !ds.resolved_by_pane.contains_key(&t.pane))
            });
            meta_changed || codex_title_changed
        };
        if need_full_fallback {
            return self
                .run_sync_inner(display_name, claimed_external_ids, false)
                .await;
        }

        let now_hub = id::now_ms();
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_success();

        // Ingest inline hot deltas or fall back to conduit delta pull for >64 KiB updates.
        let mut mirror_updates: Vec<DirtyMirrorUpdate> = Vec::new();
        for target in &targets {
            let pane = &target.pane;
            let agent = target.agent;
            let Some(ref hot) = target.hot else {
                continue;
            };
            let remote_path = &hot.path;
            let Some(&stat) = parsed.tr_stats.get(remote_path) else {
                continue;
            };
            let prev_stat = {
                let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                ds.resolved_by_pane.get(pane).and_then(|r| r.last_stat)
            };
            if prev_stat.is_some_and(|ps| {
                ps.ino == stat.ino && ps.size == stat.size && ps.mtime_s == stat.mtime_s
            }) {
                continue;
            }
            if RemoteTranscriptMirror::validate_remote_transcript_path(&remote_home, remote_path)
                .is_err()
            {
                continue;
            }
            if let Some(hd) = parsed.hot_deltas.get(remote_path) {
                if hd.stat.ino == stat.ino && hd.stat.size == stat.size {
                    if let Ok(sync_res) = self.mirror.ingest_inline_delta(
                        self.host_id.as_str(),
                        &remote_home,
                        agent,
                        remote_path,
                        hd.stat,
                        hd.base_offset,
                        &hd.bytes,
                    ) {
                        let (metrics, model, effort) = {
                            let mut guard = self
                                .transcript_index
                                .lock()
                                .unwrap_or_else(|p| p.into_inner());
                            guard.sync_and_telemetry(agent, &sync_res.local_path, now_hub)
                        };
                        mirror_updates.push(DirtyMirrorUpdate {
                            pane: pane.clone(),
                            agent,
                            remote_path: remote_path.clone(),
                            stat,
                            local_path: sync_res.local_path,
                            metrics,
                            model,
                            effort,
                        });
                        continue;
                    }
                }
            }
            if let Ok((metrics, model, effort, sync_res)) = self
                .mirror
                .sync_telemetry_conduit(
                    &self.conduit_pool,
                    ConduitLane::Interactive,
                    &self.transcript_index,
                    self.host_id.as_str(),
                    &remote_home,
                    agent,
                    remote_path,
                    Some(stat),
                    now_hub,
                )
                .await
            {
                mirror_updates.push(DirtyMirrorUpdate {
                    pane: pane.clone(),
                    agent,
                    remote_path: remote_path.clone(),
                    stat,
                    local_path: sync_res.local_path,
                    metrics,
                    model,
                    effort,
                });
            }
        }

        if !mirror_updates.is_empty() && !remote_home.is_empty() {
            let cached_models: HashMap<PaneId, String> = {
                let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
                ds.resolved_by_pane
                    .iter()
                    .filter_map(|(p, r)| r.model.as_ref().map(|m| (p.clone(), m.clone())))
                    .collect()
            };
            for upd in &mut mirror_updates {
                if upd.agent != AgentKind::Agy {
                    continue;
                }
                let Some(conv_id) = extract_session_id_from_transcript_path(
                    AgentKind::Agy,
                    Path::new(&upd.remote_path),
                ) else {
                    continue;
                };
                let timings = self
                    .fetch_remote_agy_step_timings(ConduitLane::Interactive, &remote_home, &conv_id)
                    .await;
                if !timings.is_empty() {
                    let model_hint = upd
                        .model
                        .as_deref()
                        .or_else(|| cached_models.get(&upd.pane).map(String::as_str));
                    if let Some(enriched) = agy_step_timings_to_metrics(
                        &timings,
                        model_hint,
                        upd.metrics.clone(),
                        now_hub,
                    ) {
                        upd.metrics = Some(enriched);
                    }
                }
            }
        }

        let mut events = Vec::new();
        let mut output_deltas = Vec::new();
        let mut emitted_sids = HashSet::new();

        let sessions = {
            let mut ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
            let mut sessions = ds.last_sessions.clone();
            for (path, stat) in &parsed.tr_stats {
                ds.latest_tr_stats.insert(path.clone(), *stat);
            }

            // Apply updated pane tails & heuristic state watch.
            for target in &targets {
                let pane = &target.pane;
                let agent = target.agent;
                let local_sid = legacy_session_id(pane);
                let qualified_sid = qualify_host_session(&self.host_id, &local_sid);
                let qualified_pane = qualify_host_pane(&self.host_id, pane);
                let agent_pid = ds.pane_agent_pid.get(pane).copied();
                if let (Some(meta), Some(tail)) =
                    (parsed.pane_metas.get(pane), parsed.captured_tails.get(pane))
                {
                    ds.tail_cache.insert(
                        pane.clone(),
                        SshTailCacheEntry {
                            history_size: meta.history_size,
                            cursor_x: meta.cursor_x,
                            cursor_y: meta.cursor_y,
                            in_mode: meta.in_mode,
                            current_command: meta.current_command.clone(),
                            agent_pid,
                            tail: tail.clone(),
                        },
                    );
                    if let Some(rec) = sessions.iter_mut().find(|s| s.session_id == qualified_sid) {
                        let (st, st_since, last_ev_ms, last_err, hev) = apply_ssh_pane_tail_watch(
                            &mut ds.pane_watch,
                            pane,
                            &qualified_sid,
                            &qualified_pane,
                            agent,
                            rec.cwd.as_deref(),
                            rec.model.as_deref(),
                            tail,
                            rec.state,
                            now_hub,
                        );
                        rec.state = st;
                        rec.state_since_ms = st_since;
                        rec.last_event_ms = last_ev_ms;
                        rec.last_error = last_err;
                        if let Some(ev) = hev {
                            events.push(ev);
                        }
                        if let Some(st_m) = parse_status_line_metrics(agent, tail, now_hub) {
                            let tr_m = ds
                                .resolved_by_pane
                                .get(pane)
                                .and_then(|r| r.metrics.clone());
                            rec.metrics = merge_metrics(tr_m, Some(st_m));
                        }
                        if agent == AgentKind::Codex
                            && (rec.model.is_none() || rec.effort.is_none())
                        {
                            let (st_model, st_effort) = parse_codex_status_line_model_effort(tail);
                            if rec.model.is_none() {
                                rec.model = st_model;
                            }
                            if rec.effort.is_none() {
                                rec.effort = st_effort;
                            }
                        }
                    }
                }
            }

            // Apply transcript delta updates & emit `DaemonEvent::Output`.
            for DirtyMirrorUpdate {
                pane,
                agent,
                remote_path,
                stat,
                local_path,
                metrics: tr_metrics,
                model: tr_model,
                effort: tr_effort,
            } in mirror_updates
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
                let mut final_metrics = tr_metrics.clone();
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
                        final_metrics = merged;
                    }
                    if let Some(lk) = ds.session_lookup.get_mut(&local_sid) {
                        lk.latest_stat = Some(stat);
                        lk.metrics = rec.metrics.clone();
                    }
                }
                let live_tail = parsed.captured_tails.get(&pane).cloned();
                if let Some(ev) = self.build_transcript_output_delta(
                    &mut ds,
                    &qualified_sid,
                    agent,
                    &local_path,
                    true,
                    live_tail,
                    final_metrics,
                    now_hub,
                ) {
                    emitted_sids.insert(qualified_sid);
                    output_deltas.push(ev);
                }
            }

            // Emit `live_tail` `DaemonEvent::Output` for dirty panes whose tail changed.
            for target in &targets {
                let pane = &target.pane;
                let qualified_sid = qualify_host_session(&self.host_id, &legacy_session_id(pane));
                if emitted_sids.contains(&qualified_sid) {
                    continue;
                }
                let Some(tail) = parsed.captured_tails.get(pane) else {
                    continue;
                };
                let h = hash_tail(tail);
                let prev_h = ds.last_emitted_tail_hash.insert(pane.clone(), h);
                if prev_h.is_none_or(|ph| ph != h) {
                    let metrics = sessions
                        .iter()
                        .find(|s| s.session_id == qualified_sid)
                        .and_then(|s| s.metrics.clone());
                    output_deltas.push(DaemonEvent::Output {
                        seq: 0,
                        session_id: qualified_sid,
                        start_seq: None,
                        total_messages: None,
                        appended_messages: Vec::new(),
                        live_tail: Some(tail.clone()),
                        metrics,
                        dirty: false,
                    });
                }
            }

            ds.last_sessions = sessions.clone();
            sessions
        };

        let elapsed_ms = started.elapsed().as_millis().min((RTT_UNSET - 1) as u128) as u64;
        self.last_sync_ms.store(elapsed_ms, Ordering::Release);
        let live_pane_ids: HashSet<PaneId> = sessions
            .iter()
            .filter(|s| s.state != SessionState::Ended)
            .filter_map(|s| s.pane.clone())
            .collect();
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
                sync_ms: Some(elapsed_ms),
                next_retry_ms: None,
                retry_paused: false,
            },
            sessions,
            live_pane_ids,
            events,
            output_deltas,
        })
    }
}

#[derive(Debug, Clone)]
struct HotCursorSpec {
    path: String,
    ino: u64,
    offset: u64,
}

#[derive(Debug, Clone)]
struct DirtyPaneTarget {
    pane: PaneId,
    agent: AgentKind,
    hot: Option<HotCursorSpec>,
}

struct DirtyMirrorUpdate {
    pane: PaneId,
    agent: AgentKind,
    remote_path: String,
    stat: RemoteFileStat,
    local_path: PathBuf,
    metrics: Option<SessionMetrics>,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone)]
struct DirtyPaneMeta {
    dead: bool,
    in_mode: bool,
    history_size: u32,
    cursor_x: u32,
    cursor_y: u32,
    current_command: String,
}

#[derive(Debug, Default)]
struct ParsedDirtyPanes {
    missing_panes: Vec<PaneId>,
    pane_metas: HashMap<PaneId, DirtyPaneMeta>,
    captured_tails: HashMap<PaneId, String>,
    tr_stats: HashMap<String, RemoteFileStat>,
    hot_deltas: HashMap<String, InlineHotDelta>,
}

fn parse_dirty_panes_output(stdout: &str) -> ParsedDirtyPanes {
    let clean = stdout.replace("\r\n", "\n");
    let (panes_part, deltas_part) = match split_marker_line_last(&clean, SEC_HOT_DELTAS) {
        Some((p, d)) => (p, d),
        None => (clean.as_str(), ""),
    };
    let mut out = ParsedDirtyPanes::default();
    let mut cur_pane: Option<PaneId> = None;
    let mut cur_buf = String::new();
    for line in panes_part.lines() {
        if let Some(pid_raw) = line.strip_prefix("__SF_DIRTY_MISSING__ ") {
            out.missing_panes.push(PaneId::new(pid_raw.trim()));
        } else if let Some(rest) = line.strip_prefix("__SF_DIRTY_META__ ") {
            let fields = starfane_tmux::split_tmux_fields(rest);
            if fields.len() >= 7 {
                let pane = PaneId::new(fields[0].trim());
                out.pane_metas.insert(
                    pane,
                    DirtyPaneMeta {
                        dead: fields[1].trim() == "1",
                        in_mode: fields[2].trim() == "1",
                        history_size: fields[3].trim().parse::<u32>().unwrap_or(0),
                        cursor_x: fields[4].trim().parse::<u32>().unwrap_or(0),
                        cursor_y: fields[5].trim().parse::<u32>().unwrap_or(0),
                        current_command: fields[6].trim().to_string(),
                    },
                );
            }
        } else if let Some(pid_raw) = line.strip_prefix("__SF_TAIL_BEGIN__ ") {
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
    for line in deltas_part.lines() {
        if let Some(rest) = line.strip_prefix("__SF_DIRTY_STAT__ ") {
            if let Some((stat, path)) = parse_stat_and_path(rest) {
                out.tr_stats.insert(path, stat);
            }
        }
    }
    parse_hot_deltas_section(deltas_part, &mut out.hot_deltas);
    out
}

// ---------------------------------------------------------------------------
// Zero-Install `SshTmuxControlWatcher` (`tmux -C` + Multi-Session Digest)
// ---------------------------------------------------------------------------

/// Parsed control-mode notification/framing line from `tmux -C`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxControlEvent {
    Output(PaneId),
    ExtendedOutput(PaneId),
    Pause(PaneId),
    Continue(PaneId),
    TopologyChanged { pane: Option<PaneId> },
    Begin { cmd_num: u64 },
    End { cmd_num: u64 },
    Error { cmd_num: u64 },
    Exit(String),
}

/// Parse a single line emitted by `tmux -C` control mode.
pub fn parse_tmux_control_line(line: &str) -> Option<TmuxControlEvent> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if !trimmed.starts_with('%') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let tag = parts.next()?;
    let parse_pane_tok = |tok: Option<&str>| -> Option<PaneId> {
        let s = tok?;
        if s.len() > 1 && s.starts_with('%') && s[1..].bytes().all(|b| b.is_ascii_digit()) {
            Some(PaneId::new(s))
        } else {
            None
        }
    };
    match tag {
        "%output" => Some(TmuxControlEvent::Output(parse_pane_tok(parts.next())?)),
        "%extended-output" => Some(TmuxControlEvent::ExtendedOutput(parse_pane_tok(
            parts.next(),
        )?)),
        "%pause" => Some(TmuxControlEvent::Pause(parse_pane_tok(parts.next())?)),
        "%continue" => Some(TmuxControlEvent::Continue(parse_pane_tok(parts.next())?)),
        "%pane-mode-changed" => Some(TmuxControlEvent::TopologyChanged {
            pane: parse_pane_tok(parts.next()),
        }),
        "%window-add"
        | "%window-close"
        | "%window-renamed"
        | "%unlinked-window-add"
        | "%unlinked-window-close"
        | "%unlinked-window-renamed"
        | "%layout-change"
        | "%sessions-changed"
        | "%session-changed"
        | "%session-renamed" => Some(TmuxControlEvent::TopologyChanged { pane: None }),
        "%begin" => {
            let _ts = parts.next();
            let cmd_num = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Some(TmuxControlEvent::Begin { cmd_num })
        }
        "%end" => {
            let _ts = parts.next();
            let cmd_num = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Some(TmuxControlEvent::End { cmd_num })
        }
        "%error" => {
            let _ts = parts.next();
            let cmd_num = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Some(TmuxControlEvent::Error { cmd_num })
        }
        "%exit" => {
            let reason = trimmed
                .strip_prefix("%exit")
                .unwrap_or("")
                .trim()
                .to_string();
            Some(TmuxControlEvent::Exit(reason))
        }
        _ => None,
    }
}

/// Compact per-pane state digest emitted by zero-fork in-control `tmux list-panes -a -F`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDigest {
    pub pane_id: PaneId,
    pub history_size: u32,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub in_mode: bool,
    pub dead: bool,
    pub current_command: String,
}

/// Parse one pipe-delimited line from `list-panes -a -F '#{pane_id}|#{history_size}|#{cursor_x}|#{cursor_y}|#{pane_in_mode}|#{pane_dead}|#{pane_current_command}'`.
pub fn parse_pane_digest_line(line: &str) -> Option<PaneDigest> {
    let trimmed = line.trim();
    if !trimmed.starts_with('%') {
        return None;
    }
    let mut parts = trimmed.splitn(7, '|');
    let pane_raw = parts.next()?;
    if pane_raw.len() <= 1 || !pane_raw[1..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let history_size = parts.next()?.parse::<u32>().ok()?;
    let cursor_x = parts.next()?.parse::<u32>().ok()?;
    let cursor_y = parts.next()?.parse::<u32>().ok()?;
    let in_mode = parts.next()? == "1";
    let dead = parts.next()? == "1";
    let current_command = parts.next()?.to_string();
    Some(PaneDigest {
        pane_id: PaneId::new(pane_raw),
        history_size,
        cursor_x,
        cursor_y,
        in_mode,
        dead,
        current_command,
    })
}

/// Result of comparing two consecutive multi-session `list-panes -a` digests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DigestDiff {
    pub topology_changed: bool,
    pub dirty_panes: Vec<PaneId>,
}

/// Compare `cur` against `prev`, classifying added/removed/dead/command-changed panes as
/// `topology_changed` (`FullSync`) and cursor/history/mode changes as targeted `dirty_panes`.
pub fn diff_pane_digests(prev: &HashMap<PaneId, PaneDigest>, cur: &[PaneDigest]) -> DigestDiff {
    let mut diff = DigestDiff::default();
    if prev.is_empty() {
        return diff;
    }
    let mut seen = HashSet::with_capacity(cur.len());
    for d in cur {
        seen.insert(&d.pane_id);
        match prev.get(&d.pane_id) {
            None => {
                diff.topology_changed = true;
            }
            Some(old) => {
                if old.dead != d.dead || old.current_command != d.current_command {
                    diff.topology_changed = true;
                } else if old.history_size != d.history_size
                    || old.cursor_x != d.cursor_x
                    || old.cursor_y != d.cursor_y
                    || old.in_mode != d.in_mode
                {
                    diff.dirty_panes.push(d.pane_id.clone());
                }
            }
        }
    }
    if prev.keys().any(|k| !seen.contains(k)) {
        diff.topology_changed = true;
    }
    diff
}

/// Read up to `max_keep` bytes of the next `\n`-terminated line and drain any trailing bytes
/// beyond `max_keep` without allocating unbounded memory (defends against giant `%output` floods).
async fn read_control_line_prefix<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_keep: usize,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(256);
    let mut saw_any = false;
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return if saw_any {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            } else {
                Ok(None)
            };
        }
        saw_any = true;
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            let keep = max_keep.saturating_sub(buf.len()).min(pos);
            if keep > 0 {
                buf.extend_from_slice(&avail[..keep]);
            }
            reader.consume(take);
            if buf.ends_with(b"\r") {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        let n = avail.len();
        let keep = max_keep.saturating_sub(buf.len()).min(n);
        if keep > 0 {
            buf.extend_from_slice(&avail[..keep]);
        }
        reader.consume(n);
    }
}

struct SshTmuxControlWatcher {
    host_id: HostId,
    ssh: Arc<SshExec>,
    tmux_cmd: String,
    include_shell_panes: bool,
    circuit: Arc<Mutex<HostCircuitBreaker>>,
    discovery: Arc<Mutex<SshDiscoveryState>>,
}

impl SshTmuxControlWatcher {
    fn pane_is_interesting(&self, pane: &PaneId) -> bool {
        if self.include_shell_panes {
            return true;
        }
        let ds = self.discovery.lock().unwrap_or_else(|p| p.into_inner());
        if ds.pane_agent.is_empty() {
            return true;
        }
        ds.pane_agent
            .get(pane)
            .is_some_and(|agent| !agent.is_shell())
    }

    fn digest_interval(&self) -> Duration {
        let active = self
            .discovery
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .has_active_session();
        if active {
            Duration::from_millis(WATCH_DIGEST_ACTIVE_MS)
        } else {
            Duration::from_millis(WATCH_DIGEST_IDLE_MS)
        }
    }

    async fn run(self, tx: tokio::sync::mpsc::UnboundedSender<HostPushSignal>) {
        const BACKOFF_SECS: &[u64] = &[1, 2, 5, 15];
        let mut backoff_idx = 0usize;
        let mut ever_connected = false;

        while !tx.is_closed() {
            // Respect paused or open offline circuit breaker instead of hammering unreachable hosts.
            let (paused, wait_offline) = {
                let cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
                let paused = cb.is_retry_paused();
                let wait = if !paused && cb.status() == HostStatus::Offline {
                    let now = id::now_ms();
                    cb.active_backoff_until(now).map(|until| {
                        Duration::from_millis(until.saturating_sub(now).clamp(500, 15_000))
                    })
                } else {
                    None
                };
                (paused, wait)
            };
            if paused {
                return;
            }
            if let Some(dur) = wait_offline {
                tokio::select! {
                    _ = tokio::time::sleep(dur) => continue,
                    _ = tx.closed() => return,
                }
            }

            let session_start = Instant::now();
            let connected = self.run_once(&tx, ever_connected).await;
            if tx.is_closed() {
                return;
            }
            if connected {
                ever_connected = true;
                if session_start.elapsed() >= Duration::from_secs(10) {
                    backoff_idx = 0;
                }
            }
            let delay = Duration::from_secs(BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(15));
            if backoff_idx + 1 < BACKOFF_SECS.len() {
                backoff_idx += 1;
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = tx.closed() => return,
            }
        }
    }

    async fn run_once(
        &self,
        tx: &tokio::sync::mpsc::UnboundedSender<HostPushSignal>,
        was_connected: bool,
    ) -> bool {
        let watch_script = format!(
            "{bootstrap}SESS=$({tmux} list-sessions -F '#{{session_name}}' 2>/dev/null | head -n 1)\n\
             [ -n \"$SESS\" ] || exit 75\n\
             exec {tmux} -C attach-session -t \"=$SESS\"\n",
            bootstrap = REMOTE_PATH_BOOTSTRAP,
            tmux = self.tmux_cmd,
        );
        let mut args = self.ssh.build_ssh_base_args();
        args.push("-T".to_string());
        args.push(self.ssh.target().to_string());
        args.push("--".to_string());
        args.push(format!(
            "/bin/sh -c {}",
            starfane_tmux::ssh_universal_quote(&watch_script)
        ));

        let mut cmd = tokio::process::Command::new(self.ssh.ssh_bin());
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill().await;
            return false;
        };
        let mut reader = BufReader::with_capacity(16 * 1024, stdout);

        let mut handshake_done = false;
        let mut sent_pause_fallback = false;
        let mut flow_cmd_in_flight = false;
        let mut digest_in_flight = false;
        let mut digest_collecting = false;
        let mut digest_rows: Vec<PaneDigest> = Vec::new();
        let mut prev_digests: HashMap<PaneId, PaneDigest> = HashMap::new();

        let mut pending_dirty: HashSet<PaneId> = HashSet::new();
        let mut paused_panes: HashSet<PaneId> = HashSet::new();
        let mut pending_full_sync = false;
        let mut debounce_deadline: Option<tokio::time::Instant> = None;
        let mut next_digest_at = tokio::time::Instant::now() + self.digest_interval();

        let digest_cmd = "list-panes -a -F '#{pane_id}|#{history_size}|#{cursor_x}|#{cursor_y}|#{pane_in_mode}|#{pane_dead}|#{pane_current_command}'\n";

        loop {
            let debounce_fut = async {
                match debounce_deadline {
                    Some(dl) => tokio::time::sleep_until(dl).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tx.closed() => {
                    let _ = child.kill().await;
                    return handshake_done;
                }
                _ = debounce_fut => {
                    debounce_deadline = None;
                    if pending_full_sync {
                        pending_full_sync = false;
                        pending_dirty.clear();
                        if tx.send(HostPushSignal::FullSync).is_err() {
                            let _ = child.kill().await;
                            return handshake_done;
                        }
                    } else if !pending_dirty.is_empty() {
                        let dirty: Vec<PaneId> = pending_dirty.drain().collect();
                        if tx.send(HostPushSignal::DirtyPanes(dirty)).is_err() {
                            let _ = child.kill().await;
                            return handshake_done;
                        }
                    }
                    if !paused_panes.is_empty() {
                        for pane in paused_panes.drain() {
                            let resume = format!("refresh-client -A {}:continue\n", pane.as_str());
                            let _ = stdin.write_all(resume.as_bytes()).await;
                        }
                        let _ = stdin.flush().await;
                    }
                }
                _ = tokio::time::sleep_until(next_digest_at), if handshake_done => {
                    next_digest_at = tokio::time::Instant::now() + self.digest_interval();
                    if !digest_in_flight && !flow_cmd_in_flight {
                        if stdin.write_all(digest_cmd.as_bytes()).await.is_err()
                            || stdin.flush().await.is_err()
                        {
                            break;
                        }
                        digest_in_flight = true;
                        digest_rows.clear();
                    }
                }
                line_res = read_control_line_prefix(&mut reader, WATCH_MAX_LINE_KEEP_BYTES) => {
                    let Ok(Some(line)) = line_res else {
                        break;
                    };
                    if let Some(ev) = parse_tmux_control_line(&line) {
                        match ev {
                            TmuxControlEvent::Begin { .. } => {
                                if digest_in_flight {
                                    digest_collecting = true;
                                    digest_rows.clear();
                                }
                            }
                            TmuxControlEvent::End { .. } => {
                                if !handshake_done {
                                    handshake_done = true;
                                    if was_connected {
                                        let _ = tx.send(HostPushSignal::Reconnect);
                                    }
                                    // Enable tmux >= 3.2 flow-control backpressure + ignore-size.
                                    if stdin
                                        .write_all(b"refresh-client -f pause-after=2,ignore-size\n")
                                        .await
                                        .is_ok()
                                    {
                                        let _ = stdin.flush().await;
                                        flow_cmd_in_flight = true;
                                    }
                                } else if flow_cmd_in_flight {
                                    flow_cmd_in_flight = false;
                                } else if digest_in_flight {
                                    digest_in_flight = false;
                                    digest_collecting = false;
                                    let diff = diff_pane_digests(&prev_digests, &digest_rows);
                                    prev_digests = digest_rows
                                        .drain(..)
                                        .map(|d| (d.pane_id.clone(), d))
                                        .collect();
                                    if diff.topology_changed {
                                        pending_full_sync = true;
                                        debounce_deadline.get_or_insert_with(|| {
                                            tokio::time::Instant::now()
                                                + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                        });
                                    } else {
                                        let mut any = false;
                                        for pane in diff.dirty_panes {
                                            if self.pane_is_interesting(&pane) {
                                                pending_dirty.insert(pane);
                                                any = true;
                                            }
                                        }
                                        if any {
                                            debounce_deadline.get_or_insert_with(|| {
                                                tokio::time::Instant::now()
                                                    + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                            });
                                        }
                                    }
                                }
                            }
                            TmuxControlEvent::Error { .. } => {
                                if flow_cmd_in_flight {
                                    flow_cmd_in_flight = false;
                                    if !sent_pause_fallback {
                                        sent_pause_fallback = true;
                                        if stdin
                                            .write_all(b"refresh-client -f pause-after=2\n")
                                            .await
                                            .is_ok()
                                        {
                                            let _ = stdin.flush().await;
                                            flow_cmd_in_flight = true;
                                        }
                                    }
                                } else if digest_in_flight {
                                    digest_in_flight = false;
                                    digest_collecting = false;
                                    digest_rows.clear();
                                }
                            }
                            TmuxControlEvent::Output(pane)
                            | TmuxControlEvent::ExtendedOutput(pane) => {
                                if self.pane_is_interesting(&pane) {
                                    pending_dirty.insert(pane);
                                    debounce_deadline.get_or_insert_with(|| {
                                        tokio::time::Instant::now()
                                            + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                    });
                                }
                            }
                            TmuxControlEvent::Pause(pane) => {
                                paused_panes.insert(pane.clone());
                                if self.pane_is_interesting(&pane) {
                                    pending_dirty.insert(pane);
                                }
                                debounce_deadline.get_or_insert_with(|| {
                                    tokio::time::Instant::now()
                                        + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                });
                            }
                            TmuxControlEvent::Continue(_) => {}
                            TmuxControlEvent::TopologyChanged { pane } => {
                                if let Some(p) = pane {
                                    if self.pane_is_interesting(&p) {
                                        pending_dirty.insert(p);
                                        debounce_deadline.get_or_insert_with(|| {
                                            tokio::time::Instant::now()
                                                + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                        });
                                    }
                                } else {
                                    pending_full_sync = true;
                                    debounce_deadline.get_or_insert_with(|| {
                                        tokio::time::Instant::now()
                                            + Duration::from_millis(WATCH_DEBOUNCE_MS)
                                    });
                                }
                            }
                            TmuxControlEvent::Exit(_) => break,
                        }
                    } else if digest_collecting {
                        if let Some(d) = parse_pane_digest_line(&line) {
                            digest_rows.push(d);
                        }
                    }
                }
            }
        }

        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_millis(200), child.wait()).await;
        if handshake_done {
            log::debug!(
                "ssh {} tmux -C control stream ended; will reconnect",
                self.host_id
            );
        }
        handshake_done
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
            resume_id: None,
            suspended_at_ms: None,
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

    fn sync_dirty_panes<'a>(
        &'a self,
        display_name: &'a str,
        dirty_panes: &'a [PaneId],
        claimed_external_ids: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<NodeSnapshot, String>> {
        Box::pin(async move {
            self.run_sync_dirty_panes_inner(display_name, dirty_panes, claimed_external_ids)
                .await
        })
    }

    fn spawn_push_watcher(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<HostPushSignal>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.push_watch {
            return None;
        }
        let watcher = SshTmuxControlWatcher {
            host_id: self.host_id.clone(),
            ssh: self.transport.clone(),
            tmux_cmd: self.tmux_cmd_prefix(),
            include_shell_panes: self.include_shell_panes,
            circuit: self.circuit.clone(),
            discovery: self.discovery.clone(),
        };
        Some(tokio::spawn(async move {
            watcher.run(tx).await;
        }))
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
            let out = self
                .exec_conduit_idempotent(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh list_sessions: {e}"))
                })?
                .into_utf8_lossy();
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

            // RTT 1: Read-only remote preflight probe over persistent `ConduitLane::Interactive`.
            let pf_script = self.build_spawn_preflight_script(req.agent, &raw_cwd, &target_session);
            let pf_out = self
                .exec_conduit_idempotent(
                    ConduitLane::Interactive,
                    &pf_script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh spawn preflight: {e}"))
                })?
                .into_utf8_lossy();
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

            // RTT 2: Atomic remote tmux spawn (NON-idempotent `exec_bytes`, NEVER auto-retried).
            let spawn_script =
                self.build_atomic_spawn_script(&target_session, &win_label, &pf.lex_cwd, &argv);
            let sp_out = self
                .conduit_pool
                .exec_bytes(
                    ConduitLane::Interactive,
                    &spawn_script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh spawn: {e}"))
                })?
                .into_utf8_lossy();
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
            self.discovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .last_emitted_watermarks
                .insert(qualified_sid.clone(), TranscriptWatermark::default());
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
                resume_id: None,
                suspended_at_ms: None,
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
                let timeout = self.transport.timeout().max(Duration::from_secs(15));
                // Small pastes (<= 64 KiB) ride the persistent interactive conduit (0 ssh fork);
                // large binary uploads (> 64 KiB, e.g. 5 MB screenshots) use one-shot `SshExec`
                // under `io_sem` so they never head-of-line block `lane_interactive`.
                let out = if bytes.len() <= SSH_STAGE_CONDUIT_MAX_BYTES {
                    let _permit = self
                        .interactive_sem
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;
                    self.exec_conduit_idempotent(
                        ConduitLane::Interactive,
                        &script,
                        Some(&bytes),
                        timeout,
                    )
                    .await
                    .map_err(|e| {
                        self.circuit
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .record_failure(id::now_ms(), e.to_string());
                        ApiError::new(ErrorCode::Unavailable, format!("ssh stage file: {e}"))
                    })?
                } else {
                    let _permit = self
                        .io_sem
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| ApiError::internal("ssh semaphore closed"))?;
                    let transport = self.transport.clone();
                    tokio::task::spawn_blocking(move || {
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
                    })?
                };
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
                    retry_paused: snap.host.retry_paused,
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
                        retry_paused: cb.is_retry_paused(),
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

    fn sync_ms(&self) -> Option<u64> {
        let v = self.last_sync_ms.load(Ordering::Acquire);
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

    fn is_retry_paused(&self) -> bool {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_retry_paused()
    }

    fn set_retry_paused(&self, paused: bool) {
        let mut cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
        if paused {
            cb.pause_retry();
        } else {
            cb.reset_for_probe();
        }
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
            let out = self
                .exec_conduit_idempotent(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh capture: {e}"))
                })?
                .into_utf8_lossy();
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

            let page_res = self
                .mirror
                .read_page_and_metrics_conduit(
                    &self.conduit_pool,
                    ConduitLane::Interactive,
                    &self.transcript_index,
                    self.host_id.as_str(),
                    &remote_home,
                    info.agent,
                    &info.remote_path,
                    info.latest_stat,
                    query,
                    info.metrics.clone(),
                    now_ms,
                )
                .await;

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
            let (effective_settle, post_check_ms) =
                fast_ssh_dispatch_timings(settle_ms, &clean_text);

            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;

            // RTT 1: Single compound gate + copy-mode cancel + pre-screen modal guard +
            // bracketed paste + fast-ack settle + C-m + fast post-capture over persistent
            // `ConduitLane::Interactive` (NEVER auto-retried on exit 255 / transport failure).
            let script = self.build_dispatch_script(
                local_pane,
                known_agent,
                effective_settle,
                post_check_ms,
            );
            let timeout = self.transport.timeout().max(Duration::from_secs(12));
            let out = self
                .conduit_pool
                .exec_bytes(
                    ConduitLane::Interactive,
                    &script,
                    Some(clean_text.as_bytes()),
                    timeout,
                )
                .await
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
                                [("0.20", "0.22"), ("0.30", "0.24")];
                            for (settle_s, observe_s) in RETRY_SCHEDULE {
                                retried = true;
                                let r_script =
                                    self.build_enter_retry_script(local_pane, settle_s, observe_s);
                                let r_out = self
                                    .conduit_pool
                                    .exec_bytes(
                                        ConduitLane::Interactive,
                                        &r_script,
                                        None,
                                        self.transport.timeout(),
                                    )
                                    .await
                                    .map_err(|e| {
                                        ApiError::new(
                                            ErrorCode::Unavailable,
                                            format!("ssh C-m retry: {e}"),
                                        )
                                    })?
                                    .into_utf8_lossy();
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
            let out = self
                .exec_conduit_idempotent(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("ssh focus: {e}")))?
                .into_utf8_lossy();
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
            let out = self
                .conduit_pool
                .exec_bytes(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh close: {e}"))
                })?
                .into_utf8_lossy();

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

    fn suspend_pane<'a>(
        &'a self,
        _remote_session: &'a SessionId,
        local_pane: &'a PaneId,
        cwd: Option<&'a str>,
        waiter_script: &'a str,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            if waiter_script.trim().is_empty() {
                return Err(ApiError::bad_request(
                    "suspend waiter script cannot be empty",
                ));
            }
            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;
            let cwd_opt = cwd
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|d| format!(" -c {}", shell_single_quote(d)))
                .unwrap_or_default();
            let script = format!(
                "{bootstrap}{tmux} respawn-pane -k -t {pane}{cwd_opt} -- /bin/sh -lc {waiter}\n",
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                tmux = self.tmux_cmd_prefix(),
                pane = shell_single_quote(local_pane.as_str()),
                waiter = shell_single_quote(waiter_script),
            );
            let out = self
                .conduit_pool
                .exec_bytes(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh suspend: {e}"))
                })?
                .into_utf8_lossy();

            if !out.success {
                let stderr = out.stderr.trim();
                let lower = stderr.to_ascii_lowercase();
                if out.code != Some(255)
                    && (lower.contains("can't find pane")
                        || lower.contains("no such pane")
                        || lower.contains("no server running"))
                {
                    return Err(ApiError::not_found(format!(
                        "remote pane {local_pane} no longer exists"
                    )));
                }
                return Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "ssh suspend pane failed: {}",
                        if stderr.is_empty() {
                            "unknown error"
                        } else {
                            stderr
                        }
                    ),
                ));
            }
            let now_hub = id::now_ms();
            self.discovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .evict_pane(local_pane, now_hub);
            Ok(local_pane.clone())
        })
    }

    fn wake_suspended_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        Box::pin(async move {
            self.send_key(remote_session, local_pane, "Enter").await?;
            let now_hub = id::now_ms();
            self.discovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .evict_pane(local_pane, now_hub);
            Ok(local_pane.clone())
        })
    }

    fn send_key<'a>(
        &'a self,
        _remote_session: &'a SessionId,
        local_pane: &'a PaneId,
        key: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            validate_tmux_pane_id(local_pane)?;
            let tmux_key =
                validate_interactive_key(key).map_err(|e| ApiError::bad_request(e.to_string()))?;

            let _permit = self
                .interactive_sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("ssh interactive semaphore closed"))?;

            let tmux_cmd = self.tmux_cmd_prefix();
            let pane_q = shell_single_quote(local_pane.as_str());
            let send_args = if tmux_key.len() == 1 {
                format!("-l {}", shell_single_quote(tmux_key))
            } else {
                shell_single_quote(tmux_key)
            };
            // Single ~15ms RTT over persistent `ConduitLane::Interactive` (NEVER auto-retried
            // on exit 255 / transport failure so a keystroke is never double-sent).
            let script = format!(
                r##"{bootstrap}
SF_TAB=$(printf '\t')
SF_US=$(printf '\037')
PANE={pane_q}
META=$({tmux} display-message -p -t "$PANE" "#{{pane_dead}}${{SF_TAB}}#{{pane_in_mode}}${{SF_TAB}}#{{pane_mode}}" </dev/null 2>&1)
if [ $? -ne 0 ]; then
  printf 'STATUS:MISSING:%s\n' "$META"
  exit 0
fi
META_US=$(printf '%s\n' "$META" | LC_ALL=C tr '\t' '\037')
IFS="$SF_US" read -r P_DEAD P_INMODE P_MODE <<__SF_KM_EOF__
$META_US
__SF_KM_EOF__
if [ "$P_DEAD" = "1" ]; then
  printf 'STATUS:DEAD\n'
  exit 0
fi
if [ "$P_INMODE" = "1" ]; then
  case "$P_MODE" in
    ""|"copy-mode"|"view-mode")
      {tmux} send-keys -t "$PANE" -X cancel </dev/null 2>/dev/null || true
      sleep 0.03 </dev/null 2>/dev/null || true
      P_INMODE=$({tmux} display-message -p -t "$PANE" '#{{pane_in_mode}}' </dev/null 2>/dev/null || printf '1')
      if [ "$P_INMODE" = "1" ]; then
        printf 'STATUS:MODE:%s\n' "$P_MODE"
        exit 0
      fi
      ;;
    *)
      printf 'STATUS:MODE:%s\n' "$P_MODE"
      exit 0
      ;;
  esac
fi
SEND_ERR=$({tmux} send-keys -t "$PANE" {send_args} </dev/null 2>&1)
if [ $? -ne 0 ]; then
  printf 'STATUS:ERR:%s\n' "$SEND_ERR"
  exit 0
fi
printf 'STATUS:OK\n'
"##,
                bootstrap = REMOTE_PATH_BOOTSTRAP,
                pane_q = pane_q,
                tmux = tmux_cmd,
                send_args = send_args,
            );

            let out = self
                .conduit_pool
                .exec_bytes(
                    ConduitLane::Interactive,
                    &script,
                    None,
                    self.transport.timeout(),
                )
                .await
                .map_err(|e| {
                    self.circuit
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .record_failure(id::now_ms(), e.to_string());
                    ApiError::new(ErrorCode::Unavailable, format!("ssh send_key: {e}"))
                })?
                .into_utf8_lossy();

            if !out.success {
                let msg = format!(
                    "ssh {} send_key failed (exit {:?}): {}",
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

            let status_line = out
                .stdout
                .lines()
                .find(|l| l.starts_with("STATUS:"))
                .unwrap_or("");
            if status_line == "STATUS:OK" {
                Ok(())
            } else if let Some(msg) = status_line.strip_prefix("STATUS:MISSING:") {
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "ssh {} pane {} not found: {}",
                        self.host_id,
                        local_pane,
                        msg.trim()
                    ),
                ))
            } else if status_line == "STATUS:DEAD" {
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("ssh {} pane {} is dead", self.host_id, local_pane),
                ))
            } else if let Some(mode) = status_line.strip_prefix("STATUS:MODE:") {
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "ssh {} pane {} is in mode {}",
                        self.host_id,
                        local_pane,
                        mode.trim()
                    ),
                ))
            } else if let Some(msg) = status_line.strip_prefix("STATUS:ERR:") {
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("ssh {} send-keys failed: {}", self.host_id, msg.trim()),
                ))
            } else {
                Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "ssh {} unexpected send_key output: {:?}",
                        self.host_id,
                        out.stdout.trim()
                    ),
                ))
            }
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
    push_watch: bool,
    endpoints: Vec<String>,
    token: Option<String>,
    active_endpoint_idx: Mutex<usize>,
    pool: Arc<PeerKeepAlivePool>,
    tunnel: Option<Arc<SshLoopbackTunnel>>,
    last_sync_ms: AtomicU64,
    circuit: Arc<Mutex<HostCircuitBreaker>>,
    staged_cache: Mutex<StagedFileCache>,
}

impl PeerDaemonNodeProvider {
    pub fn new(cfg: &NodeConfig) -> Self {
        let fallback_state = std::env::temp_dir().join("starfane-peer-state");
        Self::with_state_dir(cfg, &fallback_state)
    }

    pub fn with_state_dir(cfg: &NodeConfig, state_dir: &Path) -> Self {
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
        let tunnel = {
            let ssh_target = cfg.ssh_target.trim();
            if ssh_target.is_empty() {
                None
            } else {
                let ssh_dir = state_dir.join("ssh-cm");
                let control_path = ssh_dir.join(format!(
                    "cm-peer-{}.sock",
                    sanitize_sock_name(host_id.as_str())
                ));
                let ssh_exec = SshExec::new(ssh_target, control_path)
                    .with_ssh_bin(cfg.ssh_bin_override())
                    .with_connect_timeout_secs(cfg.effective_connect_timeout_secs())
                    .with_compression(cfg.compression)
                    .with_extra_args(cfg.ssh_args.clone());
                Some(Arc::new(SshLoopbackTunnel::new(ssh_exec)))
            }
        };
        Self {
            circuit: Arc::new(Mutex::new(HostCircuitBreaker::new(host_id.as_str()))),
            host_id,
            last_display_name: Mutex::new(display.clone()),
            default_display_name: display,
            ownership: cfg.effective_ownership(),
            push_watch: cfg.push_watch,
            endpoints,
            token,
            active_endpoint_idx: Mutex::new(0),
            pool: Arc::new(PeerKeepAlivePool::new()),
            tunnel,
            last_sync_ms: AtomicU64::new(RTT_UNSET),
            staged_cache: Mutex::new(StagedFileCache::default()),
        }
    }

    pub fn keep_alive_pool(&self) -> &Arc<PeerKeepAlivePool> {
        &self.pool
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

    /// Execute an HTTP request across optional SSH loopback tunnel + multi-candidate outbound
    /// endpoints via `PeerKeepAlivePool` with sticky failover.
    /// Transport errors and 5xx responses fail over to the next candidate; authoritative 4xx
    /// application responses (400/404/409/422) from a live peer are returned immediately.
    async fn request_json(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(String, serde_json::Value), PeerHttpError> {
        struct PeerCandidate {
            idx: Option<usize>,
            base: String,
            host_override: Option<String>,
            token: Option<String>,
        }
        let mut candidates: Vec<PeerCandidate> = Vec::new();
        let mut last_err = PeerHttpError::Transport("no peer endpoints configured".into());

        if let Some(ref tun) = self.tunnel {
            match tun.ensure_endpoint().await {
                Ok(ep) => {
                    let tok = self.token.clone().or(ep.token);
                    candidates.push(PeerCandidate {
                        idx: None,
                        base: ep.local_endpoint,
                        host_override: Some(ep.host_header),
                        token: tok,
                    });
                }
                Err(e) => {
                    last_err = e.with_prefix("ssh-tunnel");
                }
            }
        }
        for (idx, base) in self.ordered_candidates() {
            candidates.push(PeerCandidate {
                idx: Some(idx),
                base,
                host_override: None,
                token: self.token.clone(),
            });
        }
        if candidates.is_empty() {
            return Err(last_err);
        }

        let is_idempotent =
            method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
        for PeerCandidate {
            idx: idx_opt,
            base,
            host_override,
            token: tok,
        } in candidates
        {
            let url = format!("{base}{path_and_query}");
            let started = Instant::now();
            let res = self
                .pool
                .request_json_with_host(
                    method,
                    &url,
                    host_override.as_deref(),
                    tok.as_deref(),
                    body,
                )
                .await;
            match res {
                Ok(val) => {
                    let sample = started.elapsed().as_millis().min((RTT_UNSET - 1) as u128) as u64;
                    self.pool.record_rtt_sample(sample);
                    if let Some(idx) = idx_opt {
                        *self
                            .active_endpoint_idx
                            .lock()
                            .unwrap_or_else(|p| p.into_inner()) = idx;
                    }
                    return Ok((base, val));
                }
                Err(PeerHttpError::Http { status, body })
                    if matches!(status, 400 | 404 | 409 | 422) =>
                {
                    let sample = started.elapsed().as_millis().min((RTT_UNSET - 1) as u128) as u64;
                    self.pool.record_rtt_sample(sample);
                    if let Some(idx) = idx_opt {
                        *self
                            .active_endpoint_idx
                            .lock()
                            .unwrap_or_else(|p| p.into_inner()) = idx;
                    }
                    return Err(PeerHttpError::Http { status, body });
                }
                Err(e) => {
                    // If the active SSH loopback tunnel transport broke on an idempotent call,
                    // bounce the tunnel once and retry before failing over.
                    if idx_opt.is_none()
                        && is_idempotent
                        && matches!(e, PeerHttpError::Transport(_))
                    {
                        if let Some(ref tun) = self.tunnel {
                            self.pool.clear_idle();
                            if let Ok(fresh_ep) = tun.reconnect().await {
                                let fresh_tok = self.token.clone().or(fresh_ep.token);
                                let fresh_url =
                                    format!("{}{path_and_query}", fresh_ep.local_endpoint);
                                let retry_start = Instant::now();
                                if let Ok(val) = self
                                    .pool
                                    .request_json_with_host(
                                        method,
                                        &fresh_url,
                                        Some(&fresh_ep.host_header),
                                        fresh_tok.as_deref(),
                                        body,
                                    )
                                    .await
                                {
                                    let sample = retry_start
                                        .elapsed()
                                        .as_millis()
                                        .min((RTT_UNSET - 1) as u128)
                                        as u64;
                                    self.pool.record_rtt_sample(sample);
                                    return Ok((fresh_ep.local_endpoint, val));
                                }
                            }
                        }
                    }
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

        let sync_started = Instant::now();
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
        let elapsed_sync_ms = sync_started
            .elapsed()
            .as_millis()
            .min((RTT_UNSET - 1) as u128) as u64;
        self.last_sync_ms.store(elapsed_sync_ms, Ordering::Release);
        let now = id::now_ms();
        let count = qualified
            .iter()
            .filter(|s| !s.is_subagent && s.state != SessionState::Ended)
            .count();
        let live_pane_ids: HashSet<PaneId> = qualified
            .iter()
            .filter(|s| s.state != SessionState::Ended)
            .filter_map(|s| s.pane.clone())
            .collect();
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
                rtt_ms: self.pool.rtt_ms(),
                sync_ms: Some(elapsed_sync_ms),
                next_retry_ms: None,
                retry_paused: false,
            },
            sessions: qualified,
            live_pane_ids,
            events: Vec::new(),
            output_deltas: Vec::new(),
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
                resume_id: None,
                suspended_at_ms: None,
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
                    retry_paused: snap.host.retry_paused,
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
                        retry_paused: cb.is_retry_paused(),
                        error: Some(err),
                        snapshot: None,
                    })
                }
            }
        })
    }

    fn rtt_ms(&self) -> Option<u64> {
        self.pool.rtt_ms()
    }

    fn sync_ms(&self) -> Option<u64> {
        let v = self.last_sync_ms.load(Ordering::Acquire);
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

    fn is_retry_paused(&self) -> bool {
        self.circuit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_retry_paused()
    }

    fn set_retry_paused(&self, paused: bool) {
        let mut cb = self.circuit.lock().unwrap_or_else(|p| p.into_inner());
        if paused {
            cb.pause_retry();
        } else {
            cb.reset_for_probe();
        }
    }

    fn spawn_push_watcher(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<HostPushSignal>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.push_watch || (self.endpoints.is_empty() && self.tunnel.is_none()) {
            return None;
        }
        let host_id = self.host_id.clone();
        let static_endpoints = self.endpoints.clone();
        let cfg_token = self.token.clone();
        let tunnel = self.tunnel.clone();
        let circuit = self.circuit.clone();
        let pool = self.pool.clone();

        Some(tokio::spawn(async move {
            const TUNNEL_BACKOFF_SECS: &[u64] = &[1, 2, 5, 15];
            let mut tun_backoff_idx = 0usize;
            let mut last_event_id: Option<String> = None;

            while !tx.is_closed() {
                if circuit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_retry_paused()
                {
                    return;
                }
                let mut sse_endpoints: Vec<PeerSseEndpoint> = Vec::new();
                let mut using_tunnel = false;
                if let Some(ref tun) = tunnel {
                    match tun.ensure_endpoint().await {
                        Ok(ep) => {
                            tun_backoff_idx = 0;
                            using_tunnel = true;
                            let tok = cfg_token.clone().or(ep.token);
                            sse_endpoints.push(
                                PeerSseEndpoint::new(ep.local_endpoint, tok)
                                    .with_host_override(Some(ep.host_header)),
                            );
                        }
                        Err(e) => {
                            log::debug!("peer {host_id} SSH tunnel bootstrap pending: {e}");
                        }
                    }
                }
                for base in &static_endpoints {
                    sse_endpoints.push(PeerSseEndpoint::new(base.clone(), cfg_token.clone()));
                }
                if sse_endpoints.is_empty() {
                    let delay = Duration::from_secs(
                        TUNNEL_BACKOFF_SECS
                            .get(tun_backoff_idx)
                            .copied()
                            .unwrap_or(15),
                    );
                    if tun_backoff_idx + 1 < TUNNEL_BACKOFF_SECS.len() {
                        tun_backoff_idx += 1;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => continue,
                        _ = tx.closed() => return,
                    }
                }

                let subscriber = PeerSseSubscriber::with_endpoints(sse_endpoints)
                    .with_initial_last_event_id(last_event_id.clone());
                let sub_ref = subscriber.clone();
                let (sig_tx, mut sig_rx) = tokio::sync::mpsc::unbounded_channel();
                let runner = subscriber.spawn_signals(sig_tx);

                let mut consecutive_disconnects = 0u32;
                let mut need_tunnel_bounce = false;
                while let Some(sig) = tokio::select! {
                    s = sig_rx.recv() => s,
                    _ = tx.closed() => None,
                } {
                    match sig {
                        PeerSseSignal::Connected { reconnected, .. } => {
                            consecutive_disconnects = 0;
                            circuit
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .record_success();
                            let push = if reconnected {
                                HostPushSignal::Reconnect
                            } else {
                                HostPushSignal::FullSync
                            };
                            if tx.send(push).is_err() {
                                break;
                            }
                        }
                        PeerSseSignal::Frame(frame) => {
                            if let Some(ref fid) = frame.id {
                                last_event_id = Some(fid.clone());
                            }
                            if frame.is_ping() {
                                continue;
                            }
                            if frame.is_resync() {
                                if tx.send(HostPushSignal::FullSync).is_err() {
                                    break;
                                }
                                continue;
                            }
                            match frame.parse_daemon_event() {
                                Ok(ev) => {
                                    // Filter out federated (non-local) sessions on the peer to prevent loops.
                                    if let DaemonEvent::Session { ref session, .. } = ev {
                                        if !session.is_local_host() {
                                            continue;
                                        }
                                    }
                                    if tx.send(HostPushSignal::PeerEvent(ev)).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    log::debug!(
                                        "peer {host_id} ignored unparsed SSE frame event={:?}: {e}",
                                        frame.event
                                    );
                                }
                            }
                        }
                        PeerSseSignal::Disconnected { error, .. } => {
                            consecutive_disconnects = consecutive_disconnects.saturating_add(1);
                            log::debug!(
                                "peer {host_id} SSE disconnected (#{consecutive_disconnects}): {error}"
                            );
                            if using_tunnel && consecutive_disconnects >= 2 {
                                need_tunnel_bounce = true;
                                break;
                            }
                        }
                    }
                }

                if let Some(lid) = sub_ref.last_event_id() {
                    last_event_id = Some(lid);
                }
                runner.abort();
                if tx.is_closed() {
                    return;
                }
                if need_tunnel_bounce {
                    if let Some(ref tun) = tunnel {
                        pool.clear_idle();
                        let _ = tun.reconnect().await;
                    }
                }
            }
        }))
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

    fn send_key<'a>(
        &'a self,
        remote_session: &'a SessionId,
        _local_pane: &'a PaneId,
        key: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/key");
        let key_owned = key.to_string();
        Box::pin(async move {
            validate_interactive_key(&key_owned)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            let body = serde_json::json!({ "key": key_owned });
            let _ = self
                .request_json("POST", &path, Some(&body))
                .await
                .map_err(|e| e.into_api_error("peer send_key failed"))?;
            Ok(())
        })
    }

    fn suspend_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
        _cwd: Option<&'a str>,
        _waiter_script: &'a str,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/suspend");
        let fallback_pane = local_pane.clone();
        Box::pin(async move {
            let (_, env) = self
                .request_json("POST", &path, Some(&serde_json::json!({})))
                .await
                .map_err(|e| e.into_api_error("peer suspend failed"))?;
            let pane_str = env
                .pointer("/data/pane")
                .or_else(|| env.pointer("/data/session/pane"))
                .or_else(|| env.get("pane"))
                .and_then(Value::as_str)
                .map(|s| starfane_tmux::split_host_prefix(s).1.to_string());
            Ok(pane_str.map(PaneId::new).unwrap_or(fallback_pane))
        })
    }

    fn wake_suspended_pane<'a>(
        &'a self,
        remote_session: &'a SessionId,
        local_pane: &'a PaneId,
    ) -> BoxFuture<'a, Result<PaneId, ApiError>> {
        let encoded_sid = pct_encode(remote_session.as_str());
        let path = format!("/api/sessions/{encoded_sid}/resume");
        let fallback_pane = local_pane.clone();
        Box::pin(async move {
            let (_, env) = self
                .request_json("POST", &path, Some(&serde_json::json!({})))
                .await
                .map_err(|e| e.into_api_error("peer resume failed"))?;
            let pane_str = env
                .pointer("/data/pane")
                .or_else(|| env.pointer("/data/session/pane"))
                .or_else(|| env.get("pane"))
                .and_then(Value::as_str)
                .map(|s| starfane_tmux::split_host_prefix(s).1.to_string());
            Ok(pane_str.map(PaneId::new).unwrap_or(fallback_pane))
        })
    }
}

// ---------------------------------------------------------------------------
// Utility helpers (chunked test parser, Base64, socket-name & URL encoding)
// ---------------------------------------------------------------------------

#[cfg(test)]
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
    use tokio::io::AsyncReadExt;
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
        assert!(!cb.is_retry_paused());
    }

    #[test]
    fn circuit_breaker_pause_retry_stops_backoff_and_resume_unpauses() {
        let host = HostId::new("gpu-paused");
        let mut cb = HostCircuitBreaker::new(host.as_str());
        for _ in 0..3 {
            cb.record_failure(200_000, "timeout");
        }
        assert_eq!(cb.status(), HostStatus::Offline);
        assert!(cb.next_retry_ms().is_some());
        assert!(!cb.is_retry_paused());

        // Pausing clears scheduled next_retry_ms and fast-fails with a paused message.
        cb.pause_retry();
        assert!(cb.is_retry_paused());
        assert_eq!(cb.next_retry_ms(), None);
        let err = cb.fast_fail_if_offline(&host, 201_000).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("paused"));

        // Further failures recorded while paused must NOT re-schedule a countdown.
        let (st, _) = cb.record_failure(202_000, "still unreachable");
        assert_eq!(st, HostStatus::Offline);
        assert!(cb.is_retry_paused());
        assert_eq!(cb.next_retry_ms(), None);

        // User-triggered reconnect unpauses cleanly.
        cb.reset_for_probe();
        assert!(!cb.is_retry_paused());
        assert_eq!(cb.next_retry_ms(), None);
        assert!(cb.fast_fail_if_offline(&host, 203_000).is_ok());
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
    fn remote_codex_multi_pane_same_cwd_resume_title_match_blank_isolation_and_new_clear() {
        let host = HostId::new("mac-xiling");
        let mut ds = SshDiscoveryState::default();
        let remote_now_s = 1_795_620_000u64;
        let hub_now_ms = remote_now_s * 1_000;

        // Reproduces live `mac-xiling`:
        // - `%20` (blank) & `%21` (resumed Nov 10 thread) share `/Users/xiling/Work/chatgpt-register`
        // - `%33` (resumed Oct 24 thread, NOT in `tr_stats`!) & `%34` (resumed Nov 8 thread) & `%37` (blank)
        //   all share `/Users/xiling/Work/Einsia`
        // - `%31` is Claude stuck on project MCP confirmation prompt (`NeedsPermission`)
        let p20 = pane_row("%20", 1001, "codex", "/Users/xiling/Work/chatgpt-register");
        let p21 = pane_row("%21", 1002, "codex", "/Users/xiling/Work/chatgpt-register");
        let p33 = pane_row("%33", 1003, "codex", "/Users/xiling/Work/Einsia");
        let p34 = pane_row("%34", 1004, "codex", "/Users/xiling/Work/Einsia");
        let p37 = pane_row("%37", 1005, "codex", "/Users/xiling/Work/Einsia");
        let p31 = pane_row(
            "%31",
            1006,
            "claude",
            "/Users/xiling/Work/openkernelchallenge",
        );

        let us = '\x1f';
        let raw_t1 = format!(
            "{SEC_META}\nREMOTE_NOW_S={remote_now_s}\nREMOTE_HOME=/Users/xiling\n\
\n{SEC_PANES}\n{p20}\n{p21}\n{p33}\n{p34}\n{p37}\n{p31}\n\
\n{SEC_PS}\n\
 1001 1 00:10 codex\n\
 1002 1 00:10 codex\n\
 1003 1 00:10 codex\n\
 1004 1 00:10 codex\n\
 1005 1 00:10 codex\n\
 1006 1 00:10 claude --allow-dangerously-skip-permissions\n\
\n{SEC_TAILS}\n\
__SF_TAIL_BEGIN__ %20\n› Ask Codex to do anything\n  GPT-6.1-Sol low · ~/Work/chatgpt-register\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %20\n\
__SF_TAIL_BEGIN__ %21\n› Ask Codex to do anything\n  GPT-6.1-Sol high · ~/Work/chatgpt-register · 新增 zjc API 并按本地样式打包\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %21\n\
__SF_TAIL_BEGIN__ %33\n› Ask Codex to do anything\n  GPT-6.1-Sol medium · ~/Work/Einsia · 测试该出口访问网站是否需要代理\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %33\n\
__SF_TAIL_BEGIN__ %34\n› Ask Codex to do anything\n  GPT-6.1-Sol high · ~/Work/Einsia · 只读查看 vm2api+magpie 项目及连接方式\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %34\n\
__SF_TAIL_BEGIN__ %37\n› Ask Codex to do anything\n  GPT-6.1-Sol low · ~/Work/Einsia\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %37\n\
__SF_TAIL_BEGIN__ %31\n  New MCP server found in this project: ida-pro\n  Use this and all future MCP servers in this project\n  ❯ Continue without using this MCP server\n  Enter to confirm · Esc to cancel\n__SF_TAIL_END__ %31\n\
\n{SEC_RESOLVE}\n\
CODEX_DB_SIG 1:1:1|0:0:0\n\
CODEX_DB_BEGIN\n\
01a1c31c{us}/Users/xiling/.codex/sessions/2026/11/10/r-21.jsonl{us}/Users/xiling/Work/chatgpt-register{us}新增 zjc API 并按本地样式打包{us}1794300000{us}1795550000\n\
01a1bb5d{us}/Users/xiling/.codex/sessions/2026/11/08/r-34.jsonl{us}/Users/xiling/Work/Einsia{us}只读查看 vm2api+magpie 项目及连接方式{us}1794100000{us}1795200000\n\
01a16c82{us}/Users/xiling/.codex/sessions/2026/10/24/r-33.jsonl{us}/Users/xiling/Work/Einsia{us}测试该出口访问网站是否需要代理{us}1792800000{us}1794900000\n\
CODEX_DB_END\n\
\n{SEC_TR_STATS}\n\
101 2970000 1795550000 /Users/xiling/.codex/sessions/2026/11/10/r-21.jsonl\n\
102 20100000 1795200000 /Users/xiling/.codex/sessions/2026/11/08/r-34.jsonl\n"
        );
        // Note: `%33` (`r-33.jsonl`) is deliberately OMITTED from `SEC_TR_STATS` to prove tick-1
        // mirror scheduling works even for older resumed threads below top-30 SQLite rows!
        let parsed1 = parse_ssh_compound_v2_output(&raw_t1, hub_now_ms).unwrap();
        let res1 = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "mac-xiling",
            parsed1,
            false,
            &HashSet::new(),
            hub_now_ms,
        );
        let find_s = |res: &SshReconcileResult, pane: &str| -> SessionRecord {
            res.sessions
                .iter()
                .find(|s| s.session_id.as_str() == format!("mac-xiling:tmux-{pane}"))
                .unwrap()
                .clone()
        };

        let s20 = find_s(&res1, "%20");
        assert_eq!(s20.transcript_path, None);
        assert_eq!(s20.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s20.effort.as_deref(), Some("low"));

        let s21 = find_s(&res1, "%21");
        assert_eq!(
            s21.transcript_path.as_deref(),
            Some("/Users/xiling/.codex/sessions/2026/11/10/r-21.jsonl")
        );
        assert_eq!(s21.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s21.effort.as_deref(), Some("high"));

        let s33 = find_s(&res1, "%33");
        assert_eq!(
            s33.transcript_path.as_deref(),
            Some("/Users/xiling/.codex/sessions/2026/10/24/r-33.jsonl")
        );
        assert_eq!(s33.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s33.effort.as_deref(), Some("medium"));
        // Tick-1 mirror scheduled even though `r-33.jsonl` was absent from `SEC_TR_STATS`!
        assert!(
            res1.need_mirror
                .iter()
                .any(|(p, _, path, st)| p.as_str() == "%33"
                    && path.ends_with("r-33.jsonl")
                    && st.is_none()),
            "expected tick-1 mirror entry with stat=None for %33: {:?}",
            res1.need_mirror
        );

        let s34 = find_s(&res1, "%34");
        assert_eq!(
            s34.transcript_path.as_deref(),
            Some("/Users/xiling/.codex/sessions/2026/11/08/r-34.jsonl")
        );
        assert_eq!(s34.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s34.effort.as_deref(), Some("high"));

        let s37 = find_s(&res1, "%37");
        assert_eq!(s37.transcript_path, None);
        assert_eq!(s37.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s37.effort.as_deref(), Some("low"));

        let s31 = find_s(&res1, "%31");
        assert_eq!(s31.state, SessionState::NeedsPermission);

        // Tick 2: user runs `/new` inside `%21` (status bar becomes blank `GPT-6.1-Sol low · ~/Work/chatgpt-register`)
        // -> stale title binding is dropped cleanly, while `%33` & `%34` stay authoritatively bound!
        let raw_t2 = format!(
            "{SEC_META}\nREMOTE_NOW_S={}\nREMOTE_HOME=/Users/xiling\n\
\n{SEC_PANES}\n{p20}\n{p21}\n{p33}\n{p34}\n{p37}\n{p31}\n\
\n{SEC_PS}\n\
 1001 1 00:20 codex\n\
 1002 1 00:20 codex\n\
 1003 1 00:20 codex\n\
 1004 1 00:20 codex\n\
 1005 1 00:20 codex\n\
 1006 1 00:20 claude --allow-dangerously-skip-permissions\n\
\n{SEC_TAILS}\n\
__SF_TAIL_BEGIN__ %21\n› Ask Codex to do anything\n  GPT-6.1-Sol low · ~/Work/chatgpt-register\n  ← for agents · ? for shortcuts\n__SF_TAIL_END__ %21\n\
\n{SEC_RESOLVE}\n\
CODEX_DB_SIG 1:1:1|0:0:0\n\
\n{SEC_TR_STATS}\n\
102 20100000 1795200000 /Users/xiling/.codex/sessions/2026/11/08/r-34.jsonl\n",
            remote_now_s + 10
        );
        let parsed2 = parse_ssh_compound_v2_output(&raw_t2, hub_now_ms + 10_000).unwrap();
        let res2 = reconcile_ssh_discovery_v2(
            &mut ds,
            &host,
            "mac-xiling",
            parsed2,
            false,
            &HashSet::new(),
            hub_now_ms + 10_000,
        );
        let s21_after = find_s(&res2, "%21");
        assert_eq!(
            s21_after.transcript_path, None,
            "switching %21 to blank /new prompt must clear stale title binding"
        );
        assert_eq!(s21_after.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(s21_after.effort.as_deref(), Some("low"));
        assert_eq!(
            find_s(&res2, "%34").transcript_path.as_deref(),
            Some("/Users/xiling/.codex/sessions/2026/11/08/r-34.jsonl")
        );
        assert_eq!(
            find_s(&res2, "%33").transcript_path.as_deref(),
            Some("/Users/xiling/.codex/sessions/2026/10/24/r-33.jsonl")
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

    #[test]
    fn compound_v3_wrap_safe_awk_batched_stat_shell_skip_and_inline_hot_delta() {
        // 1. Test `parse_ssh_compound_v3_output` with inline `__SF_HOT_DELTA__` payload.
        let delta_text = concat!(
            r#"{"type":"assistant","timestamp":"2026-11-25T11:00:00Z","message":{"id":"m9","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"inline v3 streaming reply"}]}}"#,
            "\n"
        );
        let delta_b64 = encode_base64(delta_text.as_bytes());
        let p_cl = pane_row("%1", 50000, "zsh", "/home/u/repo");
        let raw_v3 = format!(
            "{SEC_META}\nREMOTE_NOW_S=1760000100\nREMOTE_HOME=/home/u\n\
\n{SEC_PANES}\n{p_cl}\n\
\n{SEC_PS}\n50000 1 01:00 -zsh\n120 50000 00:10 claude --model opus\n\
\n{SEC_TAILS}\n__SF_TAIL_BEGIN__ %1\nWorking...\n__SF_TAIL_END__ %1\n\
\n{SEC_RESOLVE}\n\
\n{SEC_TR_STATS}\n101 {} 1760000100 /home/u/.claude/projects/-home-u-repo/s1.jsonl\n\
\n{SEC_HOT_DELTAS}\n__SF_HOT_DELTA__ 101 {} 1760000100 500 {} /home/u/.claude/projects/-home-u-repo/s1.jsonl\n{delta_b64}\n",
            500 + delta_text.len(),
            500 + delta_text.len(),
            delta_text.len(),
        );
        let parsed = parse_ssh_compound_v3_output(&raw_v3, 1_760_000_100_000).unwrap();
        let hd = parsed
            .hot_deltas
            .get("/home/u/.claude/projects/-home-u-repo/s1.jsonl")
            .expect("inline hot delta parsed");
        assert_eq!(hd.stat.ino, 101);
        assert_eq!(hd.base_offset, 500);
        assert_eq!(hd.bytes, delta_text.as_bytes());

        // 2. Execute the actual generated `compound_discover_script_v3` with synthetic `tmux` and
        //    `ps` shims to verify:
        //    - Wrap-safe fixed-point `awk` catches child PID `120 < parent PID 50000` (and grandchild `42 < 120`)
        //    - Unrelated background processes (PID 90001) are filtered out of `__SF_SEC_PS__`
        //    - Pure-shell pane `%2` (`zsh`) is NOT captured when `include_shell_panes == false`
        //    - Already-resolved AGY PID `3003` (within birth skew tolerance) is skipped from `AGY_PIDS`
        //    - Single batched OS-native `stat` emits stats for files including spaces in path
        //    - Inline hot delta emits exact base64 bytes for `0 < size - offset <= 65536`
        let root = std::env::temp_dir().join(format!("sf-v3-script-{}", id::new_id("T")));
        let rhome = root.join("home");
        let shim_bin = root.join("bin");
        std::fs::create_dir_all(&shim_bin).unwrap();
        let tr_dir = rhome.join(".claude/projects/-home-u-my repo");
        std::fs::create_dir_all(&tr_dir).unwrap();
        let tr_file = tr_dir.join("hot-s1.jsonl");
        let prefix = "line0-existing\n";
        let appended = "line1-hot-delta\n";
        std::fs::write(&tr_file, format!("{prefix}{appended}")).unwrap();

        #[cfg(unix)]
        let tr_ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&tr_file).unwrap().ino()
        };
        #[cfg(not(unix))]
        let tr_ino = 0u64;

        let cap_log = root.join("captures.log");
        let pane1 = pane_row("%1", 50000, "zsh", "/home/u/repo");
        let pane2 = pane_row("%2", 60000, "zsh", "/home/u/shell");
        let fake_tmux = shim_bin.join("fake-tmux");
        std::fs::write(
            &fake_tmux,
            format!(
                r#"#!/bin/sh
if [ "$1" = "list-panes" ]; then
  printf '%s\n%s\n' '{pane1}' '{pane2}'
  exit 0
fi
if [ "$1" = "capture-pane" ]; then
  printf '%s\n' "$*" >> "{cap_log}"
  printf 'captured-tail-for-%s\n' "$3"
  exit 0
fi
exit 0
"#,
                cap_log = cap_log.display(),
            ),
        )
        .unwrap();
        let fake_ps = shim_bin.join("ps");
        std::fs::write(
            &fake_ps,
            r#"#!/bin/sh
cat <<'EOF'
   42   120 00:05 claude --model opus
  120 50000 00:10 bash -c claude
50000     1 01:00 -zsh
60000     1 02:00 -zsh
 3003     1 00:30 /usr/local/bin/agy --model gemini-4-neon-high
 3004     1 00:02 /usr/local/bin/agy --model gemini-4-neon-high
90001     1 10:00 /usr/sbin/unrelated-system-daemon --noise
EOF
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&fake_ps, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = NodeConfig {
            id: "v3-box".into(),
            kind: NodeKindConfig::Ssh,
            enabled: true,
            ssh_target: "demo@v3-box".into(),
            tmux_bin: fake_tmux.to_string_lossy().into_owned(),
            include_shell_panes: false,
            ..NodeConfig::default()
        };
        let provider = SshMuxNodeProvider::new(&cfg, &root.join("ssh-cm"));
        let now_s = id::now_ms() / 1_000;
        let script = provider.compound_discover_script_v3(
            &[],
            None,
            None,
            &[tr_file.to_string_lossy().into_owned()],
            &[(3003, now_s.saturating_sub(30))],
            &[],
            &[(
                tr_file.to_string_lossy().into_owned(),
                tr_ino,
                prefix.len() as u64,
            )],
        );

        let cur_path = std::env::var("PATH").unwrap_or_default();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", &rhome)
            .env("PATH", format!("{}:{cur_path}", shim_bin.display()))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "v3 script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let res = parse_ssh_compound_v3_output(&stdout, id::now_ms()).unwrap();

        // Wrap-safe fixed-point awk included wrapped child 120 and grandchild 42, excluded noise 90001!
        assert!(
            res.live_remote_pids.contains(&42),
            "wrapped grandchild PID 42 (< 120 < 50000) must be included"
        );
        assert!(res.live_remote_pids.contains(&120));
        assert!(res.live_remote_pids.contains(&50000));
        assert!(
            !res.live_remote_pids.contains(&90001),
            "unrelated system daemon 90001 must be filtered out"
        );

        // Agent pane %1 WAS captured; pure-shell pane %2 was SKIPPED (`include_shell_panes == false`)!
        assert!(res.captured_tails.contains_key(&PaneId::new("%1")));
        assert!(
            !res.captured_tails.contains_key(&PaneId::new("%2")),
            "pure-shell pane %2 must be skipped when include_shell_panes == false"
        );

        // Batched stat handled path with spaces!
        let tr_key = tr_file.to_string_lossy().into_owned();
        let tr_st = res
            .tr_stats
            .get(&tr_key)
            .expect("batched stat must include path with spaces");
        assert_eq!(tr_st.size, (prefix.len() + appended.len()) as u64);

        // Inline hot delta returned ONLY the appended suffix bytes!
        let delta = res
            .hot_deltas
            .get(&tr_key)
            .expect("inline hot delta emitted for active cursor");
        assert_eq!(delta.base_offset, prefix.len() as u64);
        assert_eq!(delta.bytes, appended.as_bytes());

        // Staggered active verification + zero Idle expiry cliff in `cached_tail_fingerprints`.
        let mut ds = SshDiscoveryState::default();
        ds.pane_agent_pid.insert(PaneId::new("%1"), 42);
        ds.pane_agent_pid.insert(PaneId::new("%4"), 43);
        ds.tail_cache.insert(
            PaneId::new("%1"),
            SshTailCacheEntry {
                history_size: 100,
                cursor_x: 2,
                cursor_y: 10,
                in_mode: false,
                current_command: "claude".into(),
                agent_pid: Some(42),
                tail: "idle".into(),
            },
        );
        ds.tail_cache.insert(
            PaneId::new("%4"),
            SshTailCacheEntry {
                history_size: 200,
                cursor_x: 5,
                cursor_y: 12,
                in_mode: false,
                current_command: "codex".into(),
                agent_pid: Some(43),
                tail: "working".into(),
            },
        );
        ds.pane_watch.insert(
            PaneId::new("%1"),
            SshPaneWatch {
                last_hash: 1,
                stable_ticks: 10,
                state: SessionState::Idle,
                state_since_ms: 0,
                last_event_ms: 0,
                last_error: None,
            },
        );
        ds.pane_watch.insert(
            PaneId::new("%4"),
            SshPaneWatch {
                last_hash: 2,
                stable_ticks: 0,
                state: SessionState::Working,
                state_since_ms: 0,
                last_event_ms: 0,
                last_error: None,
            },
        );
        // Idle pane %1 stays cached across ALL 8 ticks (no synchronized 3-tick cliff!).
        // Working pane %4 skips cache only when `(4 + tick) % 4 == 0` (ticks 0 and 4).
        for tick in 0..8u64 {
            let fps = ds.cached_tail_fingerprints(tick);
            assert!(
                fps.iter().any(|f| f.starts_with("%1|")),
                "idle pane %1 must stay cached on tick {tick}"
            );
            let p4_cached = fps.iter().any(|f| f.starts_with("%4|"));
            assert_eq!(
                p4_cached,
                (4 + tick) % 4 != 0,
                "working pane %4 staggered verification mismatch on tick {tick}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tmux_control_watcher_parses_output_pause_and_multi_session_digest() {
        assert_eq!(
            parse_tmux_control_line("%output %12 hello\\015\\012"),
            Some(TmuxControlEvent::Output(PaneId::new("%12")))
        );
        assert_eq!(
            parse_tmux_control_line("%extended-output %7 0 : some output"),
            Some(TmuxControlEvent::ExtendedOutput(PaneId::new("%7")))
        );
        assert_eq!(
            parse_tmux_control_line("%pause %9"),
            Some(TmuxControlEvent::Pause(PaneId::new("%9")))
        );
        assert_eq!(
            parse_tmux_control_line("%continue %9"),
            Some(TmuxControlEvent::Continue(PaneId::new("%9")))
        );
        assert_eq!(
            parse_tmux_control_line("%pane-mode-changed %3"),
            Some(TmuxControlEvent::TopologyChanged {
                pane: Some(PaneId::new("%3"))
            })
        );
        assert_eq!(
            parse_tmux_control_line("%sessions-changed"),
            Some(TmuxControlEvent::TopologyChanged { pane: None })
        );
        assert_eq!(
            parse_tmux_control_line("%window-add @4"),
            Some(TmuxControlEvent::TopologyChanged { pane: None })
        );
        assert_eq!(
            parse_tmux_control_line("%begin 1760000000 42 1"),
            Some(TmuxControlEvent::Begin { cmd_num: 42 })
        );
        assert_eq!(
            parse_tmux_control_line("%end 1760000000 42 1"),
            Some(TmuxControlEvent::End { cmd_num: 42 })
        );
        assert_eq!(
            parse_tmux_control_line("%error 1760000000 43 1"),
            Some(TmuxControlEvent::Error { cmd_num: 43 })
        );
        assert_eq!(
            parse_tmux_control_line("%exit client detached"),
            Some(TmuxControlEvent::Exit("client detached".into()))
        );
        assert_eq!(parse_tmux_control_line("regular digest line"), None);

        // Multi-session digest parser + diff logic.
        let d1 = parse_pane_digest_line("%1|500|4|20|0|0|claude").unwrap();
        let d2 = parse_pane_digest_line("%2|120|0|10|0|0|codex").unwrap();
        let mut prev = HashMap::new();
        prev.insert(d1.pane_id.clone(), d1.clone());
        prev.insert(d2.pane_id.clone(), d2.clone());

        // Cursor/history change on %2 -> targeted DirtyPanes([%2]), topology_changed == false.
        let mut d2_moved = d2.clone();
        d2_moved.history_size = 505;
        let diff1 = diff_pane_digests(&prev, &[d1.clone(), d2_moved]);
        assert!(!diff1.topology_changed);
        assert_eq!(diff1.dirty_panes, vec![PaneId::new("%2")]);

        // Foreground command change on %1 -> topology_changed == true (triggers FullSync).
        let mut d1_cmd = d1.clone();
        d1_cmd.current_command = "zsh".into();
        let diff2 = diff_pane_digests(&prev, &[d1_cmd, d2.clone()]);
        assert!(diff2.topology_changed);

        // New pane %3 added -> topology_changed == true.
        let d3 = parse_pane_digest_line("%3|0|0|0|0|0|agy").unwrap();
        let diff3 = diff_pane_digests(&prev, &[d1, d2, d3]);
        assert!(diff3.topology_changed);
    }

    #[test]
    fn fast_ssh_dispatch_timings_short_single_line_vs_multiline() {
        // Short single-line prompt uses fast 30ms settle + 50ms post-check!
        assert_eq!(fast_ssh_dispatch_timings(250, "run cargo test"), (30, 50));
        assert_eq!(fast_ssh_dispatch_timings(20, "hi"), (20, 50));
        assert_eq!(fast_ssh_dispatch_timings(0, "hi"), (0, 50));

        // Multi-line or >160-char prompt retains adaptive paste-burst settling + 120ms post-check.
        let (ml_settle, ml_post) = fast_ssh_dispatch_timings(250, "line 1\nline 2");
        assert!(ml_settle >= 400, "expected >=400ms settle, got {ml_settle}");
        assert_eq!(ml_post, 120);

        let long_line = "a".repeat(220);
        let (long_settle, long_post) = fast_ssh_dispatch_timings(250, &long_line);
        assert!(long_settle >= 250);
        assert_eq!(long_post, 120);
    }

    #[tokio::test]
    async fn peer_provider_push_watcher_streams_sse_events_and_filters_federated_loops() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let local_out = DaemonEvent::Output {
                seq: 11,
                session_id: SessionId::new("tmux-%5"),
                start_seq: Some(2),
                total_messages: Some(3),
                appended_messages: vec![
                    serde_json::json!({"role": "assistant", "text": "fast push"}),
                ],
                live_tail: None,
                metrics: None,
                dirty: false,
            };
            let foreign_sess = DaemonEvent::Session {
                seq: 12,
                session: SessionRecord {
                    session_id: SessionId::new("other-host:tmux-%9"),
                    agent: AgentKind::Claude,
                    pane: Some(PaneId::new("other-host:%9")),
                    pane_resolution: PaneResolution::Env,
                    cwd: None,
                    git_root: None,
                    model: None,
                    effort: None,
                    state: SessionState::Idle,
                    state_since_ms: 0,
                    last_event_id: None,
                    last_event_ms: 0,
                    last_message: None,
                    last_error: None,
                    parent_session_id: None,
                    is_subagent: false,
                    subagent_count: 0,
                    active_subagents: 0,
                    stage: None,
                    unread_inbox: 0,
                    confidence: Confidence::Exact,
                    tmux_target: None,
                    tmux_label: None,
                    discovered_only: false,
                    transcript_path: None,
                    metrics: None,
                    host_id: Some(HostId::new("other-host")),
                    host_display_name: None,
                    mux_kind: None,
                    resume_id: None,
                    suspended_at_ms: None,
                },
            };
            let frames = format!(
                "{}{}",
                local_out.to_sse_frame().unwrap(),
                foreign_sess.to_sse_frame().unwrap()
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n{frames}"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let cfg = NodeConfig {
            id: "peer-push".into(),
            kind: NodeKindConfig::Peer,
            enabled: true,
            push_watch: true,
            endpoints: vec![format!("http://{addr}")],
            ..NodeConfig::default()
        };
        let provider = PeerDaemonNodeProvider::new(&cfg);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = provider
            .spawn_push_watcher(tx)
            .expect("peer push_watch=true must spawn watcher");

        // 1. First signal: initial FullSync on SSE connect.
        let sig1 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(sig1, HostPushSignal::FullSync), "{sig1:?}");

        // 2. Second signal: local DaemonEvent::Output pushed directly!
        let sig2 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match sig2 {
            HostPushSignal::PeerEvent(DaemonEvent::Output {
                session_id,
                total_messages,
                ..
            }) => {
                assert_eq!(session_id.as_str(), "tmux-%5");
                assert_eq!(total_messages, Some(3));
            }
            other => panic!("expected PeerEvent(Output), got {other:?}"),
        }

        // 3. Foreign-host session event must NOT be forwarded (loop prevention!).
        let no_foreign = tokio::time::timeout(Duration::from_millis(120), rx.recv()).await;
        assert!(
            no_foreign.is_err(),
            "federated foreign-host session must be filtered out, got {no_foreign:?}"
        );

        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        server.await.unwrap();
    }

    #[test]
    fn ssh_build_transcript_output_delta_handles_watermark_mutations_shrink_and_caps() {
        let dir = std::env::temp_dir().join(format!("starfane-ssh-wm-{}", id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();
        let tr_cache = Arc::new(Mutex::new(TranscriptIndexCache::default()));
        let provider = SshMuxNodeProvider::with_shared_caches(
            &NodeConfig {
                id: "ssh-wm".into(),
                kind: NodeKindConfig::Ssh,
                enabled: true,
                ssh_target: "dev@example".into(),
                ..NodeConfig::default()
            },
            &dir,
            Arc::new(RemoteTranscriptMirror::new(dir.clone())),
            tr_cache,
        );
        let mut ds = SshDiscoveryState::default();
        let qsid = SessionId::new("ssh-wm:tmux-%1");

        // 1. AGY trailing pending_tools -> final PLANNER_RESPONSE (N -> N) + late hydration
        let agy_dir = dir.join("agy");
        std::fs::create_dir_all(&agy_dir).unwrap();
        let agy_compact = agy_dir.join("transcript.jsonl");
        let agy_full = agy_dir.join("transcript_full.jsonl");
        let u1 = r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-26T00:00:00Z","content":"test"}"#;
        let p_tools = r#"{"step_index":1,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:01Z","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls"}}]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n")).unwrap();

        // Baseline seed with allow_emit = false -> None
        assert!(provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid,
                AgentKind::Agy,
                &agy_compact,
                false,
                None,
                None,
                1_000,
            )
            .is_none());

        let p_final_trunc = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"Truncated... <truncated 50 chars>","truncated_fields":["content"]}"#;
        std::fs::write(&agy_compact, format!("{u1}\n{p_tools}\n{p_final_trunc}\n")).unwrap();
        let ev1 = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid,
                AgentKind::Agy,
                &agy_compact,
                true,
                None,
                None,
                2_000,
            )
            .expect("expected SSH Output on pending_tools -> final");
        match ev1 {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(2));
                assert!(!dirty);
                assert_eq!(
                    appended_messages[0]["text"],
                    "Truncated... <truncated 50 chars>"
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        // Late transcript_full.jsonl hydration emits overlap tail(1).
        let p_final_full = r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:02Z","content":"Hydrated final output"}"#;
        std::fs::write(&agy_full, format!("{p_final_full}\n")).unwrap();
        let ev2 = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid,
                AgentKind::Agy,
                &agy_compact,
                true,
                None,
                None,
                3_000,
            )
            .expect("expected SSH Output on late hydration");
        match ev2 {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(2));
                assert!(!dirty);
                assert_eq!(appended_messages[0]["text"], "Hydrated final output");
            }
            other => panic!("unexpected {other:?}"),
        }

        // 2. Growth where previous last turn had pending_tools overlaps by 1 (start_seq = old_total - 1).
        let agy_growth = dir.join("agy-growth.jsonl");
        let p_tools2 = r#"{"step_index":3,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:03Z","tool_calls":[{"name":"read_file","args":{"Path":"x"}}]}"#;
        std::fs::write(&agy_growth, format!("{u1}\n{p_tools2}\n")).unwrap();
        let qsid_g = SessionId::new("ssh-wm:tmux-%3");
        assert!(provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_g,
                AgentKind::Agy,
                &agy_growth,
                false,
                None,
                None,
                3_500,
            )
            .is_none());
        let u2 = r#"{"step_index":4,"type":"USER_INPUT","created_at":"2026-11-26T00:00:04Z","content":"next"}"#;
        let p_final2 = r#"{"step_index":5,"type":"PLANNER_RESPONSE","created_at":"2026-11-26T00:00:05Z","content":"done"}"#;
        std::fs::write(&agy_growth, format!("{u1}\n{p_tools2}\n{u2}\n{p_final2}\n")).unwrap();
        let ev_g = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_g,
                AgentKind::Agy,
                &agy_growth,
                true,
                None,
                None,
                3_600,
            )
            .expect("expected SSH growth overlap");
        match ev_g {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(3));
                assert!(!dirty);
                assert_eq!(appended_messages.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }

        // 3. Claude same-msg_id append (2 -> 2), >32 KiB dirty, and shrink (3 -> 1) -> dirty=true.
        let c_path = dir.join("claude.jsonl");
        let cu = r#"{"type":"user","timestamp":"2026-11-26T00:00:00Z","message":{"content":"go"}}"#;
        let ca1 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:01Z","message":{"id":"cm1","stop_reason":null,"content":[{"type":"text","text":"step 1"}]}}"#;
        std::fs::write(&c_path, format!("{cu}\n{ca1}\n")).unwrap();
        let qsid_c = SessionId::new("ssh-wm:tmux-%2");
        assert!(provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_c,
                AgentKind::Claude,
                &c_path,
                false,
                None,
                None,
                4_000,
            )
            .is_none());

        let ca2 = r#"{"type":"assistant","timestamp":"2026-11-26T00:00:02Z","message":{"id":"cm1","stop_reason":"end_turn","content":[{"type":"text","text":"step 2"}]}}"#;
        std::fs::write(&c_path, format!("{cu}\n{ca1}\n{ca2}\n")).unwrap();
        let ev_c = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_c,
                AgentKind::Claude,
                &c_path,
                true,
                None,
                None,
                5_000,
            )
            .expect("expected SSH Output on same-msg_id append");
        match ev_c {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(2));
                assert!(!dirty);
                assert_eq!(appended_messages[0]["text"], "step 1\n\nstep 2");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Oversized > 32 KiB growth -> dirty = true
        let big = "b".repeat(40 * 1024);
        let ca_big = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-11-26T00:00:03Z",
            "message": {
                "id": "cm2",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": big}]
            }
        })
        .to_string();
        std::fs::write(&c_path, format!("{cu}\n{ca1}\n{ca2}\n{ca_big}\n")).unwrap();
        let ev_big = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_c,
                AgentKind::Claude,
                &c_path,
                true,
                None,
                None,
                6_000,
            )
            .expect("expected dirty delta on >32KiB turn");
        match ev_big {
            DaemonEvent::Output {
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(total_messages, Some(3));
                assert!(appended_messages.is_empty());
                assert!(dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        // Shrink 3 -> 1 -> dirty = true
        std::fs::write(&c_path, format!("{cu}\n")).unwrap();
        let ev_shrink = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_c,
                AgentKind::Claude,
                &c_path,
                true,
                None,
                None,
                7_000,
            )
            .expect("expected dirty delta on shrink");
        match ev_shrink {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, None);
                assert_eq!(total_messages, Some(1));
                assert!(appended_messages.is_empty());
                assert!(dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        // > SSH_INLINE_OUTPUT_MAX_TURNS (32) growth -> dirty = true
        let mut many_lines = format!("{cu}\n");
        for idx in 0..35 {
            many_lines.push_str(&format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-11-26T00:01:{:02}Z\",\"message\":{{\"id\":\"many_{idx}\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"turn {idx}\"}}]}}}} \n",
                idx % 60
            ));
        }
        std::fs::write(&c_path, many_lines).unwrap();
        let ev_many = provider
            .build_transcript_output_delta(
                &mut ds,
                &qsid_c,
                AgentKind::Claude,
                &c_path,
                true,
                None,
                None,
                8_000,
            )
            .expect("expected dirty delta on >32 turn jump");
        match ev_many {
            DaemonEvent::Output {
                start_seq,
                total_messages,
                appended_messages,
                dirty,
                ..
            } => {
                assert_eq!(start_seq, Some(1));
                assert_eq!(total_messages, Some(36));
                assert!(appended_messages.is_empty());
                assert!(dirty);
            }
            other => panic!("unexpected {other:?}"),
        }

        // Verify dirty panes script stats hot.path even when hot.ino == 0 (first-time dirty mirror).
        let script = provider.build_dirty_panes_script(&[DirtyPaneTarget {
            pane: PaneId::new("%1"),
            agent: AgentKind::Claude,
            hot: Some(HotCursorSpec {
                path: "/home/dev/.claude/projects/p/s.jsonl".into(),
                ino: 0,
                offset: 0,
            }),
        }]);
        assert!(script.contains("sf_hot_delta '/home/dev/.claude/projects/p/s.jsonl' 0 0"));
        assert!(script.contains("printf '__SF_DIRTY_STAT__ %s %s %s %s\\n'"));
        assert!(script.contains(
            "if [ \"$_hino\" -ne 0 ] && [ \"$_cino\" -ne \"$_hino\" ]; then return 0; fi"
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
