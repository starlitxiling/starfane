//! Hub-side incremental byte-offset transcript mirror for zero-install SSH hosts (`kind = "ssh"`).
//!
//! Mirrors remote Claude Code, Codex CLI, and Antigravity (AGY) JSONL transcripts onto local Hub
//! disk under `<state_dir>/remote-cache/<safe_host_id>/` using zero remote dependencies
//! (POSIX `/bin/sh` + `stat` + `tail -c` + `head -c` + optional `gzip -1 -c`), enabling full reuse
//! of `TranscriptIndex`, `TranscriptIndexCache`, `TelemetryFold`, cursor pagination, and selective
//! AGY `step_index` hydration with 0 SSH calls when unchanged.

pub use crate::core::metrics::{
    agy_remote_transcript_path, claude_remote_transcript_path, match_remote_agy_conversation,
    match_remote_codex_rollouts, match_remote_codex_threads, parse_agy_child_rows_delimited,
    parse_agy_child_rows_json, parse_agy_conv_rows_delimited, parse_agy_conv_rows_json,
    parse_claude_default_effort_json, parse_claude_pid_session_json,
    parse_codex_default_effort_toml, parse_codex_session_meta_line,
    parse_codex_thread_rows_delimited, parse_codex_thread_rows_json, parse_proc_fd_agy_presence,
    parse_status_line_metrics, remote_agy_children_of, verify_remote_claude_pid_session,
    AgyChildRow, AgyConvRow, CodexSessionMeta, CodexThreadRow, RemoteAgyChild,
    RemoteClaudeSessionFile, RemoteCodexRolloutCandidate,
};
use crate::core::transcript::{
    fast_extract_step_index, TranscriptIndexCache, TranscriptQuery, TranscriptResponse,
    MAX_TRANSCRIPT_LIMIT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use starfane_proto::{AgentKind, SessionMetrics};
use starfane_tmux::{ssh_universal_quote, ConduitLane, SshConduitPool, SshExec, TmuxError};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Cold-start tail window for giant remote transcripts (2 MiB).
pub const INITIAL_TAIL_WINDOW: u64 = 2 * 1024 * 1024;
/// Maximum raw bytes transferred in a single incremental delta chunk (8 MiB).
pub const MAX_DELTA_CHUNK: u64 = 8 * 1024 * 1024;
/// Maximum total local mirror size per remote transcript (64 MiB).
pub const MAX_REMOTE_MIRROR_BYTES: u64 = 64 * 1024 * 1024;
/// Minimum raw delta size before piping through `gzip -1 -c` (16 KiB).
pub const GZIP_COMPRESS_THRESHOLD: u64 = 16 * 1024;
/// Strict zip-bomb output ceiling when decompressing `GZ1` deltas on the Hub (16 MiB).
pub const MAX_DECOMPRESSED_CHUNK_BYTES: usize = 16 * 1024 * 1024;
/// Maximum AGY `step_index` lines selectively hydrated in one remote round-trip.
pub const MAX_AGY_HYDRATE_STEPS: usize = 20;
/// Maximum bytes accepted per hydrated AGY step line (512 KiB).
pub const MAX_AGY_HYDRATE_STEP_BYTES: usize = 512 * 1024;
/// Maximum byte length of an inline hot transcript delta carried inside `compound_discover_script_v3` (64 KiB).
pub const MAX_INLINE_DELTA_BYTES: usize = 64 * 1024;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Portable `stat` identity for a remote transcript file (`sf_stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFileStat {
    pub ino: u64,
    pub size: u64,
    pub mtime_s: i64,
}

/// Result of syncing or backfilling a remote transcript mirror on local Hub disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorSyncResult {
    pub local_path: PathBuf,
    pub base_offset: u64,
    pub synced_offset: u64,
    pub changed: bool,
}

/// Combined result of serving a paginated remote transcript page plus telemetry.
#[derive(Debug, Clone)]
pub struct RemotePageOutcome {
    pub response: TranscriptResponse,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub sync: MirrorSyncResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MirrorMeta {
    remote_path: String,
    remote_ino: u64,
    remote_size: u64,
    remote_mtime_s: i64,
    base_offset: u64,
    synced_offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferMode {
    Raw,
    Gz1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FetchHeader {
    stat: RemoteFileStat,
    offset: u64,
    raw_len: u64,
    mode: TransferMode,
    reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeFetchOutcome {
    Header(FetchHeader),
    Rotated(RemoteFileStat),
    Truncated(RemoteFileStat),
}

/// Parse a portable `sf_stat` output line `<ino> <size> <mtime_s> <path>` into `(RemoteFileStat, path)`.
pub fn parse_sf_stat_line(line: &str) -> Option<(RemoteFileStat, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(4, char::is_whitespace);
    let ino = parts.next()?.parse::<u64>().ok()?;
    let size_raw = parts.next()?.parse::<i64>().ok()?;
    if size_raw < 0 {
        return None;
    }
    let mtime_s = parts.next()?.parse::<i64>().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((
        RemoteFileStat {
            ino,
            size: size_raw as u64,
            mtime_s,
        },
        path,
    ))
}

/// Tuple returned by [`RemoteTranscriptMirror::sync_telemetry`].
pub type RemoteTelemetryOutcome = (
    Option<SessionMetrics>,
    Option<String>,
    Option<String>,
    MirrorSyncResult,
);

type KeyLockMap = HashMap<(String, String), Arc<Mutex<()>>>;
type AsyncKeyLockMap = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

/// Hub-side incremental mirror manager for remote SSH host transcripts.
///
/// Cheaply `Clone`able and `Send + Sync`; internally serializes concurrent pulls per
/// `(host_id, remote_path)` so parallel UI/sync requests never race on the same cache file.
#[derive(Debug, Clone)]
pub struct RemoteTranscriptMirror {
    cache_root: PathBuf,
    key_locks: Arc<Mutex<KeyLockMap>>,
    async_key_locks: Arc<Mutex<AsyncKeyLockMap>>,
}

impl RemoteTranscriptMirror {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            cache_root: state_dir.join("remote-cache"),
            key_locks: Arc::new(Mutex::new(HashMap::new())),
            async_key_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Strictly validate that `remote_path` is a safe regular transcript JSONL path under
    /// `remote_home` for Claude Code, Codex CLI, or Antigravity (AGY).
    ///
    /// Rejects `\0`, `\n`, `\r`, any `..` or `.` path segment, non-absolute paths, and any path
    /// outside:
    /// - `<remote_home>/.claude/projects/.../*.jsonl`
    /// - `<remote_home>/.codex/sessions/.../*.jsonl`
    /// - `<remote_home>/.gemini/antigravity-cli/brain/<conv_id>/[.system_generated/logs/]transcript{,_full}.jsonl`
    pub fn validate_remote_transcript_path(
        remote_home: &str,
        remote_path: &str,
    ) -> Result<(), String> {
        if remote_home.is_empty() || remote_path.is_empty() {
            return Err("remote home and transcript path must be non-empty".to_string());
        }
        if remote_home
            .bytes()
            .any(|b| b == 0 || b == b'\n' || b == b'\r')
        {
            return Err("remote home contains forbidden control characters".to_string());
        }
        if remote_path
            .bytes()
            .any(|b| b == 0 || b == b'\n' || b == b'\r')
        {
            return Err("remote transcript path contains forbidden control characters".to_string());
        }
        if !remote_home.starts_with('/') {
            return Err("remote home must be an absolute path".to_string());
        }
        if !remote_path.starts_with('/') {
            return Err("remote transcript path must be an absolute path".to_string());
        }
        if has_traversal_segment(remote_home) {
            return Err("remote home contains traversal segment".to_string());
        }
        if has_traversal_segment(remote_path) {
            return Err("remote transcript path contains traversal segment".to_string());
        }

        let home_norm = remote_home.trim_end_matches('/');
        if home_norm.is_empty() {
            return Err("remote home must not be filesystem root '/'".to_string());
        }
        let Some(rel) = remote_path
            .strip_prefix(home_norm)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return Err(format!(
                "remote transcript path {remote_path:?} is outside remote home {remote_home:?}"
            ));
        };

        // 1. Claude: `<remote_home>/.claude/projects/.../*.jsonl`
        if let Some(rest) = rel.strip_prefix(".claude/projects/") {
            if is_valid_relative_jsonl(rest) {
                return Ok(());
            }
            return Err(format!(
                "invalid Claude remote transcript path: {remote_path:?}"
            ));
        }

        // 2. Codex: `<remote_home>/.codex/sessions/.../*.jsonl`
        if let Some(rest) = rel.strip_prefix(".codex/sessions/") {
            if is_valid_relative_jsonl(rest) {
                return Ok(());
            }
            return Err(format!(
                "invalid Codex remote transcript path: {remote_path:?}"
            ));
        }

        // 3. AGY: `<remote_home>/.gemini/antigravity-cli/brain/<conv_id>/[.system_generated/logs/]transcript{,_full}.jsonl`
        if let Some(rest) = rel.strip_prefix(".gemini/antigravity-cli/brain/") {
            let segs: Vec<&str> = rest.split('/').collect();
            let valid = match segs.as_slice() {
                [conv_id, file] => is_safe_segment(conv_id) && is_agy_transcript_filename(file),
                [conv_id, ".system_generated", "logs", file] => {
                    is_safe_segment(conv_id) && is_agy_transcript_filename(file)
                }
                _ => false,
            };
            if valid {
                return Ok(());
            }
            return Err(format!(
                "invalid AGY remote transcript path: {remote_path:?}"
            ));
        }

        Err(format!(
            "remote transcript path {remote_path:?} is outside allowed agent directories"
        ))
    }

    /// Compute the deterministic local cache path for `(host_id, agent, remote_path)`.
    pub fn local_mirror_path(&self, host_id: &str, agent: AgentKind, remote_path: &str) -> PathBuf {
        self.cache_paths(host_id, agent, remote_path).local_path
    }

    /// Return the currently mirrored `(remote_ino, synced_offset)` for `(host_id, agent, remote_path)`
    /// when a valid local mirror exists on disk (`None` if unmirrored or inconsistent).
    pub fn cached_sync_cursor(
        &self,
        host_id: &str,
        agent: AgentKind,
        remote_path: &str,
    ) -> Option<(u64, u64)> {
        let paths = self.cache_paths(host_id, agent, remote_path);
        let meta = load_valid_meta(&paths.local_path, &paths.meta_path, remote_path)?;
        Some((meta.remote_ino, meta.synced_offset))
    }

    /// Return the latest verified remote `(ino, size, mtime_s)` from local mirror metadata,
    /// used to seed `last_stat` on tick 1 when a newly resolved transcript wasn't in `latest_tr_stats`.
    pub fn cached_remote_stat(
        &self,
        host_id: &str,
        agent: AgentKind,
        remote_path: &str,
    ) -> Option<RemoteFileStat> {
        let paths = self.cache_paths(host_id, agent, remote_path);
        let meta = load_valid_meta(&paths.local_path, &paths.meta_path, remote_path)?;
        (meta.remote_ino != 0).then_some(RemoteFileStat {
            ino: meta.remote_ino,
            size: meta.remote_size,
            mtime_s: meta.remote_mtime_s,
        })
    }

    /// Ingest an inline hot transcript delta (`<= 64 KiB`) piggybacked inside a discovery or
    /// dirty-pane response with **0 extra SSH round-trips**.
    ///
    /// Supports both:
    /// - Incremental append onto an existing valid mirror matching `(stat.ino, base_offset_expected)`
    /// - Fresh small cold-seed (`base_offset_expected == 0 && delta_bytes.len() as u64 == stat.size`)
    ///
    /// Returns `Err` on any inode/offset/size mismatch so the caller can fall back cleanly to
    /// [`RemoteTranscriptMirror::ensure_fresh_conduit`].
    #[allow(clippy::too_many_arguments)]
    pub fn ingest_inline_delta(
        &self,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        stat: RemoteFileStat,
        base_offset_expected: u64,
        delta_bytes: &[u8],
    ) -> Result<MirrorSyncResult, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_path)
            .map_err(TmuxError::InvalidRequest)?;
        if delta_bytes.len() > MAX_INLINE_DELTA_BYTES {
            return Err(TmuxError::Parse(format!(
                "inline delta ({} bytes) exceeds {} byte cap",
                delta_bytes.len(),
                MAX_INLINE_DELTA_BYTES
            )));
        }
        let delta_len = delta_bytes.len() as u64;
        let new_synced = base_offset_expected
            .checked_add(delta_len)
            .ok_or_else(|| TmuxError::Parse("inline delta offset overflow".into()))?;
        if new_synced > stat.size {
            return Err(TmuxError::Parse(format!(
                "inline delta end ({new_synced}) exceeds remote file size ({})",
                stat.size
            )));
        }

        let key_lock = self.lock_for(host_id, remote_path);
        let _guard = lock_mutex(&key_lock);

        let paths = self.cache_paths(host_id, agent, remote_path);
        let existing_meta = load_valid_meta(&paths.local_path, &paths.meta_path, remote_path);

        if let Some(meta) = existing_meta {
            if meta.remote_ino != 0
                && meta.remote_ino == stat.ino
                && meta.synced_offset == base_offset_expected
            {
                let cur_local_len = meta.synced_offset.saturating_sub(meta.base_offset);
                if cur_local_len.saturating_add(delta_len) > MAX_REMOTE_MIRROR_BYTES {
                    return Err(TmuxError::Parse(
                        "inline delta would exceed MAX_REMOTE_MIRROR_BYTES".into(),
                    ));
                }
                let actual_local_len = fs::metadata(&paths.local_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if actual_local_len != cur_local_len {
                    return Err(TmuxError::Parse(format!(
                        "local mirror size mismatch: disk={actual_local_len}, meta={cur_local_len}"
                    )));
                }
                if delta_bytes.is_empty() {
                    let mut next_meta = meta;
                    if next_meta.remote_mtime_s != stat.mtime_s
                        || next_meta.remote_size != stat.size
                    {
                        next_meta.remote_size = stat.size;
                        next_meta.remote_mtime_s = stat.mtime_s;
                        save_meta(&paths.meta_path, &next_meta)?;
                    }
                    return Ok(MirrorSyncResult {
                        local_path: paths.local_path,
                        base_offset: next_meta.base_offset,
                        synced_offset: next_meta.synced_offset,
                        changed: false,
                    });
                }

                let mut file =
                    open_private_append(&paths.local_path).map_err(|e| TmuxError::Io {
                        what: format!("append inline mirror delta {}", paths.local_path.display()),
                        source: e,
                    })?;
                file.write_all(delta_bytes).map_err(|e| TmuxError::Io {
                    what: format!("write inline mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;
                file.flush().map_err(|e| TmuxError::Io {
                    what: format!("flush inline mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;

                let next_meta = MirrorMeta {
                    remote_path: remote_path.to_string(),
                    remote_ino: stat.ino,
                    remote_size: stat.size.max(new_synced),
                    remote_mtime_s: stat.mtime_s,
                    base_offset: meta.base_offset,
                    synced_offset: new_synced,
                };
                save_meta(&paths.meta_path, &next_meta)?;
                return Ok(MirrorSyncResult {
                    local_path: paths.local_path,
                    base_offset: next_meta.base_offset,
                    synced_offset: next_meta.synced_offset,
                    changed: true,
                });
            }
            if base_offset_expected != 0 {
                return Err(TmuxError::Parse(format!(
                    "inline delta cursor mismatch: local ino={} off={}, incoming ino={} base={base_offset_expected}",
                    meta.remote_ino, meta.synced_offset, stat.ino
                )));
            }
        }

        // Small fresh/rotated cold-seed (`base_offset_expected == 0`, complete file inline).
        if base_offset_expected == 0
            && new_synced == stat.size
            && stat.size <= MAX_REMOTE_MIRROR_BYTES
        {
            ensure_private_dir(&paths.dir).map_err(|e| TmuxError::Io {
                what: format!("create remote cache dir {}", paths.dir.display()),
                source: e,
            })?;
            if agent == AgentKind::Agy {
                let stale_full = paths.local_path.with_file_name("transcript_full.jsonl");
                if stale_full.exists() {
                    let _ = fs::remove_file(&stale_full);
                }
            }
            atomic_write_private(&paths.local_path, delta_bytes).map_err(|e| TmuxError::Io {
                what: format!("write inline mirror file {}", paths.local_path.display()),
                source: e,
            })?;
            let next_meta = MirrorMeta {
                remote_path: remote_path.to_string(),
                remote_ino: stat.ino,
                remote_size: stat.size,
                remote_mtime_s: stat.mtime_s,
                base_offset: 0,
                synced_offset: new_synced,
            };
            save_meta(&paths.meta_path, &next_meta)?;
            return Ok(MirrorSyncResult {
                local_path: paths.local_path,
                base_offset: 0,
                synced_offset: new_synced,
                changed: true,
            });
        }

        Err(TmuxError::Parse(format!(
            "cannot ingest inline delta at base={base_offset_expected} len={delta_len} without matching mirror"
        )))
    }

    /// Ensure the local Hub mirror of `remote_path` is up to date.
    ///
    /// - Fast-path (**0 SSH calls / 0 bytes**): when `latest_stat` matches `(meta.remote_ino, meta.synced_offset)`,
    ///   the local mirror exists with expected byte length, and `(!need_full_history || meta.base_offset == 0)`.
    /// - Cold-start (`remote_size > 2 MiB` and `!need_full_history`): fetches the trailing 2 MiB,
    ///   discards partial leading bytes up to the first `\n`, and records `base_offset`.
    /// - Incremental append: transfers only `[synced_offset .. remote_size)` in chunks `<= 8 MiB`,
    ///   compressing with `gzip -1 -c` when `raw_len >= 16 KiB` and verifying decompressed size +
    ///   16 MiB zip-bomb cap on the Hub.
    /// - Rotation / truncation (`remote_ino != meta.remote_ino || remote_size < meta.synced_offset`):
    ///   resets and re-seeds cleanly.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_fresh(
        &self,
        ssh: &SshExec,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        need_full_history: bool,
    ) -> Result<MirrorSyncResult, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_path)
            .map_err(TmuxError::InvalidRequest)?;

        let key_lock = self.lock_for(host_id, remote_path);
        let _guard = lock_mutex(&key_lock);

        self.ensure_fresh_locked(
            ssh,
            host_id,
            agent,
            remote_path,
            latest_stat,
            need_full_history,
        )
    }

    /// Backfill the unmirrored prefix `[target_start .. base_offset)` (capped at `MAX_REMOTE_MIRROR_BYTES`)
    /// for a cold-tail-seeded mirror and atomically stitch `prefix + tail` on local disk.
    pub fn backfill_prefix(
        &self,
        ssh: &SshExec,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
    ) -> Result<MirrorSyncResult, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_path)
            .map_err(TmuxError::InvalidRequest)?;

        let key_lock = self.lock_for(host_id, remote_path);
        let _guard = lock_mutex(&key_lock);

        self.backfill_prefix_locked(ssh, host_id, agent, remote_path)
    }

    /// Selectively fetch up to `MAX_AGY_HYDRATE_STEPS` (20) `step_index` lines (each capped at
    /// `MAX_AGY_HYDRATE_STEP_BYTES` = 512 KiB) from the remote sibling `transcript_full.jsonl`
    /// and append newly fetched lines to local `agy-<safe_conv_id>/transcript_full.jsonl`.
    ///
    /// Returns the number of newly appended step lines (`0` if all requested steps are already
    /// locally cached or if remote `transcript_full.jsonl` does not exist).
    pub fn hydrate_remote_agy_steps(
        &self,
        ssh: &SshExec,
        host_id: &str,
        remote_home: &str,
        remote_compact_path: &str,
        step_indices: &[usize],
    ) -> Result<usize, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_compact_path)
            .map_err(TmuxError::InvalidRequest)?;
        if step_indices.is_empty() {
            return Ok(0);
        }
        let Some(remote_full_path) = remote_sibling_transcript_full(remote_compact_path) else {
            return Ok(0);
        };
        Self::validate_remote_transcript_path(remote_home, &remote_full_path)
            .map_err(TmuxError::InvalidRequest)?;

        let key_lock = self.lock_for(host_id, remote_compact_path);
        let _guard = lock_mutex(&key_lock);

        let paths = self.cache_paths(host_id, AgentKind::Agy, remote_compact_path);
        let local_full_path = paths.local_path.with_file_name("transcript_full.jsonl");

        // Determine which requested steps are not yet present in local sparse `transcript_full.jsonl`.
        let already_present = scan_local_step_indices(&local_full_path);
        let mut needed: Vec<usize> = Vec::new();
        for &idx in step_indices {
            if !already_present.contains(&(idx as u64)) && !needed.contains(&idx) {
                needed.push(idx);
                if needed.len() >= MAX_AGY_HYDRATE_STEPS {
                    break;
                }
            }
        }
        if needed.is_empty() {
            return Ok(0);
        }
        needed.sort_unstable();

        let script = build_agy_hydrate_script(&remote_full_path, &needed);
        let out = ssh.run_idempotent_shell_bytes(&script, None, ssh.timeout())?;
        let payload = parse_hydrate_response(&out)?;
        let Some(raw_lines) = payload else {
            // Remote `transcript_full.jsonl` absent: non-fatal, return 0 hydrated steps.
            return Ok(0);
        };

        let wanted_set: HashSet<u64> = needed.iter().map(|&n| n as u64).collect();
        let mut seen_new: HashSet<u64> = HashSet::new();
        let mut verified_lines: Vec<Vec<u8>> = Vec::new();

        for raw_line in raw_lines.split(|&b| b == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() || line.len() > MAX_AGY_HYDRATE_STEP_BYTES {
                continue;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                continue;
            };
            let trimmed = text.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(step_idx) = obj.get("step_index").and_then(Value::as_u64) else {
                continue;
            };
            if wanted_set.contains(&step_idx) && seen_new.insert(step_idx) {
                verified_lines.push(trimmed.as_bytes().to_vec());
            }
        }

        if verified_lines.is_empty() {
            return Ok(0);
        }

        ensure_private_dir(&paths.dir).map_err(|e| TmuxError::Io {
            what: format!("create remote cache dir {}", paths.dir.display()),
            source: e,
        })?;
        let mut file = open_private_append(&local_full_path).map_err(|e| TmuxError::Io {
            what: format!("open sparse full transcript {}", local_full_path.display()),
            source: e,
        })?;
        for line in &verified_lines {
            file.write_all(line).map_err(|e| TmuxError::Io {
                what: format!("append hydrated step to {}", local_full_path.display()),
                source: e,
            })?;
            file.write_all(b"\n").map_err(|e| TmuxError::Io {
                what: format!("append hydrated newline to {}", local_full_path.display()),
                source: e,
            })?;
        }
        file.flush().map_err(|e| TmuxError::Io {
            what: format!("flush {}", local_full_path.display()),
            source: e,
        })?;

        Ok(verified_lines.len())
    }

    /// Background telemetry helper for `SshMuxNodeProvider::sync()`:
    /// ensures the remote transcript mirror is fresh (0 SSH calls if `latest_stat` unchanged)
    /// and updates `TranscriptIndexCache` without holding the cache mutex during SSH I/O.
    #[allow(clippy::too_many_arguments)]
    pub fn sync_telemetry(
        &self,
        ssh: &SshExec,
        cache: &Mutex<TranscriptIndexCache>,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        now_ms: u64,
    ) -> Result<RemoteTelemetryOutcome, TmuxError> {
        let sync = self.ensure_fresh(
            ssh,
            host_id,
            remote_home,
            agent,
            remote_path,
            latest_stat,
            false,
        )?;
        let (metrics, model, effort) = {
            let mut guard = lock_mutex(cache);
            guard.sync_and_telemetry(agent, &sync.local_path, now_ms)
        };
        Ok((metrics, model, effort, sync))
    }

    /// End-to-end paginated transcript + telemetry read for `SshMuxNodeProvider::fetch_transcript`:
    /// 1. Syncs the remote transcript via `ensure_fresh` (0 SSH calls on unchanged `latest_stat`).
    /// 2. If `query.all` or `query.before` reaches the beginning of the mirrored tail window while
    ///    `base_offset > 0`, invokes `backfill_prefix` once and shifts `query.before` by the
    ///    newly prepended turn count so cursor pagination is seamless.
    /// 3. For `AgentKind::Agy`, detects unhydrated `step_index` turns in the requested page,
    ///    selectively hydrates them (`<= 20` steps, `<= 512 KiB`/step) into sibling
    ///    `transcript_full.jsonl`, and re-hydrates the page.
    /// 4. Sets `has_more = resp.has_more || sync.base_offset > 0` and `transcript_path = Some(remote_path)`.
    ///
    /// Never holds `cache`'s mutex across SSH network I/O.
    #[allow(clippy::too_many_arguments)]
    pub fn read_page_and_metrics(
        &self,
        ssh: &SshExec,
        cache: &Mutex<TranscriptIndexCache>,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        query: TranscriptQuery,
        session_metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> Result<RemotePageOutcome, TmuxError> {
        let mut sync = self.ensure_fresh(
            ssh,
            host_id,
            remote_home,
            agent,
            remote_path,
            latest_stat,
            query.all,
        )?;

        let effective_limit = if query.all {
            MAX_TRANSCRIPT_LIMIT
        } else {
            query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT)
        };
        let mut effective_query = query;

        // If `before` reaches start of the mirrored window (or `all == true`) while `base_offset > 0`,
        // invoke `backfill_prefix` once and adjust the `before` cursor by newly prepended turns.
        let reaches_start = query.all || query.before.is_some_and(|b| b <= effective_limit);
        if sync.base_offset > 0 && reaches_start {
            let old_total = {
                let mut guard = lock_mutex(cache);
                guard
                    .total_messages_for_path(agent, &sync.local_path)
                    .unwrap_or(0)
            };
            let backfilled = self.backfill_prefix(ssh, host_id, remote_home, agent, remote_path)?;
            if backfilled.changed {
                let new_total = {
                    let mut guard = lock_mutex(cache);
                    guard.invalidate_path(&backfilled.local_path);
                    guard
                        .total_messages_for_path(agent, &backfilled.local_path)
                        .unwrap_or(old_total)
                };
                if let Some(b) = effective_query.before {
                    let prefix_delta = new_total.saturating_sub(old_total);
                    effective_query.before = Some(b.saturating_add(prefix_delta));
                }
            }
            sync = backfilled;
        }

        // For AGY, detect unhydrated steps in the target page and selectively fetch them from remote.
        if agent == AgentKind::Agy {
            let missing = {
                let mut guard = lock_mutex(cache);
                guard.missing_agy_hydration_steps(
                    agent,
                    &sync.local_path,
                    effective_query,
                    MAX_AGY_HYDRATE_STEPS,
                )
            };
            if !missing.is_empty() {
                match self.hydrate_remote_agy_steps(
                    ssh,
                    host_id,
                    remote_home,
                    remote_path,
                    &missing,
                ) {
                    Ok(hydrated_count) if hydrated_count > 0 => {
                        let mut guard = lock_mutex(cache);
                        guard.invalidate_pages(&sync.local_path);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::debug!(
                            "remote AGY selective hydration skipped for {remote_path}: {e}"
                        );
                    }
                }
            }
        }

        let (mut resp, model, effort) = {
            let mut guard = lock_mutex(cache);
            let resp = guard
                .get_page_or_sync(
                    agent,
                    &sync.local_path,
                    effective_query,
                    session_metrics.clone(),
                    now_ms,
                )
                .unwrap_or_else(|| TranscriptResponse {
                    source: "none".to_string(),
                    transcript_path: None,
                    messages: Vec::new(),
                    has_more: false,
                    total_messages: 0,
                    start_seq: 0,
                    next_before: None,
                    metrics: session_metrics,
                });
            let (_, model, effort) = guard.sync_and_telemetry(agent, &sync.local_path, now_ms);
            (resp, model, effort)
        };

        if resp.source == "transcript" {
            resp.transcript_path = Some(remote_path.to_string());
            resp.has_more = resp.has_more || sync.base_offset > 0;
            if resp.has_more && resp.next_before.is_none() {
                resp.next_before = Some(resp.start_seq);
            }
        }

        Ok(RemotePageOutcome {
            response: resp,
            model,
            effort,
            sync,
        })
    }

    // -----------------------------------------------------------------------
    // Async persistent-conduit (`SshConduitPool`) mirror APIs (0 per-call ssh fork)
    // -----------------------------------------------------------------------

    /// Persistent-conduit equivalent of [`RemoteTranscriptMirror::ensure_fresh`].
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_fresh_conduit(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        need_full_history: bool,
    ) -> Result<MirrorSyncResult, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_path)
            .map_err(TmuxError::InvalidRequest)?;

        let async_lock = self.async_lock_for(host_id, remote_path);
        let _async_guard = async_lock.lock().await;

        self.ensure_fresh_conduit_locked(
            pool,
            lane,
            host_id,
            agent,
            remote_path,
            latest_stat,
            need_full_history,
        )
        .await
    }

    /// Persistent-conduit equivalent of [`RemoteTranscriptMirror::backfill_prefix`].
    pub async fn backfill_prefix_conduit(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
    ) -> Result<MirrorSyncResult, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_path)
            .map_err(TmuxError::InvalidRequest)?;

        let async_lock = self.async_lock_for(host_id, remote_path);
        let _async_guard = async_lock.lock().await;

        self.backfill_prefix_conduit_locked(pool, lane, host_id, agent, remote_path)
            .await
    }

    /// Persistent-conduit equivalent of [`RemoteTranscriptMirror::hydrate_remote_agy_steps`].
    pub async fn hydrate_remote_agy_steps_conduit(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        host_id: &str,
        remote_home: &str,
        remote_compact_path: &str,
        step_indices: &[usize],
    ) -> Result<usize, TmuxError> {
        Self::validate_remote_transcript_path(remote_home, remote_compact_path)
            .map_err(TmuxError::InvalidRequest)?;
        if step_indices.is_empty() {
            return Ok(0);
        }
        let Some(remote_full_path) = remote_sibling_transcript_full(remote_compact_path) else {
            return Ok(0);
        };
        Self::validate_remote_transcript_path(remote_home, &remote_full_path)
            .map_err(TmuxError::InvalidRequest)?;

        let async_lock = self.async_lock_for(host_id, remote_compact_path);
        let _async_guard = async_lock.lock().await;

        let paths = self.cache_paths(host_id, AgentKind::Agy, remote_compact_path);
        let local_full_path = paths.local_path.with_file_name("transcript_full.jsonl");

        let already_present = scan_local_step_indices(&local_full_path);
        let mut needed: Vec<usize> = Vec::new();
        for &idx in step_indices {
            if !already_present.contains(&(idx as u64)) && !needed.contains(&idx) {
                needed.push(idx);
                if needed.len() >= MAX_AGY_HYDRATE_STEPS {
                    break;
                }
            }
        }
        if needed.is_empty() {
            return Ok(0);
        }
        needed.sort_unstable();

        let script = build_agy_hydrate_script(&remote_full_path, &needed);
        let out = pool
            .exec_bytes(lane, &script, None, pool.ssh().timeout())
            .await?;
        let payload = parse_hydrate_response_async(out).await?;
        let Some(raw_lines) = payload else {
            return Ok(0);
        };

        let key_lock = self.lock_for(host_id, remote_compact_path);
        let _sync_guard = lock_mutex(&key_lock);

        let wanted_set: HashSet<u64> = needed.iter().map(|&n| n as u64).collect();
        let mut seen_new: HashSet<u64> = HashSet::new();
        let mut verified_lines: Vec<Vec<u8>> = Vec::new();

        for raw_line in raw_lines.split(|&b| b == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() || line.len() > MAX_AGY_HYDRATE_STEP_BYTES {
                continue;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                continue;
            };
            let trimmed = text.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(step_idx) = obj.get("step_index").and_then(Value::as_u64) else {
                continue;
            };
            if wanted_set.contains(&step_idx) && seen_new.insert(step_idx) {
                verified_lines.push(trimmed.as_bytes().to_vec());
            }
        }

        if verified_lines.is_empty() {
            return Ok(0);
        }

        ensure_private_dir(&paths.dir).map_err(|e| TmuxError::Io {
            what: format!("create remote cache dir {}", paths.dir.display()),
            source: e,
        })?;
        let mut file = open_private_append(&local_full_path).map_err(|e| TmuxError::Io {
            what: format!("open sparse full transcript {}", local_full_path.display()),
            source: e,
        })?;
        for line in &verified_lines {
            file.write_all(line).map_err(|e| TmuxError::Io {
                what: format!("append hydrated step to {}", local_full_path.display()),
                source: e,
            })?;
            file.write_all(b"\n").map_err(|e| TmuxError::Io {
                what: format!("append hydrated newline to {}", local_full_path.display()),
                source: e,
            })?;
        }
        file.flush().map_err(|e| TmuxError::Io {
            what: format!("flush {}", local_full_path.display()),
            source: e,
        })?;

        Ok(verified_lines.len())
    }

    /// Persistent-conduit equivalent of [`RemoteTranscriptMirror::sync_telemetry`].
    #[allow(clippy::too_many_arguments)]
    pub async fn sync_telemetry_conduit(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        cache: &Mutex<TranscriptIndexCache>,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        now_ms: u64,
    ) -> Result<RemoteTelemetryOutcome, TmuxError> {
        let sync = self
            .ensure_fresh_conduit(
                pool,
                lane,
                host_id,
                remote_home,
                agent,
                remote_path,
                latest_stat,
                false,
            )
            .await?;
        let (metrics, model, effort) = {
            let mut guard = lock_mutex(cache);
            guard.sync_and_telemetry(agent, &sync.local_path, now_ms)
        };
        Ok((metrics, model, effort, sync))
    }

    /// Persistent-conduit equivalent of [`RemoteTranscriptMirror::read_page_and_metrics`].
    /// Never holds `cache`'s mutex across async conduit I/O.
    #[allow(clippy::too_many_arguments)]
    pub async fn read_page_and_metrics_conduit(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        cache: &Mutex<TranscriptIndexCache>,
        host_id: &str,
        remote_home: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        query: TranscriptQuery,
        session_metrics: Option<SessionMetrics>,
        now_ms: u64,
    ) -> Result<RemotePageOutcome, TmuxError> {
        let mut sync = self
            .ensure_fresh_conduit(
                pool,
                lane,
                host_id,
                remote_home,
                agent,
                remote_path,
                latest_stat,
                query.all,
            )
            .await?;

        let effective_limit = if query.all {
            MAX_TRANSCRIPT_LIMIT
        } else {
            query.limit.clamp(1, MAX_TRANSCRIPT_LIMIT)
        };
        let mut effective_query = query;

        let reaches_start = query.all || query.before.is_some_and(|b| b <= effective_limit);
        if sync.base_offset > 0 && reaches_start {
            let old_total = {
                let mut guard = lock_mutex(cache);
                guard
                    .total_messages_for_path(agent, &sync.local_path)
                    .unwrap_or(0)
            };
            let backfilled = self
                .backfill_prefix_conduit(pool, lane, host_id, remote_home, agent, remote_path)
                .await?;
            if backfilled.changed {
                let new_total = {
                    let mut guard = lock_mutex(cache);
                    guard.invalidate_path(&backfilled.local_path);
                    guard
                        .total_messages_for_path(agent, &backfilled.local_path)
                        .unwrap_or(old_total)
                };
                if let Some(b) = effective_query.before {
                    let prefix_delta = new_total.saturating_sub(old_total);
                    effective_query.before = Some(b.saturating_add(prefix_delta));
                }
            }
            sync = backfilled;
        }

        if agent == AgentKind::Agy {
            let missing = {
                let mut guard = lock_mutex(cache);
                guard.missing_agy_hydration_steps(
                    agent,
                    &sync.local_path,
                    effective_query,
                    MAX_AGY_HYDRATE_STEPS,
                )
            };
            if !missing.is_empty() {
                match self
                    .hydrate_remote_agy_steps_conduit(
                        pool,
                        lane,
                        host_id,
                        remote_home,
                        remote_path,
                        &missing,
                    )
                    .await
                {
                    Ok(hydrated_count) if hydrated_count > 0 => {
                        let mut guard = lock_mutex(cache);
                        guard.invalidate_pages(&sync.local_path);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::debug!(
                            "remote AGY selective hydration (conduit) skipped for {remote_path}: {e}"
                        );
                    }
                }
            }
        }

        let (mut resp, model, effort) = {
            let mut guard = lock_mutex(cache);
            let resp = guard
                .get_page_or_sync(
                    agent,
                    &sync.local_path,
                    effective_query,
                    session_metrics.clone(),
                    now_ms,
                )
                .unwrap_or_else(|| TranscriptResponse {
                    source: "none".to_string(),
                    transcript_path: None,
                    messages: Vec::new(),
                    has_more: false,
                    total_messages: 0,
                    start_seq: 0,
                    next_before: None,
                    metrics: session_metrics,
                });
            let (_, model, effort) = guard.sync_and_telemetry(agent, &sync.local_path, now_ms);
            (resp, model, effort)
        };

        if resp.source == "transcript" {
            resp.transcript_path = Some(remote_path.to_string());
            resp.has_more = resp.has_more || sync.base_offset > 0;
            if resp.has_more && resp.next_before.is_none() {
                resp.next_before = Some(resp.start_seq);
            }
        }

        Ok(RemotePageOutcome {
            response: resp,
            model,
            effort,
            sync,
        })
    }

    // -----------------------------------------------------------------------
    // Internal locked sync / backfill implementation
    // -----------------------------------------------------------------------

    fn ensure_fresh_locked(
        &self,
        ssh: &SshExec,
        host_id: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        need_full_history: bool,
    ) -> Result<MirrorSyncResult, TmuxError> {
        let paths = self.cache_paths(host_id, agent, remote_path);
        let existing_meta = load_valid_meta(&paths.local_path, &paths.meta_path, remote_path);

        // 0-RTT fast path: `latest_stat` proves remote file has identical (ino, size) and we
        // already satisfy `need_full_history`.
        if let (Some(stat), Some(meta)) = (latest_stat, existing_meta.as_ref()) {
            if meta.remote_ino != 0
                && stat.ino == meta.remote_ino
                && stat.size == meta.synced_offset
                && (!need_full_history || meta.base_offset == 0)
            {
                return Ok(MirrorSyncResult {
                    local_path: paths.local_path,
                    base_offset: meta.base_offset,
                    synced_offset: meta.synced_offset,
                    changed: false,
                });
            }
        }

        // If we already have a valid tail-seeded mirror (`base_offset > 0`) and the caller now wants
        // full history (`need_full_history == true`), backfill prefix first if inode/size still match.
        let mut current_meta = existing_meta;
        let mut any_changed = false;
        if need_full_history {
            if let Some(ref meta) = current_meta {
                let compatible = latest_stat.is_none_or(|s| {
                    meta.remote_ino != 0 && s.ino == meta.remote_ino && s.size >= meta.synced_offset
                });
                if meta.base_offset > 0 && compatible {
                    let bf = self.backfill_prefix_locked(ssh, host_id, agent, remote_path)?;
                    any_changed |= bf.changed;
                    current_meta =
                        load_valid_meta(&paths.local_path, &paths.meta_path, remote_path);
                    // If the remote size already matches `synced_offset` (either via `latest_stat`
                    // or via the live `sf_stat` returned by `backfill_prefix`), we are done in 1 RTT!
                    if let Some(ref updated) = current_meta {
                        let up_to_date = match latest_stat {
                            Some(stat) => {
                                updated.remote_ino != 0
                                    && stat.ino == updated.remote_ino
                                    && stat.size == updated.synced_offset
                            }
                            None => updated.remote_size == updated.synced_offset,
                        };
                        if up_to_date {
                            return Ok(MirrorSyncResult {
                                local_path: paths.local_path,
                                base_offset: updated.base_offset,
                                synced_offset: updated.synced_offset,
                                changed: any_changed,
                            });
                        }
                    }
                }
            }
        }

        // Pull cold seed or incremental delta chunks until caught up to `remote_size`
        // (bounded to prevent runaway loops if the remote file is continuously written).
        let max_chunks = ((MAX_REMOTE_MIRROR_BYTES / MAX_DELTA_CHUNK) as usize).max(1) + 2;
        for _ in 0..max_chunks {
            let (prev_ino, prev_off, cur_local_len) = match &current_meta {
                Some(m) => {
                    let local_len = m.synced_offset.saturating_sub(m.base_offset);
                    // Enforce MAX_REMOTE_MIRROR_BYTES cap by re-seeding if local mirror is full.
                    if local_len >= MAX_REMOTE_MIRROR_BYTES {
                        (0, 0, 0)
                    } else {
                        (m.remote_ino, m.synced_offset, local_len)
                    }
                }
                None => (0, 0, 0),
            };
            let remaining_mirror_budget = MAX_REMOTE_MIRROR_BYTES
                .saturating_sub(cur_local_len)
                .min(MAX_DELTA_CHUNK);
            let chunk_cap = if prev_ino == 0 {
                MAX_DELTA_CHUNK
            } else {
                remaining_mirror_budget.max(1)
            };

            let script = build_ensure_fresh_script(
                remote_path,
                prev_ino,
                prev_off,
                need_full_history,
                chunk_cap,
            );
            let out = ssh.run_idempotent_shell_bytes(&script, None, ssh.timeout())?;
            let (hdr, raw_bytes) = parse_fetch_response(&out)?;

            if hdr.reset {
                // Cold start, inode rotation, truncation, or mirror-cap re-window.
                ensure_private_dir(&paths.dir).map_err(|e| TmuxError::Io {
                    what: format!("create remote cache dir {}", paths.dir.display()),
                    source: e,
                })?;
                if agent == AgentKind::Agy {
                    // Remove stale sparse `transcript_full.jsonl` when compact file rotated/reset.
                    let stale_full = paths.local_path.with_file_name("transcript_full.jsonl");
                    if stale_full.exists() {
                        let _ = fs::remove_file(&stale_full);
                    }
                }

                let (clean_slice, base_offset) = if hdr.offset > 0 {
                    // Cold-start tail window: discard leading partial JSONL bytes up to first `\n`.
                    align_tail_slice(&raw_bytes, hdr.offset)
                } else {
                    (raw_bytes.as_slice(), 0)
                };
                let synced_offset = hdr.offset.saturating_add(hdr.raw_len);
                atomic_write_private(&paths.local_path, clean_slice).map_err(|e| {
                    TmuxError::Io {
                        what: format!("write mirror file {}", paths.local_path.display()),
                        source: e,
                    }
                })?;

                let next_meta = MirrorMeta {
                    remote_path: remote_path.to_string(),
                    remote_ino: hdr.stat.ino,
                    remote_size: hdr.stat.size,
                    remote_mtime_s: hdr.stat.mtime_s,
                    base_offset,
                    synced_offset,
                };
                save_meta(&paths.meta_path, &next_meta)?;
                current_meta = Some(next_meta);
                any_changed = true;
            } else if hdr.raw_len == 0 {
                // Remote unchanged since `prev_off`.
                let meta = current_meta.as_mut().ok_or_else(|| {
                    TmuxError::Parse("unexpected zero-byte non-reset mirror response".into())
                })?;
                if meta.remote_mtime_s != hdr.stat.mtime_s || meta.remote_size != hdr.stat.size {
                    meta.remote_size = hdr.stat.size;
                    meta.remote_mtime_s = hdr.stat.mtime_s;
                    save_meta(&paths.meta_path, meta)?;
                }
                break;
            } else {
                // Incremental delta append at exact `meta.synced_offset`.
                let meta = current_meta.as_ref().ok_or_else(|| {
                    TmuxError::Parse("missing mirror meta on incremental append".into())
                })?;
                if hdr.offset != meta.synced_offset || hdr.stat.ino != meta.remote_ino {
                    return Err(TmuxError::Parse(format!(
                        "mirror delta mismatch: expected ino={} off={}, got ino={} off={}",
                        meta.remote_ino, meta.synced_offset, hdr.stat.ino, hdr.offset
                    )));
                }
                let expected_local_len = meta.synced_offset.saturating_sub(meta.base_offset);
                let actual_local_len = fs::metadata(&paths.local_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if actual_local_len != expected_local_len {
                    // Local cache changed externally; re-seed on next iteration.
                    current_meta = None;
                    continue;
                }

                let mut file =
                    open_private_append(&paths.local_path).map_err(|e| TmuxError::Io {
                        what: format!("append mirror file {}", paths.local_path.display()),
                        source: e,
                    })?;
                file.write_all(&raw_bytes).map_err(|e| TmuxError::Io {
                    what: format!("write mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;
                file.flush().map_err(|e| TmuxError::Io {
                    what: format!("flush mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;

                let synced_offset = hdr.offset.saturating_add(hdr.raw_len);
                let next_meta = MirrorMeta {
                    remote_path: remote_path.to_string(),
                    remote_ino: hdr.stat.ino,
                    remote_size: hdr.stat.size,
                    remote_mtime_s: hdr.stat.mtime_s,
                    base_offset: meta.base_offset,
                    synced_offset,
                };
                save_meta(&paths.meta_path, &next_meta)?;
                current_meta = Some(next_meta);
                any_changed = true;
            }

            let Some(ref m) = current_meta else {
                break;
            };
            if m.synced_offset >= m.remote_size
                || m.synced_offset.saturating_sub(m.base_offset) >= MAX_REMOTE_MIRROR_BYTES
            {
                break;
            }
        }

        let final_meta = current_meta.ok_or_else(|| {
            TmuxError::Parse(format!("failed to materialize mirror for {remote_path}"))
        })?;
        Ok(MirrorSyncResult {
            local_path: paths.local_path,
            base_offset: final_meta.base_offset,
            synced_offset: final_meta.synced_offset,
            changed: any_changed,
        })
    }

    fn backfill_prefix_locked(
        &self,
        ssh: &SshExec,
        host_id: &str,
        agent: AgentKind,
        remote_path: &str,
    ) -> Result<MirrorSyncResult, TmuxError> {
        let paths = self.cache_paths(host_id, agent, remote_path);
        let Some(meta) = load_valid_meta(&paths.local_path, &paths.meta_path, remote_path) else {
            return self.ensure_fresh_locked(ssh, host_id, agent, remote_path, None, true);
        };
        if meta.base_offset == 0 {
            return Ok(MirrorSyncResult {
                local_path: paths.local_path,
                base_offset: 0,
                synced_offset: meta.synced_offset,
                changed: false,
            });
        }

        let cur_local_len = meta.synced_offset.saturating_sub(meta.base_offset);
        let budget = MAX_REMOTE_MIRROR_BYTES.saturating_sub(cur_local_len);
        if budget == 0 {
            return Ok(MirrorSyncResult {
                local_path: paths.local_path,
                base_offset: meta.base_offset,
                synced_offset: meta.synced_offset,
                changed: false,
            });
        }

        let target_start = meta.base_offset.saturating_sub(budget);
        let total_prefix_bytes = meta.base_offset.saturating_sub(target_start);
        let mut prefix_buf: Vec<u8> = Vec::with_capacity(
            usize::try_from(total_prefix_bytes).unwrap_or(MAX_DELTA_CHUNK as usize),
        );
        let mut cursor = target_start;
        let mut latest_remote_stat = RemoteFileStat {
            ino: meta.remote_ino,
            size: meta.remote_size,
            mtime_s: meta.remote_mtime_s,
        };

        while cursor < meta.base_offset {
            let cap = (meta.base_offset - cursor).min(MAX_DELTA_CHUNK);
            let script = build_range_fetch_script(remote_path, meta.remote_ino, cursor, cap);
            let out = ssh.run_idempotent_shell_bytes(&script, None, ssh.timeout())?;
            match parse_range_fetch_response(&out)? {
                (RangeFetchOutcome::Rotated(_) | RangeFetchOutcome::Truncated(_), _) => {
                    // Remote file rotated or shrank mid-backfill: fall back cleanly to full re-seed.
                    return self.ensure_fresh_locked(ssh, host_id, agent, remote_path, None, true);
                }
                (RangeFetchOutcome::Header(hdr), chunk_bytes) => {
                    if hdr.offset != cursor || hdr.raw_len != cap {
                        return Err(TmuxError::Parse(format!(
                            "backfill range mismatch: expected off={cursor} len={cap}, got off={} len={}",
                            hdr.offset, hdr.raw_len
                        )));
                    }
                    latest_remote_stat = hdr.stat;
                    prefix_buf.extend_from_slice(&chunk_bytes);
                    cursor = cursor.saturating_add(cap);
                }
            }
        }

        let (prefix_slice, new_base_offset) = if target_start > 0 {
            align_tail_slice(&prefix_buf, target_start)
        } else {
            (prefix_buf.as_slice(), 0)
        };

        // Atomically stitch `prefix_slice + existing_tail` into a private `.tmp` file and rename.
        atomic_stitch_prefix_private(&paths.local_path, prefix_slice).map_err(|e| {
            TmuxError::Io {
                what: format!(
                    "stitch backfilled prefix into {}",
                    paths.local_path.display()
                ),
                source: e,
            }
        })?;

        let updated_meta = MirrorMeta {
            remote_path: remote_path.to_string(),
            remote_ino: latest_remote_stat.ino,
            remote_size: latest_remote_stat.size,
            remote_mtime_s: latest_remote_stat.mtime_s,
            base_offset: new_base_offset,
            synced_offset: meta.synced_offset,
        };
        save_meta(&paths.meta_path, &updated_meta)?;

        Ok(MirrorSyncResult {
            local_path: paths.local_path,
            base_offset: updated_meta.base_offset,
            synced_offset: updated_meta.synced_offset,
            changed: true,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_fresh_conduit_locked(
        &self,
        pool: &SshConduitPool,
        lane: ConduitLane,
        host_id: &str,
        agent: AgentKind,
        remote_path: &str,
        latest_stat: Option<RemoteFileStat>,
        need_full_history: bool,
    ) -> Result<MirrorSyncResult, TmuxError> {
        let paths = self.cache_paths(host_id, agent, remote_path);
        let existing_meta = {
            let key_lock = self.lock_for(host_id, remote_path);
            let _g = lock_mutex(&key_lock);
            load_valid_meta(&paths.local_path, &paths.meta_path, remote_path)
        };

        if let (Some(stat), Some(meta)) = (latest_stat, existing_meta.as_ref()) {
            if meta.remote_ino != 0
                && stat.ino == meta.remote_ino
                && stat.size == meta.synced_offset
                && (!need_full_history || meta.base_offset == 0)
            {
                return Ok(MirrorSyncResult {
                    local_path: paths.local_path,
                    base_offset: meta.base_offset,
                    synced_offset: meta.synced_offset,
                    changed: false,
                });
            }
        }

        let mut current_meta = existing_meta;
        let mut any_changed = false;
        if need_full_history {
            if let Some(ref meta) = current_meta {
                let compatible = latest_stat.is_none_or(|s| {
                    meta.remote_ino != 0 && s.ino == meta.remote_ino && s.size >= meta.synced_offset
                });
                if meta.base_offset > 0 && compatible {
                    let bf = self
                        .backfill_prefix_conduit_locked(pool, lane, host_id, agent, remote_path)
                        .await?;
                    any_changed |= bf.changed;
                    current_meta = {
                        let key_lock = self.lock_for(host_id, remote_path);
                        let _g = lock_mutex(&key_lock);
                        load_valid_meta(&paths.local_path, &paths.meta_path, remote_path)
                    };
                    if let Some(ref updated) = current_meta {
                        let up_to_date = match latest_stat {
                            Some(stat) => {
                                updated.remote_ino != 0
                                    && stat.ino == updated.remote_ino
                                    && stat.size == updated.synced_offset
                            }
                            None => updated.remote_size == updated.synced_offset,
                        };
                        if up_to_date {
                            return Ok(MirrorSyncResult {
                                local_path: paths.local_path,
                                base_offset: updated.base_offset,
                                synced_offset: updated.synced_offset,
                                changed: any_changed,
                            });
                        }
                    }
                }
            }
        }

        let max_chunks = ((MAX_REMOTE_MIRROR_BYTES / MAX_DELTA_CHUNK) as usize).max(1) + 2;
        for _ in 0..max_chunks {
            let (prev_ino, prev_off, cur_local_len) = match &current_meta {
                Some(m) => {
                    let local_len = m.synced_offset.saturating_sub(m.base_offset);
                    if local_len >= MAX_REMOTE_MIRROR_BYTES {
                        (0, 0, 0)
                    } else {
                        (m.remote_ino, m.synced_offset, local_len)
                    }
                }
                None => (0, 0, 0),
            };
            let remaining_mirror_budget = MAX_REMOTE_MIRROR_BYTES
                .saturating_sub(cur_local_len)
                .min(MAX_DELTA_CHUNK);
            let chunk_cap = if prev_ino == 0 {
                MAX_DELTA_CHUNK
            } else {
                remaining_mirror_budget.max(1)
            };

            let script = build_ensure_fresh_script(
                remote_path,
                prev_ino,
                prev_off,
                need_full_history,
                chunk_cap,
            );
            let out = pool
                .exec_bytes(lane, &script, None, pool.ssh().timeout())
                .await?;
            let (hdr, raw_bytes) = parse_fetch_response_async(out).await?;

            let key_lock = self.lock_for(host_id, remote_path);
            let _sync_guard = lock_mutex(&key_lock);

            if hdr.reset {
                ensure_private_dir(&paths.dir).map_err(|e| TmuxError::Io {
                    what: format!("create remote cache dir {}", paths.dir.display()),
                    source: e,
                })?;
                if agent == AgentKind::Agy {
                    let stale_full = paths.local_path.with_file_name("transcript_full.jsonl");
                    if stale_full.exists() {
                        let _ = fs::remove_file(&stale_full);
                    }
                }

                let (clean_slice, base_offset) = if hdr.offset > 0 {
                    align_tail_slice(&raw_bytes, hdr.offset)
                } else {
                    (raw_bytes.as_slice(), 0)
                };
                let synced_offset = hdr.offset.saturating_add(hdr.raw_len);
                atomic_write_private(&paths.local_path, clean_slice).map_err(|e| {
                    TmuxError::Io {
                        what: format!("write mirror file {}", paths.local_path.display()),
                        source: e,
                    }
                })?;

                let next_meta = MirrorMeta {
                    remote_path: remote_path.to_string(),
                    remote_ino: hdr.stat.ino,
                    remote_size: hdr.stat.size,
                    remote_mtime_s: hdr.stat.mtime_s,
                    base_offset,
                    synced_offset,
                };
                save_meta(&paths.meta_path, &next_meta)?;
                current_meta = Some(next_meta);
                any_changed = true;
            } else if hdr.raw_len == 0 {
                let meta = current_meta.as_mut().ok_or_else(|| {
                    TmuxError::Parse("unexpected zero-byte non-reset mirror response".into())
                })?;
                if meta.remote_mtime_s != hdr.stat.mtime_s || meta.remote_size != hdr.stat.size {
                    meta.remote_size = hdr.stat.size;
                    meta.remote_mtime_s = hdr.stat.mtime_s;
                    save_meta(&paths.meta_path, meta)?;
                }
                break;
            } else {
                let meta = current_meta.as_ref().ok_or_else(|| {
                    TmuxError::Parse("missing mirror meta on incremental append".into())
                })?;
                if hdr.offset != meta.synced_offset || hdr.stat.ino != meta.remote_ino {
                    return Err(TmuxError::Parse(format!(
                        "mirror delta mismatch: expected ino={} off={}, got ino={} off={}",
                        meta.remote_ino, meta.synced_offset, hdr.stat.ino, hdr.offset
                    )));
                }
                let expected_local_len = meta.synced_offset.saturating_sub(meta.base_offset);
                let actual_local_len = fs::metadata(&paths.local_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if actual_local_len != expected_local_len {
                    current_meta = None;
                    continue;
                }

                let mut file =
                    open_private_append(&paths.local_path).map_err(|e| TmuxError::Io {
                        what: format!("append mirror file {}", paths.local_path.display()),
                        source: e,
                    })?;
                file.write_all(&raw_bytes).map_err(|e| TmuxError::Io {
                    what: format!("write mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;
                file.flush().map_err(|e| TmuxError::Io {
                    what: format!("flush mirror delta {}", paths.local_path.display()),
                    source: e,
                })?;

                let synced_offset = hdr.offset.saturating_add(hdr.raw_len);
                let next_meta = MirrorMeta {
                    remote_path: remote_path.to_string(),
                    remote_ino: hdr.stat.ino,
                    remote_size: hdr.stat.size,
                    remote_mtime_s: hdr.stat.mtime_s,
                    base_offset: meta.base_offset,
                    synced_offset,
                };
                save_meta(&paths.meta_path, &next_meta)?;
                current_meta = Some(next_meta);
                any_changed = true;
            }

            let Some(ref m) = current_meta else {
                break;
            };
            if m.synced_offset >= m.remote_size
                || m.synced_offset.saturating_sub(m.base_offset) >= MAX_REMOTE_MIRROR_BYTES
            {
                break;
            }
        }

        let final_meta = current_meta.ok_or_else(|| {
            TmuxError::Parse(format!("failed to materialize mirror for {remote_path}"))
        })?;
        Ok(MirrorSyncResult {
            local_path: paths.local_path,
            base_offset: final_meta.base_offset,
            synced_offset: final_meta.synced_offset,
            changed: any_changed,
        })
    }

    fn backfill_prefix_conduit_locked<'a>(
        &'a self,
        pool: &'a SshConduitPool,
        lane: ConduitLane,
        host_id: &'a str,
        agent: AgentKind,
        remote_path: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MirrorSyncResult, TmuxError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let paths = self.cache_paths(host_id, agent, remote_path);
            let meta_opt = {
                let key_lock = self.lock_for(host_id, remote_path);
                let _g = lock_mutex(&key_lock);
                load_valid_meta(&paths.local_path, &paths.meta_path, remote_path)
            };
            let Some(meta) = meta_opt else {
                return self
                    .ensure_fresh_conduit_locked(
                        pool,
                        lane,
                        host_id,
                        agent,
                        remote_path,
                        None,
                        true,
                    )
                    .await;
            };
            if meta.base_offset == 0 {
                return Ok(MirrorSyncResult {
                    local_path: paths.local_path,
                    base_offset: 0,
                    synced_offset: meta.synced_offset,
                    changed: false,
                });
            }

            let cur_local_len = meta.synced_offset.saturating_sub(meta.base_offset);
            let budget = MAX_REMOTE_MIRROR_BYTES.saturating_sub(cur_local_len);
            if budget == 0 {
                return Ok(MirrorSyncResult {
                    local_path: paths.local_path,
                    base_offset: meta.base_offset,
                    synced_offset: meta.synced_offset,
                    changed: false,
                });
            }

            let target_start = meta.base_offset.saturating_sub(budget);
            let total_prefix_bytes = meta.base_offset.saturating_sub(target_start);
            let mut prefix_buf: Vec<u8> = Vec::with_capacity(
                usize::try_from(total_prefix_bytes).unwrap_or(MAX_DELTA_CHUNK as usize),
            );
            let mut cursor = target_start;
            let mut latest_remote_stat = RemoteFileStat {
                ino: meta.remote_ino,
                size: meta.remote_size,
                mtime_s: meta.remote_mtime_s,
            };

            while cursor < meta.base_offset {
                let cap = (meta.base_offset - cursor).min(MAX_DELTA_CHUNK);
                let script = build_range_fetch_script(remote_path, meta.remote_ino, cursor, cap);
                let out = pool
                    .exec_bytes(lane, &script, None, pool.ssh().timeout())
                    .await?;
                match parse_range_fetch_response_async(out).await? {
                    (RangeFetchOutcome::Rotated(_) | RangeFetchOutcome::Truncated(_), _) => {
                        return self
                            .ensure_fresh_conduit_locked(
                                pool,
                                lane,
                                host_id,
                                agent,
                                remote_path,
                                None,
                                true,
                            )
                            .await;
                    }
                    (RangeFetchOutcome::Header(hdr), chunk_bytes) => {
                        if hdr.offset != cursor || hdr.raw_len != cap {
                            return Err(TmuxError::Parse(format!(
                                "backfill range mismatch: expected off={cursor} len={cap}, got off={} len={}",
                                hdr.offset, hdr.raw_len
                            )));
                        }
                        latest_remote_stat = hdr.stat;
                        prefix_buf.extend_from_slice(&chunk_bytes);
                        cursor = cursor.saturating_add(cap);
                    }
                }
            }

            let (prefix_slice, new_base_offset) = if target_start > 0 {
                align_tail_slice(&prefix_buf, target_start)
            } else {
                (prefix_buf.as_slice(), 0)
            };

            let key_lock = self.lock_for(host_id, remote_path);
            let _sync_guard = lock_mutex(&key_lock);

            atomic_stitch_prefix_private(&paths.local_path, prefix_slice).map_err(|e| {
                TmuxError::Io {
                    what: format!(
                        "stitch backfilled prefix into {}",
                        paths.local_path.display()
                    ),
                    source: e,
                }
            })?;

            let updated_meta = MirrorMeta {
                remote_path: remote_path.to_string(),
                remote_ino: latest_remote_stat.ino,
                remote_size: latest_remote_stat.size,
                remote_mtime_s: latest_remote_stat.mtime_s,
                base_offset: new_base_offset,
                synced_offset: meta.synced_offset,
            };
            save_meta(&paths.meta_path, &updated_meta)?;

            Ok(MirrorSyncResult {
                local_path: paths.local_path,
                base_offset: updated_meta.base_offset,
                synced_offset: updated_meta.synced_offset,
                changed: true,
            })
        })
    }

    fn lock_for(&self, host_id: &str, remote_path: &str) -> Arc<Mutex<()>> {
        let mut map = lock_mutex(&self.key_locks);
        let key = (host_id.to_string(), remote_path.to_string());
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn async_lock_for(&self, host_id: &str, remote_path: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = lock_mutex(&self.async_key_locks);
        let key = (host_id.to_string(), remote_path.to_string());
        map.entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cache_paths(&self, host_id: &str, agent: AgentKind, remote_path: &str) -> CachePaths {
        let safe_host = sanitize_cache_component(host_id);
        let host_dir = self.cache_root.join(safe_host);
        if agent == AgentKind::Agy {
            let conv_id = extract_agy_conv_id(remote_path)
                .map(sanitize_cache_component)
                .unwrap_or_else(|| fnv128_hex(remote_path.as_bytes()));
            let agy_dir = host_dir.join(format!("agy-{conv_id}"));
            let filename = if remote_path.ends_with("/transcript_full.jsonl") {
                "transcript_full.jsonl"
            } else {
                "transcript.jsonl"
            };
            CachePaths {
                local_path: agy_dir.join(filename),
                meta_path: agy_dir.join(".meta.json"),
                dir: agy_dir,
            }
        } else {
            let hash = fnv128_hex(remote_path.as_bytes());
            CachePaths {
                local_path: host_dir.join(format!("{hash}.jsonl")),
                meta_path: host_dir.join(format!("{hash}.meta.json")),
                dir: host_dir,
            }
        }
    }
}

struct CachePaths {
    dir: PathBuf,
    local_path: PathBuf,
    meta_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Remote POSIX /bin/sh scripts & response parsers
// ---------------------------------------------------------------------------

const POSIX_SF_STAT_FN: &str = "\
sf_stat() {
  case \"${SF_STAT_MODE:-}\" in
    gnu) stat -c '%i %s %Y' \"$1\" 2>/dev/null || printf '0 -1 0\\n' ;;
    bsd) stat -f '%i %z %m' \"$1\" 2>/dev/null || printf '0 -1 0\\n' ;;
    *) stat -c '%i %s %Y' \"$1\" 2>/dev/null || stat -f '%i %z %m' \"$1\" 2>/dev/null || printf '0 -1 0\\n' ;;
  esac
}";

fn build_ensure_fresh_script(
    remote_path: &str,
    prev_ino: u64,
    prev_off: u64,
    need_full: bool,
    chunk_cap: u64,
) -> String {
    let quoted_path = ssh_universal_quote(remote_path);
    let need_full_flag = if need_full { 1 } else { 0 };
    format!(
        "\
P={quoted_path}
PREV_INO={prev_ino}
PREV_OFF={prev_off}
NEED_FULL={need_full_flag}
TAIL_WIN={INITIAL_TAIL_WINDOW}
MAX_MIRROR={MAX_REMOTE_MIRROR_BYTES}
MAX_CHUNK={chunk_cap}
GZ_THRESH={GZIP_COMPRESS_THRESHOLD}
if [ -L \"$P\" ]; then
  printf 'ERR SYMLINK\\n'
  exit 42
fi
if [ ! -f \"$P\" ]; then
  printf 'ERR NOT_FOUND\\n'
  exit 44
fi
{POSIX_SF_STAT_FN}
set -- $(sf_stat \"$P\")
INO=\"$1\"; SIZE=\"$2\"; MTIME=\"$3\"
if [ -z \"$SIZE\" ] || [ \"$SIZE\" -lt 0 ] 2>/dev/null; then
  printf 'ERR STAT_FAILED\\n'
  exit 45
fi
RESET=0
if [ \"$PREV_INO\" -eq 0 ] || [ \"$INO\" -ne \"$PREV_INO\" ] || [ \"$SIZE\" -lt \"$PREV_OFF\" ]; then
  RESET=1
  if [ \"$NEED_FULL\" -eq 0 ] && [ \"$SIZE\" -gt \"$TAIL_WIN\" ]; then
    OFF=$((SIZE - TAIL_WIN))
  elif [ \"$SIZE\" -gt \"$MAX_MIRROR\" ]; then
    OFF=$((SIZE - MAX_MIRROR))
  else
    OFF=0
  fi
else
  OFF=\"$PREV_OFF\"
fi
REM=$((SIZE - OFF))
if [ \"$REM\" -le 0 ]; then
  printf 'HDR %s %s %s %s 0 RAW %s\\n' \"$INO\" \"$SIZE\" \"$MTIME\" \"$OFF\" \"$RESET\"
  exit 0
fi
RAW_LEN=\"$REM\"
if [ \"$RAW_LEN\" -gt \"$MAX_CHUNK\" ]; then
  RAW_LEN=\"$MAX_CHUNK\"
fi
MODE=\"RAW\"
if [ \"$RAW_LEN\" -ge \"$GZ_THRESH\" ] && command -v gzip >/dev/null 2>&1; then
  MODE=\"GZ1\"
fi
printf 'HDR %s %s %s %s %s %s %s\\n' \"$INO\" \"$SIZE\" \"$MTIME\" \"$OFF\" \"$RAW_LEN\" \"$MODE\" \"$RESET\"
START=$((OFF + 1))
if [ \"$MODE\" = \"GZ1\" ]; then
  tail -c +\"$START\" \"$P\" 2>/dev/null | head -c \"$RAW_LEN\" | gzip -1 -c
else
  tail -c +\"$START\" \"$P\" 2>/dev/null | head -c \"$RAW_LEN\"
fi
"
    )
}

fn build_range_fetch_script(remote_path: &str, expected_ino: u64, off: u64, cap: u64) -> String {
    let quoted_path = ssh_universal_quote(remote_path);
    format!(
        "\
P={quoted_path}
EXP_INO={expected_ino}
OFF={off}
CAP={cap}
GZ_THRESH={GZIP_COMPRESS_THRESHOLD}
if [ -L \"$P\" ]; then
  printf 'ERR SYMLINK\\n'
  exit 42
fi
if [ ! -f \"$P\" ]; then
  printf 'ERR NOT_FOUND\\n'
  exit 44
fi
{POSIX_SF_STAT_FN}
set -- $(sf_stat \"$P\")
INO=\"$1\"; SIZE=\"$2\"; MTIME=\"$3\"
if [ -z \"$SIZE\" ] || [ \"$SIZE\" -lt 0 ] 2>/dev/null; then
  printf 'ERR STAT_FAILED\\n'
  exit 45
fi
if [ \"$EXP_INO\" -ne 0 ] && [ \"$INO\" -ne \"$EXP_INO\" ]; then
  printf 'ERR ROTATED %s %s %s\\n' \"$INO\" \"$SIZE\" \"$MTIME\"
  exit 0
fi
if [ \"$SIZE\" -lt $((OFF + CAP)) ]; then
  printf 'ERR TRUNCATED %s %s %s\\n' \"$INO\" \"$SIZE\" \"$MTIME\"
  exit 0
fi
MODE=\"RAW\"
if [ \"$CAP\" -ge \"$GZ_THRESH\" ] && command -v gzip >/dev/null 2>&1; then
  MODE=\"GZ1\"
fi
printf 'HDR %s %s %s %s %s %s 0\\n' \"$INO\" \"$SIZE\" \"$MTIME\" \"$OFF\" \"$CAP\" \"$MODE\"
START=$((OFF + 1))
if [ \"$MODE\" = \"GZ1\" ]; then
  tail -c +\"$START\" \"$P\" 2>/dev/null | head -c \"$CAP\" | gzip -1 -c
else
  tail -c +\"$START\" \"$P\" 2>/dev/null | head -c \"$CAP\"
fi
"
    )
}

fn build_agy_hydrate_script(remote_full_path: &str, steps: &[usize]) -> String {
    let quoted_path = ssh_universal_quote(remote_full_path);
    let steps_csv = steps
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let quoted_steps = ssh_universal_quote(&steps_csv);
    let awk_prog = ssh_universal_quote(
        "BEGIN { n = split(STEPS, arr, \",\"); for (i = 1; i <= n; i++) want[arr[i]] = 1 } \
         length($0) <= MAX_BYTES && match($0, /\"step_index\"[ \\t]*:[ \\t]*[0-9]+/) { \
           seg = substr($0, RSTART, RLENGTH); \
           sub(/^\"step_index\"[ \\t]*:[ \\t]*/, \"\", seg); \
           if (seg in want) print $0 \
         }",
    );
    format!(
        "\
P={quoted_path}
if [ -L \"$P\" ]; then
  printf 'ERR SYMLINK\\n'
  exit 42
fi
if [ ! -f \"$P\" ]; then
  printf 'ERR NOT_FOUND\\n'
  exit 44
fi
if command -v gzip >/dev/null 2>&1; then
  printf 'HDR GZ1\\n'
  LC_ALL=C awk -v STEPS={quoted_steps} -v MAX_BYTES={MAX_AGY_HYDRATE_STEP_BYTES} {awk_prog} \"$P\" | gzip -1 -c
else
  printf 'HDR RAW\\n'
  LC_ALL=C awk -v STEPS={quoted_steps} -v MAX_BYTES={MAX_AGY_HYDRATE_STEP_BYTES} {awk_prog} \"$P\"
fi
"
    )
}

fn parse_fetch_response(
    out: &starfane_tmux::ExecBytesOutput,
) -> Result<(FetchHeader, Vec<u8>), TmuxError> {
    let (hdr_line, payload) = split_header_and_payload(out)?;
    if let Some(err_code) = hdr_line.strip_prefix("ERR ") {
        return Err(map_remote_err_code(err_code.trim(), out));
    }
    if !out.success() {
        return Err(TmuxError::Command {
            args: "remote_mirror fetch".to_string(),
            code: out.exit_code,
            stderr: out.stderr.clone(),
        });
    }
    let hdr = parse_fetch_header_line(hdr_line)?;
    let decoded = decode_transfer_payload(hdr.mode, payload, hdr.raw_len)?;
    Ok((hdr, decoded))
}

fn parse_range_fetch_response(
    out: &starfane_tmux::ExecBytesOutput,
) -> Result<(RangeFetchOutcome, Vec<u8>), TmuxError> {
    let (hdr_line, payload) = split_header_and_payload(out)?;
    if let Some(rest) = hdr_line.strip_prefix("ERR ROTATED ") {
        let stat = parse_stat_triplet(rest)?;
        return Ok((RangeFetchOutcome::Rotated(stat), Vec::new()));
    }
    if let Some(rest) = hdr_line.strip_prefix("ERR TRUNCATED ") {
        let stat = parse_stat_triplet(rest)?;
        return Ok((RangeFetchOutcome::Truncated(stat), Vec::new()));
    }
    if let Some(err_code) = hdr_line.strip_prefix("ERR ") {
        return Err(map_remote_err_code(err_code.trim(), out));
    }
    if !out.success() {
        return Err(TmuxError::Command {
            args: "remote_mirror range fetch".to_string(),
            code: out.exit_code,
            stderr: out.stderr.clone(),
        });
    }
    let hdr = parse_fetch_header_line(hdr_line)?;
    let decoded = decode_transfer_payload(hdr.mode, payload, hdr.raw_len)?;
    Ok((RangeFetchOutcome::Header(hdr), decoded))
}

fn parse_hydrate_response(
    out: &starfane_tmux::ExecBytesOutput,
) -> Result<Option<Vec<u8>>, TmuxError> {
    let (hdr_line, payload) = split_header_and_payload(out)?;
    if hdr_line == "ERR NOT_FOUND" {
        return Ok(None);
    }
    if let Some(err_code) = hdr_line.strip_prefix("ERR ") {
        return Err(map_remote_err_code(err_code.trim(), out));
    }
    if !out.success() {
        return Err(TmuxError::Command {
            args: "remote_mirror agy hydrate".to_string(),
            code: out.exit_code,
            stderr: out.stderr.clone(),
        });
    }
    let max_total = (MAX_AGY_HYDRATE_STEPS * (MAX_AGY_HYDRATE_STEP_BYTES + 64))
        .min(MAX_DECOMPRESSED_CHUNK_BYTES);
    match hdr_line {
        "HDR RAW" => {
            if payload.len() > max_total {
                return Err(TmuxError::Parse(format!(
                    "hydrated AGY payload ({} bytes) exceeded cap ({max_total} bytes)",
                    payload.len()
                )));
            }
            Ok(Some(payload.to_vec()))
        }
        "HDR GZ1" => {
            if payload.is_empty() {
                return Ok(Some(Vec::new()));
            }
            let decoded =
                decompress_gzip_bounded_untyped(payload, max_total).map_err(TmuxError::Parse)?;
            Ok(Some(decoded))
        }
        other => Err(TmuxError::Parse(format!(
            "unexpected AGY hydrate header: {other:?}"
        ))),
    }
}

async fn parse_fetch_response_async(
    out: starfane_tmux::ExecBytesOutput,
) -> Result<(FetchHeader, Vec<u8>), TmuxError> {
    let is_gz = split_header_and_payload(&out)
        .ok()
        .is_some_and(|(hdr, _)| hdr.contains(" GZ1 "));
    if is_gz {
        tokio::task::spawn_blocking(move || parse_fetch_response(&out))
            .await
            .map_err(|e| TmuxError::Io {
                what: "mirror gzip decode join".into(),
                source: std::io::Error::other(e.to_string()),
            })?
    } else {
        parse_fetch_response(&out)
    }
}

async fn parse_range_fetch_response_async(
    out: starfane_tmux::ExecBytesOutput,
) -> Result<(RangeFetchOutcome, Vec<u8>), TmuxError> {
    let is_gz = split_header_and_payload(&out)
        .ok()
        .is_some_and(|(hdr, _)| hdr.contains(" GZ1 "));
    if is_gz {
        tokio::task::spawn_blocking(move || parse_range_fetch_response(&out))
            .await
            .map_err(|e| TmuxError::Io {
                what: "mirror range gzip decode join".into(),
                source: std::io::Error::other(e.to_string()),
            })?
    } else {
        parse_range_fetch_response(&out)
    }
}

async fn parse_hydrate_response_async(
    out: starfane_tmux::ExecBytesOutput,
) -> Result<Option<Vec<u8>>, TmuxError> {
    let is_gz = split_header_and_payload(&out)
        .ok()
        .is_some_and(|(hdr, _)| hdr == "HDR GZ1");
    if is_gz {
        tokio::task::spawn_blocking(move || parse_hydrate_response(&out))
            .await
            .map_err(|e| TmuxError::Io {
                what: "mirror hydrate gzip decode join".into(),
                source: std::io::Error::other(e.to_string()),
            })?
    } else {
        parse_hydrate_response(&out)
    }
}

fn split_header_and_payload(
    out: &starfane_tmux::ExecBytesOutput,
) -> Result<(&str, &[u8]), TmuxError> {
    let Some(nl) = out.stdout.iter().position(|&b| b == b'\n') else {
        if !out.success() {
            return Err(TmuxError::Command {
                args: "remote_mirror".to_string(),
                code: out.exit_code,
                stderr: out.stderr.clone(),
            });
        }
        return Err(TmuxError::Parse(
            "remote mirror response missing header newline".to_string(),
        ));
    };
    let raw_hdr = out.stdout[..nl]
        .strip_suffix(b"\r")
        .unwrap_or(&out.stdout[..nl]);
    let hdr_str = std::str::from_utf8(raw_hdr)
        .map_err(|e| TmuxError::Parse(format!("non-UTF8 mirror header: {e}")))?;
    Ok((hdr_str, &out.stdout[nl + 1..]))
}

fn map_remote_err_code(code: &str, out: &starfane_tmux::ExecBytesOutput) -> TmuxError {
    match code {
        "SYMLINK" => TmuxError::InvalidRequest("remote transcript path is a symlink".to_string()),
        "NOT_FOUND" => TmuxError::Command {
            args: "remote_mirror".to_string(),
            code: out.exit_code.or(Some(44)),
            stderr: "remote transcript file not found".to_string(),
        },
        other => TmuxError::Command {
            args: "remote_mirror".to_string(),
            code: out.exit_code,
            stderr: format!("remote mirror error: {other}"),
        },
    }
}

fn parse_stat_triplet(s: &str) -> Result<RemoteFileStat, TmuxError> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(TmuxError::Parse(format!("malformed stat triplet: {s:?}")));
    }
    let ino = parts[0]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid ino in {s:?}")))?;
    let size = parts[1]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid size in {s:?}")))?;
    let mtime_s = parts[2]
        .parse::<i64>()
        .map_err(|_| TmuxError::Parse(format!("invalid mtime in {s:?}")))?;
    Ok(RemoteFileStat { ino, size, mtime_s })
}

fn parse_fetch_header_line(hdr_line: &str) -> Result<FetchHeader, TmuxError> {
    let parts: Vec<&str> = hdr_line.split_whitespace().collect();
    if parts.len() != 8 || parts[0] != "HDR" {
        return Err(TmuxError::Parse(format!(
            "unexpected remote mirror header: {hdr_line:?}"
        )));
    }
    let ino = parts[1]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid ino: {hdr_line:?}")))?;
    let size = parts[2]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid size: {hdr_line:?}")))?;
    let mtime_s = parts[3]
        .parse::<i64>()
        .map_err(|_| TmuxError::Parse(format!("invalid mtime: {hdr_line:?}")))?;
    let offset = parts[4]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid offset: {hdr_line:?}")))?;
    let raw_len = parts[5]
        .parse::<u64>()
        .map_err(|_| TmuxError::Parse(format!("invalid raw_len: {hdr_line:?}")))?;
    let mode = match parts[6] {
        "RAW" => TransferMode::Raw,
        "GZ1" => TransferMode::Gz1,
        other => {
            return Err(TmuxError::Parse(format!(
                "unsupported transfer mode {other:?}"
            )));
        }
    };
    let reset = match parts[7] {
        "0" => false,
        "1" => true,
        other => {
            return Err(TmuxError::Parse(format!("invalid reset flag {other:?}")));
        }
    };
    Ok(FetchHeader {
        stat: RemoteFileStat { ino, size, mtime_s },
        offset,
        raw_len,
        mode,
        reset,
    })
}

fn decode_transfer_payload(
    mode: TransferMode,
    payload: &[u8],
    raw_len: u64,
) -> Result<Vec<u8>, TmuxError> {
    let expected = usize::try_from(raw_len)
        .map_err(|_| TmuxError::Parse(format!("raw_len {raw_len} overflows platform usize")))?;
    if expected > MAX_DECOMPRESSED_CHUNK_BYTES {
        return Err(TmuxError::Parse(format!(
            "raw_len {expected} exceeds zip-bomb cap {MAX_DECOMPRESSED_CHUNK_BYTES}"
        )));
    }
    if expected == 0 {
        if !payload.is_empty() {
            return Err(TmuxError::Parse(format!(
                "expected empty payload for raw_len=0, got {} bytes",
                payload.len()
            )));
        }
        return Ok(Vec::new());
    }
    match mode {
        TransferMode::Raw => {
            if payload.len() != expected {
                return Err(TmuxError::Parse(format!(
                    "RAW payload length mismatch: got {}, expected {expected}",
                    payload.len()
                )));
            }
            Ok(payload.to_vec())
        }
        TransferMode::Gz1 => {
            decompress_gzip_bounded(payload, expected, MAX_DECOMPRESSED_CHUNK_BYTES)
                .map_err(TmuxError::Parse)
        }
    }
}

/// Decompress a `gzip` payload using local `gzip -dc` with a strict `max_bytes` zip-bomb cap
/// and exact `decompressed.len() == expected_len` verification.
pub(crate) fn decompress_gzip_bounded(
    compressed: &[u8],
    expected_len: usize,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if expected_len > max_bytes {
        return Err(format!(
            "expected decompressed length {expected_len} exceeds zip-bomb cap {max_bytes}"
        ));
    }
    let out = decompress_gzip_bounded_untyped(compressed, max_bytes)?;
    if out.len() != expected_len {
        return Err(format!(
            "decompressed length mismatch: got {}, expected {expected_len}",
            out.len()
        ));
    }
    Ok(out)
}

fn decompress_gzip_bounded_untyped(compressed: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut child = Command::new("gzip")
        .arg("-dc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn gzip -dc: {e}"))?;

    let stdin_pipe = child
        .stdin
        .take()
        .ok_or_else(|| "gzip stdin pipe unavailable".to_string())?;
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "gzip stdout pipe unavailable".to_string())?;
    let stderr_pipe = child.stderr.take();

    let input = compressed.to_vec();
    let writer = thread::spawn(move || -> std::io::Result<()> {
        let mut pipe = stdin_pipe;
        pipe.write_all(&input)?;
        pipe.flush()
    });
    let err_reader = thread::spawn(move || -> String {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe {
            let _ = p.take(4096).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut out = Vec::new();
    let cap_plus_one = (max_bytes as u64).saturating_add(1);
    let read_res = stdout_pipe.take(cap_plus_one).read_to_end(&mut out);

    if out.len() > max_bytes {
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let _ = err_reader.join();
        return Err(format!(
            "gzip output exceeded zip-bomb cap of {max_bytes} bytes"
        ));
    }

    if let Err(e) = read_res {
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let _ = err_reader.join();
        return Err(format!("read gzip stdout: {e}"));
    }

    let status = child.wait().map_err(|e| format!("wait gzip -dc: {e}"))?;
    let stderr_text = err_reader.join().unwrap_or_default();

    if let Ok(Err(e)) = writer.join() {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(format!("write gzip stdin: {e}"));
        }
    }
    if !status.success() {
        return Err(format!(
            "gzip -dc failed (exit {:?}): {}",
            status.code(),
            stderr_text.trim()
        ));
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Local cache filesystem, alignment, and path validation helpers
// ---------------------------------------------------------------------------

fn align_tail_slice(bytes: &[u8], slice_start: u64) -> (&[u8], u64) {
    match bytes.iter().position(|&b| b == b'\n') {
        Some(nl) => {
            let discarded = (nl + 1) as u64;
            (&bytes[nl + 1..], slice_start.saturating_add(discarded))
        }
        None => (&[], slice_start.saturating_add(bytes.len() as u64)),
    }
}

fn load_valid_meta(local_path: &Path, meta_path: &Path, remote_path: &str) -> Option<MirrorMeta> {
    let text = fs::read_to_string(meta_path).ok()?;
    let meta: MirrorMeta = serde_json::from_str(&text).ok()?;
    if meta.remote_path != remote_path {
        return None;
    }
    if meta.base_offset > meta.synced_offset {
        return None;
    }
    let file_meta = fs::metadata(local_path).ok()?;
    if !file_meta.is_file() {
        return None;
    }
    let expected_len = meta.synced_offset.saturating_sub(meta.base_offset);
    if file_meta.len() != expected_len {
        return None;
    }
    Some(meta)
}

fn save_meta(meta_path: &Path, meta: &MirrorMeta) -> Result<(), TmuxError> {
    let bytes = serde_json::to_vec(meta)
        .map_err(|e| TmuxError::Parse(format!("serialize mirror meta: {e}")))?;
    atomic_write_private(meta_path, &bytes).map_err(|e| TmuxError::Io {
        what: format!("write mirror meta {}", meta_path.display()),
        source: e,
    })
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Enforce 0700 on leaf and parent cache directories (up to remote-cache).
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        if let Some(parent) = dir.parent() {
            if parent
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "remote-cache")
            {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            } else if let Some(grand) = parent.parent() {
                if grand
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "remote-cache")
                {
                    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
                    let _ = fs::set_permissions(grand, fs::Permissions::from_mode(0o700));
                }
            }
        }
    }
    Ok(())
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

fn make_tmp_path(dest: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mirror");
    dest.with_file_name(format!(".{file_name}.tmp.{}.{seq}", std::process::id()))
}

fn create_private_tmp(tmp_path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(tmp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(tmp_path, fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = make_tmp_path(path);
    let res = (|| -> std::io::Result<()> {
        let mut f = create_private_tmp(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        drop(f);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn atomic_stitch_prefix_private(local_path: &Path, prefix_bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = local_path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = make_tmp_path(local_path);
    let res = (|| -> std::io::Result<()> {
        let mut out = create_private_tmp(&tmp)?;
        out.write_all(prefix_bytes)?;
        let mut tail = File::open(local_path)?;
        std::io::copy(&mut tail, &mut out)?;
        out.flush()?;
        drop(out);
        fs::rename(&tmp, local_path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn scan_local_step_indices(local_full_path: &Path) -> HashSet<u64> {
    let mut out = HashSet::new();
    let Ok(file) = File::open(local_full_path) else {
        return out;
    };
    let mut reader = BufReader::with_capacity(32 * 1024, file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(idx) = fast_extract_step_index(&line) {
                    out.insert(idx);
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn remote_sibling_transcript_full(remote_compact_path: &str) -> Option<String> {
    let prefix = remote_compact_path.strip_suffix("/transcript.jsonl")?;
    Some(format!("{prefix}/transcript_full.jsonl"))
}

fn extract_agy_conv_id(remote_path: &str) -> Option<&str> {
    let marker = "/.gemini/antigravity-cli/brain/";
    let pos = remote_path.find(marker)?;
    let rest = &remote_path[pos + marker.len()..];
    let seg = rest.split('/').next()?;
    is_safe_segment(seg).then_some(seg)
}

fn has_traversal_segment(path: &str) -> bool {
    path.split('/').any(|seg| seg == ".." || seg == ".")
}

fn is_safe_segment(seg: &str) -> bool {
    !seg.is_empty() && seg != "." && seg != ".." && !seg.contains('/') && !seg.contains('\\')
}

fn is_valid_relative_jsonl(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let segs: Vec<&str> = rel.split('/').collect();
    if segs.is_empty() || !segs.iter().all(|s| is_safe_segment(s)) {
        return false;
    }
    let Some(last) = segs.last() else {
        return false;
    };
    last.ends_with(".jsonl") && last.len() > ".jsonl".len()
}

fn is_agy_transcript_filename(name: &str) -> bool {
    name == "transcript.jsonl" || name == "transcript_full.jsonl"
}

fn sanitize_cache_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        fnv128_hex(raw.as_bytes())
    } else {
        cleaned
    }
}

/// Deterministic 128-bit FNV-1a lowercase hex digest (zero external crates).
fn fnv128_hex(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u128::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:032x}")
}

fn lock_mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::transcript::TranscriptRole;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    static TEST_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_test_dir(tag: &str) -> PathBuf {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sf-remote-mirror-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_executable(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    struct FakeSshFixture {
        root: PathBuf,
        state_dir: PathBuf,
        remote_home: PathBuf,
        calls_log: PathBuf,
        bytes_log: PathBuf,
        ssh: SshExec,
    }

    impl FakeSshFixture {
        fn new(tag: &str) -> Self {
            let root = temp_test_dir(tag);
            let state_dir = root.join("state");
            let remote_home = root.join("remote_home");
            fs::create_dir_all(&state_dir).unwrap();
            fs::create_dir_all(&remote_home).unwrap();
            let calls_log = root.join("ssh_calls.log");
            let bytes_log = root.join("ssh_bytes.log");
            let fake_ssh = root.join("fake-ssh");
            let sock = root.join("cm.sock");

            // Fake SSH wrapper:
            // - logs each invocation to `calls_log`
            // - isolates HOME to `remote_home`
            // - captures stdout to a temp file to log exact wire byte count & header line, then outputs raw bytes
            write_executable(
                &fake_ssh,
                &format!(
                    r#"#!/bin/sh
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  exit 0
fi
printf 'CALL %s\n' "$*" >> "{calls}"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then
    shift
    break
  fi
  shift
done
TMP_OUT="{root}/out_$$.bin"
HOME="{rhome}" /bin/sh -c "$1" > "$TMP_OUT"
RC=$?
SZ=$(wc -c < "$TMP_OUT" | tr -d ' ')
HDR=$(head -n 1 "$TMP_OUT" | tr -d '\r')
printf '%s %s\n' "$SZ" "$HDR" >> "{bytes}"
cat "$TMP_OUT"
rm -f "$TMP_OUT"
exit $RC
"#,
                    calls = calls_log.display(),
                    root = root.display(),
                    rhome = remote_home.display(),
                    bytes = bytes_log.display(),
                ),
            );

            let ssh = SshExec::new("test@remote-gpu", sock)
                .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()))
                .with_timeout(Duration::from_secs(10));

            Self {
                root,
                state_dir,
                remote_home,
                calls_log,
                bytes_log,
                ssh,
            }
        }

        fn call_count(&self) -> usize {
            fs::read_to_string(&self.calls_log)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }

        fn byte_logs(&self) -> Vec<(usize, String)> {
            fs::read_to_string(&self.bytes_log)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let (sz, hdr) = l.split_once(' ')?;
                    Some((sz.parse::<usize>().ok()?, hdr.to_string()))
                })
                .collect()
        }

        fn stat_remote(&self, path: &Path) -> RemoteFileStat {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = fs::metadata(path).unwrap();
                RemoteFileStat {
                    ino: meta.ino(),
                    size: meta.len(),
                    mtime_s: meta.mtime(),
                }
            }
            #[cfg(not(unix))]
            {
                let meta = fs::metadata(path).unwrap();
                RemoteFileStat {
                    ino: 1,
                    size: meta.len(),
                    mtime_s: 0,
                }
            }
        }
    }

    impl Drop for FakeSshFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn make_claude_turn_pair(idx: usize, padding: &str) -> String {
        let sec = idx % 60;
        let min = (idx / 60) % 60;
        let hour = (idx / 3600) % 24;
        let next_sec = (sec + 2) % 60;
        let next_min = min + if sec + 2 >= 60 { 1 } else { 0 };
        format!(
            concat!(
                r#"{{"type":"user","timestamp":"2026-11-25T{h:02}:{m:02}:{s:02}.000Z","message":{{"role":"user","content":"turn-user-{idx} {pad}"}}}}"#,
                "\n",
                r#"{{"type":"assistant","timestamp":"2026-11-25T{h:02}:{nm:02}:{ns:02}.000Z","message":{{"id":"msg_{idx}","model":"claude-opus-5-5","role":"assistant","stop_reason":"end_turn","usage":{{"input_tokens":10,"cache_read_input_tokens":100000,"cache_creation_input_tokens":5000,"output_tokens":200}},"content":[{{"type":"text","text":"turn-assistant-{idx} {pad}"}}]}}}}"#,
                "\n"
            ),
            h = hour,
            m = min,
            s = sec,
            nm = next_min % 60,
            ns = next_sec,
            idx = idx,
            pad = padding,
        )
    }

    #[test]
    fn validate_remote_transcript_path_allowlist_and_traversal_rejection() {
        let home = "/home/ubuntu";

        // Valid Claude (root + subagent)
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.claude/projects/-home-ubuntu-repo/sid-123.jsonl"
        )
        .is_ok());
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.claude/projects/-home-ubuntu-repo/root/subagents/agent-a1.jsonl"
        )
        .is_ok());
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.claude/projects/flat.jsonl"
        )
        .is_ok());

        // Valid Codex
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.codex/sessions/2026/11/25/rollout-2026-11-25T10-00-00-01a1.jsonl"
        )
        .is_ok());

        // Valid AGY (both canonical `.system_generated/logs/` and direct `brain/<id>/`)
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.gemini/antigravity-cli/brain/conv-abc/.system_generated/logs/transcript.jsonl"
        )
        .is_ok());
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.gemini/antigravity-cli/brain/conv-abc/.system_generated/logs/transcript_full.jsonl"
        )
        .is_ok());
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            home,
            "/home/ubuntu/.gemini/antigravity-cli/brain/conv-abc/transcript.jsonl"
        )
        .is_ok());

        // Rejected: traversal, control chars, outside home, wrong dirs/files
        for bad in [
            "../../../etc/passwd",
            "/etc/passwd",
            "/home/ubuntu/.claude/projects/../../../etc/passwd",
            "/home/ubuntu/.claude/projects/repo/../secret.jsonl",
            "/home/ubuntu/.claude/projects/./a.jsonl",
            "/home/ubuntu/.claude/projects/.jsonl",
            "/home/ubuntu/.claude/projects/repo/notes.txt",
            "/home/ubuntu/.claude/sessions/123.json",
            "/home/ubuntu/.codex/config.toml",
            "/home/ubuntu/.gemini/antigravity-cli/brain/conv-abc/artifacts/plan.md",
            "/home/ubuntu/.gemini/antigravity-cli/brain/conv-abc/other.jsonl",
            "/home/ubuntu/.claude/projects/repo/a.jsonl\nrm -rf /",
            "/home/ubuntu/.claude/projects/repo/a.jsonl\r",
            "/home/ubuntu/.claude/projects/repo/a.jsonl\0",
            "/home/other/.claude/projects/repo/a.jsonl",
            "",
        ] {
            assert!(
                RemoteTranscriptMirror::validate_remote_transcript_path(home, bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            "/",
            "/.claude/projects/a.jsonl"
        )
        .is_err());
        assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
            "/home/../root",
            "/home/../root/.claude/projects/a.jsonl"
        )
        .is_err());
    }

    #[test]
    fn symlink_rejected_by_remote_posix_guard() {
        let fx = FakeSshFixture::new("symlink");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());
        let secret = fx.root.join("secret.txt");
        fs::write(&secret, "top-secret-data\n").unwrap();

        let cl_dir = fx.remote_home.join(".claude/projects/proj");
        fs::create_dir_all(&cl_dir).unwrap();
        let sym = cl_dir.join("evil.jsonl");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &sym).unwrap();

        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = sym.to_string_lossy().into_owned();
        let err = mirror
            .ensure_fresh(
                &fx.ssh,
                "gpu-1",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                false,
            )
            .unwrap_err();
        assert!(
            matches!(err, TmuxError::InvalidRequest(ref m) if m.contains("symlink")),
            "expected symlink rejection, got: {err:?}"
        );
        assert!(
            !mirror
                .local_mirror_path("gpu-1", AgentKind::Claude, &rpath)
                .exists(),
            "local mirror must not be created for rejected symlink"
        );

        // Also test symlinked AGY `transcript_full.jsonl`.
        let agy_dir = fx
            .remote_home
            .join(".gemini/antigravity-cli/brain/conv-sym/.system_generated/logs");
        fs::create_dir_all(&agy_dir).unwrap();
        let compact = agy_dir.join("transcript.jsonl");
        fs::write(&compact, "{}\n").unwrap();
        let full_sym = agy_dir.join("transcript_full.jsonl");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &full_sym).unwrap();
        let agy_err = mirror
            .hydrate_remote_agy_steps(&fx.ssh, "gpu-1", &rhome, &compact.to_string_lossy(), &[1])
            .unwrap_err();
        assert!(
            matches!(agy_err, TmuxError::InvalidRequest(ref m) if m.contains("symlink")),
            "expected symlink rejection for AGY full transcript, got: {agy_err:?}"
        );
    }

    #[test]
    fn cold_tail_window_fast_path_gz1_delta_and_seamless_prefix_backfill() {
        let fx = FakeSshFixture::new("cold-delta-backfill");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());
        let cache = Mutex::new(TranscriptIndexCache::default());

        let cl_dir = fx.remote_home.join(".claude/projects/-home-ubuntu-work");
        fs::create_dir_all(&cl_dir).unwrap();
        let remote_file = cl_dir.join("big-session.jsonl");

        // Build ~3.1 MiB (> INITIAL_TAIL_WINDOW = 2 MiB) of deterministic Claude turns.
        let pad = "a".repeat(900);
        let mut initial_content = String::with_capacity(3_300_000);
        let mut pair_count = 0usize;
        while initial_content.len() < 3_150_000 {
            initial_content.push_str(&make_claude_turn_pair(pair_count, &pad));
            pair_count += 1;
        }
        fs::write(&remote_file, &initial_content).unwrap();

        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = remote_file.to_string_lossy().into_owned();
        let stat1 = fx.stat_remote(&remote_file);
        assert!(stat1.size > INITIAL_TAIL_WINDOW);

        // 1. Cold-start tail window (2 MiB): single compressed SSH call, `\n`-aligned base_offset > 0.
        let p1 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "remote-gpu",
                &rhome,
                AgentKind::Claude,
                &rpath,
                Some(stat1),
                TranscriptQuery::tail(60),
                None,
                1_795_600_000_000,
            )
            .unwrap();
        assert!(p1.sync.changed);
        assert!(
            p1.sync.base_offset > 0,
            "expected cold tail window base_offset > 0"
        );
        assert_eq!(p1.sync.synced_offset, stat1.size);
        assert_eq!(fx.call_count(), 1);
        let logs = fx.byte_logs();
        assert!(
            logs[0].1.contains(" GZ1 1"),
            "expected GZ1 cold seed: {:?}",
            logs[0]
        );
        assert!(
            logs[0].0 < INITIAL_TAIL_WINDOW as usize / 2,
            "gzipped cold window should compress sharply (got {} bytes)",
            logs[0].0
        );

        // First byte of mirrored local file must be clean JSONL (`{`), proving partial leading line discarded.
        let local_bytes_1 = fs::read(&p1.sync.local_path).unwrap();
        assert_eq!(local_bytes_1.first(), Some(&b'{'));
        assert_eq!(
            local_bytes_1.len() as u64,
            p1.sync.synced_offset - p1.sync.base_offset
        );
        assert_eq!(p1.response.source, "transcript");
        assert_eq!(p1.response.transcript_path.as_deref(), Some(rpath.as_str()));
        assert_eq!(p1.response.messages.len(), 60);
        assert!(p1.response.has_more);
        assert_eq!(p1.model.as_deref(), Some("claude-opus-5-5"));
        let m1 = p1.response.metrics.as_ref().expect("metrics extracted");
        assert!(m1.context_used_pct.is_some());
        assert!(m1.tps.is_some());

        // Verify 0700 directory & 0600 file permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(p1.sync.local_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let file_mode = fs::metadata(&p1.sync.local_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        // 2. Unchanged fast-path: 0 SSH calls, 0 bytes transferred!
        let p2 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "remote-gpu",
                &rhome,
                AgentKind::Claude,
                &rpath,
                Some(stat1),
                TranscriptQuery::tail(60),
                None,
                1_795_600_005_000,
            )
            .unwrap();
        assert!(!p2.sync.changed);
        assert_eq!(
            fx.call_count(),
            1,
            "unchanged stat must perform 0 SSH calls"
        );
        assert_eq!(p2.response.messages.len(), 60);

        // 3. Append ~24 KiB (> 16 KiB gzip threshold) -> 1 compressed GZ1 delta call!
        let mut delta = String::new();
        let delta_start_idx = pair_count;
        while delta.len() < 24 * 1024 {
            delta.push_str(&make_claude_turn_pair(pair_count, &pad));
            pair_count += 1;
        }
        {
            let mut f = OpenOptions::new().append(true).open(&remote_file).unwrap();
            f.write_all(delta.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        let stat2 = fx.stat_remote(&remote_file);
        assert_eq!(stat2.size, stat1.size + delta.len() as u64);

        let p3 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "remote-gpu",
                &rhome,
                AgentKind::Claude,
                &rpath,
                Some(stat2),
                TranscriptQuery::tail(60),
                None,
                1_795_600_010_000,
            )
            .unwrap();
        assert!(p3.sync.changed);
        assert_eq!(p3.sync.base_offset, p1.sync.base_offset);
        assert_eq!(p3.sync.synced_offset, stat2.size);
        assert_eq!(
            fx.call_count(),
            2,
            "delta sync must take exactly 1 SSH call"
        );
        let logs2 = fx.byte_logs();
        assert!(
            logs2[1].1.contains(" GZ1 0"),
            "expected GZ1 non-reset delta: {:?}",
            logs2[1]
        );
        assert!(
            logs2[1].0 < delta.len(),
            "GZ1 delta ({}) must be smaller than raw delta ({})",
            logs2[1].0,
            delta.len()
        );
        let newest = p3.response.messages.last().unwrap();
        assert!(newest
            .text
            .contains(&format!("turn-assistant-{}", pair_count - 1)));
        assert!(delta_start_idx < pair_count);

        // 4. Upward pagination reaching start of mirrored tail window (`before = 15, limit = 40`):
        //    automatically triggers 1-RTT prefix backfill, stitches exact full file, and shifts cursor seamlessly!
        let tail_total_before_backfill = p3.response.total_messages;
        let first_tail_msg = {
            let mut c = lock_mutex(&cache);
            c.get_page_or_sync(
                AgentKind::Claude,
                &p3.sync.local_path,
                TranscriptQuery {
                    limit: 15,
                    before: Some(15),
                    all: false,
                },
                None,
                0,
            )
            .unwrap()
            .messages
        };
        assert_eq!(first_tail_msg.len(), 15);

        let p4 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "remote-gpu",
                &rhome,
                AgentKind::Claude,
                &rpath,
                Some(stat2),
                TranscriptQuery {
                    limit: 40,
                    before: Some(15),
                    all: false,
                },
                None,
                1_795_600_020_000,
            )
            .unwrap();
        assert_eq!(
            p4.sync.base_offset, 0,
            "prefix backfill must set base_offset == 0"
        );
        assert_eq!(p4.sync.synced_offset, stat2.size);
        assert_eq!(fx.call_count(), 3, "backfill must take exactly 1 SSH RTT");

        // Stitched local file must be byte-for-byte identical to the entire remote file!
        let full_remote_bytes = fs::read(&remote_file).unwrap();
        let full_local_bytes = fs::read(&p4.sync.local_path).unwrap();
        assert_eq!(
            full_local_bytes, full_remote_bytes,
            "stitched mirror must match remote file byte-for-byte"
        );

        // Returned 40 messages must end with the exact 15 tail messages preceded by 25 prefix messages!
        assert_eq!(p4.response.messages.len(), 40);
        assert_eq!(&p4.response.messages[25..], &first_tail_msg[..]);
        assert!(p4.response.total_messages > tail_total_before_backfill);
        assert_eq!(p4.response.total_messages, pair_count * 2);

        // 5. `?all=1` with unchanged stat now executes 0 SSH calls!
        let p5 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "remote-gpu",
                &rhome,
                AgentKind::Claude,
                &rpath,
                Some(stat2),
                TranscriptQuery {
                    limit: MAX_TRANSCRIPT_LIMIT,
                    before: None,
                    all: true,
                },
                None,
                1_795_600_030_000,
            )
            .unwrap();
        assert_eq!(
            fx.call_count(),
            3,
            "already backfilled ?all=1 must do 0 SSH calls"
        );
        assert!(!p5.response.has_more);
        assert_eq!(p5.response.messages.len(), pair_count * 2);
        assert!(p5.response.messages[0].text.starts_with("turn-user-0 "));
    }

    #[test]
    fn truncation_and_inode_rotation_reset_cleanly() {
        let fx = FakeSshFixture::new("rotate-trunc");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());

        let cx_dir = fx.remote_home.join(".codex/sessions/2026/11/25");
        fs::create_dir_all(&cx_dir).unwrap();
        let remote_file = cx_dir.join("rollout-test-01a1.jsonl");
        let original = concat!(
            r#"{"type":"session_meta","timestamp":"2026-11-25T01:00:00Z","payload":{"id":"s1","cwd":"/home/u"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-11-25T01:00:01Z","payload":{"type":"user_message","message":"original long prompt 0123456789"}}"#,
            "\n"
        );
        fs::write(&remote_file, original).unwrap();
        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = remote_file.to_string_lossy().into_owned();

        let s1 = mirror
            .ensure_fresh(&fx.ssh, "h1", &rhome, AgentKind::Codex, &rpath, None, false)
            .unwrap();
        assert!(s1.changed);
        assert_eq!(fs::read_to_string(&s1.local_path).unwrap(), original);

        // 1. Truncate file in-place (same inode, smaller size).
        let truncated = r#"{"type":"session_meta","payload":{"id":"s1"}}"#;
        let truncated_nl = format!("{truncated}\n");
        fs::write(&remote_file, &truncated_nl).unwrap();
        let s2 = mirror
            .ensure_fresh(&fx.ssh, "h1", &rhome, AgentKind::Codex, &rpath, None, false)
            .unwrap();
        assert!(s2.changed);
        assert_eq!(s2.base_offset, 0);
        assert_eq!(s2.synced_offset, truncated_nl.len() as u64);
        assert_eq!(fs::read_to_string(&s2.local_path).unwrap(), truncated_nl);

        // 2. Rotate inode via temp file + rename (new inode, larger size).
        let rotated = concat!(
            r#"{"type":"session_meta","payload":{"id":"s2-rotated"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"fresh file after rotation!"}}"#,
            "\n"
        );
        let tmp = cx_dir.join("rollout-test-01a1.jsonl.new");
        fs::write(&tmp, rotated).unwrap();
        fs::rename(&tmp, &remote_file).unwrap();

        let s3 = mirror
            .ensure_fresh(&fx.ssh, "h1", &rhome, AgentKind::Codex, &rpath, None, false)
            .unwrap();
        assert!(s3.changed);
        assert_eq!(s3.base_offset, 0);
        assert_eq!(s3.synced_offset, rotated.len() as u64);
        assert_eq!(fs::read_to_string(&s3.local_path).unwrap(), rotated);
    }

    #[test]
    fn gzip_zip_bomb_cap_and_length_mismatch_rejected() {
        // Generate a real gzip zip bomb: 20 MiB of zero bytes compresses down to ~20 KiB.
        let mut child = Command::new("gzip")
            .args(["-1", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn gzip");
        let mut stdin = child.stdin.take().unwrap();
        let writer = thread::spawn(move || {
            let chunk = vec![0u8; 1024 * 1024];
            for _ in 0..20 {
                stdin.write_all(&chunk).unwrap();
            }
        });
        let mut bomb = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut bomb).unwrap();
        writer.join().unwrap();
        assert!(child.wait().unwrap().success());
        assert!(
            bomb.len() < 100_000,
            "20 MiB zeros should compress tightly (got {})",
            bomb.len()
        );

        // Direct unit test: zip-bomb triggers strict 16 MiB cap cleanly.
        let err = decompress_gzip_bounded(&bomb, 20_000, MAX_DECOMPRESSED_CHUNK_BYTES).unwrap_err();
        assert!(
            err.contains("zip-bomb cap"),
            "expected zip-bomb cap error, got: {err}"
        );

        // Length mismatch on valid gzip payload is also rejected.
        let payload = b"hello starfane mirror\n".repeat(1000);
        let mut gz_child = Command::new("gzip")
            .args(["-1", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        gz_child.stdin.take().unwrap().write_all(&payload).unwrap();
        let mut compressed = Vec::new();
        gz_child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut compressed)
            .unwrap();
        assert!(gz_child.wait().unwrap().success());

        assert_eq!(
            decompress_gzip_bounded(&compressed, payload.len(), MAX_DECOMPRESSED_CHUNK_BYTES)
                .unwrap(),
            payload
        );
        let mismatch = decompress_gzip_bounded(
            &compressed,
            payload.len() - 10,
            MAX_DECOMPRESSED_CHUNK_BYTES,
        )
        .unwrap_err();
        assert!(mismatch.contains("mismatch"), "got: {mismatch}");

        // End-to-end test: malicious remote emitting a GZ1 zip bomb via fake-ssh is rejected cleanly.
        let root = temp_test_dir("zipbomb-e2e");
        let bomb_file = root.join("bomb.gz");
        fs::write(&bomb_file, &bomb).unwrap();
        let malicious_ssh = root.join("malicious-ssh");
        write_executable(
            &malicious_ssh,
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then exit 0; fi
printf 'HDR 999 20000 1700000000 0 20000 GZ1 1\n'
cat "{}"
"#,
                bomb_file.display()
            ),
        );
        let ssh = SshExec::new("evil@host", root.join("cm.sock"))
            .with_ssh_bin(Some(malicious_ssh.to_string_lossy().into_owned()));
        let mirror = RemoteTranscriptMirror::new(root.join("state"));
        let rpath = "/home/u/.claude/projects/p/s.jsonl";
        let e2e_err = mirror
            .ensure_fresh(
                &ssh,
                "evil",
                "/home/u",
                AgentKind::Claude,
                rpath,
                None,
                false,
            )
            .unwrap_err();
        assert!(
            matches!(e2e_err, TmuxError::Parse(ref m) if m.contains("zip-bomb cap")),
            "expected Parse zip-bomb error, got: {e2e_err:?}"
        );
        assert!(!mirror
            .local_mirror_path("evil", AgentKind::Claude, rpath)
            .exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agy_selective_step_hydration_and_cache_reuse() {
        let fx = FakeSshFixture::new("agy-hydrate");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());
        let cache = Mutex::new(TranscriptIndexCache::default());

        let conv_id = "81166b09-793d-4380-9ff7-637632489fcf";
        let agy_dir = fx
            .remote_home
            .join(".gemini/antigravity-cli/brain")
            .join(conv_id)
            .join(".system_generated/logs");
        fs::create_dir_all(&agy_dir).unwrap();
        let compact = agy_dir.join("transcript.jsonl");
        let full = agy_dir.join("transcript_full.jsonl");

        let compact_jsonl = concat!(
            r#"{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-25T08:00:00Z","content":"<USER_REQUEST>short preview <truncated 999 bytes></USER_REQUEST>","truncated_fields":["content"]}"#,
            "\n",
            r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:02Z","model":"gemini-4-neon-high","input_tokens":50000,"cache_read_tokens":10000,"output_tokens":300,"content":"truncated body <truncated 4000 bytes>","truncated_fields":["content"]}"#,
            "\n",
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:05Z","tool_calls":[{"name":"exit_plan_mode","args":{"plan":"<truncated 2000 bytes>"}}],"truncated_fields":["tool_calls"]}"#,
            "\n",
            r#"{"step_index":6,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:08Z","content":"giant truncated <truncated 600000 bytes>","truncated_fields":["content"]}"#,
            "\n"
        );
        let giant_600k = "Z".repeat(600 * 1024);
        let full_jsonl = format!(
            concat!(
                r#"{{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-25T08:00:00Z","content":"<USER_REQUEST>FULL HYDRATED USER PROMPT</USER_REQUEST>"}}"#,
                "\n",
                r#"{{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:02Z","content":"FULL HYDRATED ASSISTANT ANSWER"}}"#,
                "\n",
                r#"{{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:05Z","tool_calls":[{{"name":"exit_plan_mode","args":{{"plan":"FULL HYDRATED PLAN BODY"}}}}]}}"#,
                "\n",
                r#"{{"step_index":6,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:08Z","content":"{giant}"}}"#,
                "\n",
                r#"{{"step_index":99,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:00:10Z","content":"UNRELATED STEP 99 NEVER FETCHED"}}"#,
                "\n"
            ),
            giant = giant_600k
        );
        fs::write(&compact, compact_jsonl).unwrap();
        fs::write(&full, full_jsonl).unwrap();

        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = compact.to_string_lossy().into_owned();
        let stat = fx.stat_remote(&compact);

        let res = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "agy-host",
                &rhome,
                AgentKind::Agy,
                &rpath,
                Some(stat),
                TranscriptQuery::tail(60),
                None,
                1_795_600_100_000,
            )
            .unwrap();

        // Check sibling cache layout: `remote-cache/agy-host/agy-<conv_id>/transcript.jsonl`.
        assert!(res
            .sync
            .local_path
            .ends_with(format!("agy-host/agy-{conv_id}/transcript.jsonl")));
        let local_full = res.sync.local_path.with_file_name("transcript_full.jsonl");
        assert!(local_full.is_file());

        // Steps 2, 4, 5 are hydrated; step 6 (>512 KiB) stays compact; step 99 never transferred!
        let local_full_text = fs::read_to_string(&local_full).unwrap();
        assert!(local_full_text.contains("FULL HYDRATED USER PROMPT"));
        assert!(local_full_text.contains("FULL HYDRATED ASSISTANT ANSWER"));
        assert!(local_full_text.contains("FULL HYDRATED PLAN BODY"));
        assert!(
            !local_full_text.contains("UNRELATED STEP 99"),
            "unrelated step must not be fetched"
        );
        assert!(
            !local_full_text.contains(&giant_600k[..1024]),
            ">512 KiB step must be skipped by remote awk cap"
        );

        assert_eq!(res.response.messages.len(), 4);
        assert_eq!(res.response.messages[0].role, TranscriptRole::User);
        assert_eq!(res.response.messages[0].text, "FULL HYDRATED USER PROMPT");
        assert_eq!(
            res.response.messages[1].text,
            "FULL HYDRATED ASSISTANT ANSWER"
        );
        assert_eq!(res.response.messages[2].text, "FULL HYDRATED PLAN BODY");
        assert!(res.response.messages[3].text.contains("giant truncated"));
        assert_eq!(res.model.as_deref(), Some("gemini-4-neon-high"));
        assert_eq!(res.effort.as_deref(), Some("high"));

        // 2 calls total: 1 for compact transcript.jsonl, 1 for selective hydration.
        assert_eq!(fx.call_count(), 2);

        // Already-cached steps require 0 SSH calls when re-invoked directly!
        let already = mirror
            .hydrate_remote_agy_steps(&fx.ssh, "agy-host", &rhome, &rpath, &[2, 4, 5])
            .unwrap();
        assert_eq!(already, 0);
        assert_eq!(fx.call_count(), 2);
    }

    #[test]
    fn concurrent_pulls_on_same_key_serialize_without_corruption() {
        let fx = FakeSshFixture::new("concurrent");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());
        let cl_dir = fx.remote_home.join(".claude/projects/shared");
        fs::create_dir_all(&cl_dir).unwrap();
        let remote_file = cl_dir.join("race.jsonl");
        let mut content = String::new();
        for i in 0..40 {
            content.push_str(&make_claude_turn_pair(i, "concurrent-test-payload"));
        }
        fs::write(&remote_file, &content).unwrap();

        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = remote_file.to_string_lossy().into_owned();
        let stat = fx.stat_remote(&remote_file);

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let m = mirror.clone();
                let s = fx.ssh.clone();
                let rh = rhome.clone();
                let rp = rpath.clone();
                thread::spawn(move || {
                    m.ensure_fresh(
                        &s,
                        "race-host",
                        &rh,
                        AgentKind::Claude,
                        &rp,
                        Some(stat),
                        false,
                    )
                    .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in &results {
            assert_eq!(r.synced_offset, content.len() as u64);
            assert_eq!(r.base_offset, 0);
        }
        assert_eq!(fs::read_to_string(&results[0].local_path).unwrap(), content);
        // Because the 5 waiters acquire the per-key lock after thread 1 finishes, `latest_stat`
        // fast-path avoids redundant transfers so only 1 SSH call happens!
        assert_eq!(fx.call_count(), 1);
    }

    #[test]
    fn parse_sf_stat_line_handles_valid_and_invalid_inputs() {
        let (st, path) = parse_sf_stat_line(
            "123456 98765 1795600000 /home/ubuntu/.claude/projects/my repo/s1.jsonl",
        )
        .unwrap();
        assert_eq!(
            st,
            RemoteFileStat {
                ino: 123456,
                size: 98765,
                mtime_s: 1_795_600_000,
            }
        );
        assert_eq!(path, "/home/ubuntu/.claude/projects/my repo/s1.jsonl");
        assert!(parse_sf_stat_line("0 -1 0 /missing/file.jsonl").is_none());
        assert!(parse_sf_stat_line("").is_none());
        assert!(parse_sf_stat_line("bad line").is_none());
    }

    #[test]
    fn half_written_trailing_line_resumes_cleanly_on_next_raw_delta() {
        let fx = FakeSshFixture::new("half-line");
        let mirror = RemoteTranscriptMirror::new(fx.state_dir.clone());
        let cache = Mutex::new(TranscriptIndexCache::default());

        let cl_dir = fx.remote_home.join(".claude/projects/half");
        fs::create_dir_all(&cl_dir).unwrap();
        let remote_file = cl_dir.join("stream.jsonl");

        let line1 = r#"{"type":"user","timestamp":"2026-11-25T08:00:00Z","message":{"role":"user","content":"first complete prompt"}}"#;
        let line2_full = r#"{"type":"assistant","timestamp":"2026-11-25T08:00:02Z","message":{"id":"m2","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"second streamed answer"}]}}"#;
        let split_at = line2_full.len() / 2;

        // Initial write has complete line 1 + half-written line 2 (no newline yet).
        fs::write(
            &remote_file,
            format!("{line1}\n{}", &line2_full[..split_at]),
        )
        .unwrap();
        let rhome = fx.remote_home.to_string_lossy().into_owned();
        let rpath = remote_file.to_string_lossy().into_owned();

        let p1 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "h-half",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                TranscriptQuery::tail(20),
                None,
                1000,
            )
            .unwrap();
        assert_eq!(p1.response.messages.len(), 1);
        assert_eq!(p1.response.messages[0].text, "first complete prompt");

        // Flush remaining bytes of line 2 + `\n` (< 16 KiB -> RAW delta append).
        {
            let mut f = OpenOptions::new().append(true).open(&remote_file).unwrap();
            f.write_all(format!("{}\n", &line2_full[split_at..]).as_bytes())
                .unwrap();
        }
        let p2 = mirror
            .read_page_and_metrics(
                &fx.ssh,
                &cache,
                "h-half",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                TranscriptQuery::tail(20),
                None,
                2000,
            )
            .unwrap();
        assert_eq!(p2.response.messages.len(), 2);
        assert_eq!(p2.response.messages[1].text, "second streamed answer");
        let logs = fx.byte_logs();
        assert!(
            logs[1].1.contains(" RAW 0"),
            "expected RAW delta: {:?}",
            logs[1]
        );
    }

    #[test]
    fn ingest_inline_delta_cold_seed_append_and_mismatch_guards() {
        let dir = temp_test_dir("inline-delta");
        let mirror = RemoteTranscriptMirror::new(dir.join("state"));
        let rhome = "/home/demo";
        let rpath = "/home/demo/.claude/projects/-home-demo-app/s1.jsonl";

        let line1 = concat!(
            r#"{"type":"user","timestamp":"2026-11-25T10:00:00Z","message":{"role":"user","content":"inline 1"}}"#,
            "\n"
        );
        let line2 = concat!(
            r#"{"type":"assistant","timestamp":"2026-11-25T10:00:02Z","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"inline 2"}]}}"#,
            "\n"
        );

        // 1. Cold-seed small transcript inline (`base_offset_expected == 0`, 0 SSH calls).
        let stat1 = RemoteFileStat {
            ino: 42,
            size: line1.len() as u64,
            mtime_s: 1_760_000_001,
        };
        let res1 = mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                rpath,
                stat1,
                0,
                line1.as_bytes(),
            )
            .unwrap();
        assert!(res1.changed);
        assert_eq!(res1.base_offset, 0);
        assert_eq!(res1.synced_offset, line1.len() as u64);
        assert_eq!(
            mirror.cached_sync_cursor("host-a", AgentKind::Claude, rpath),
            Some((42, line1.len() as u64))
        );

        // 2. Incremental append matching `(ino, synced_offset)`.
        let stat2 = RemoteFileStat {
            ino: 42,
            size: (line1.len() + line2.len()) as u64,
            mtime_s: 1_760_000_005,
        };
        let res2 = mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                rpath,
                stat2,
                line1.len() as u64,
                line2.as_bytes(),
            )
            .unwrap();
        assert!(res2.changed);
        assert_eq!(res2.synced_offset, (line1.len() + line2.len()) as u64);
        assert_eq!(
            fs::read_to_string(&res2.local_path).unwrap(),
            format!("{line1}{line2}")
        );

        // 3. Mismatched inode or base offset is rejected cleanly (never corrupts local file).
        assert!(mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                rpath,
                RemoteFileStat {
                    ino: 99,
                    size: stat2.size + 4,
                    mtime_s: 1_760_000_006,
                },
                stat2.size,
                b"abcd",
            )
            .is_err());
        assert!(mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                rpath,
                RemoteFileStat {
                    ino: 42,
                    size: stat2.size + 4,
                    mtime_s: 1_760_000_006,
                },
                stat2.size + 1,
                b"abc",
            )
            .is_err());

        // 4. Rejects oversized inline delta (> 64 KiB) and traversal paths.
        let giant = vec![b'x'; MAX_INLINE_DELTA_BYTES + 1];
        assert!(mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                rpath,
                RemoteFileStat {
                    ino: 42,
                    size: stat2.size + giant.len() as u64,
                    mtime_s: 1_760_000_007,
                },
                stat2.size,
                &giant,
            )
            .is_err());
        assert!(mirror
            .ingest_inline_delta(
                "host-a",
                rhome,
                AgentKind::Claude,
                "/home/demo/.claude/projects/../../etc/passwd",
                stat1,
                0,
                b"bad",
            )
            .is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduit_mirror_reuses_single_ssh_spawn_across_sync_delta_and_backfill() {
        let dir = temp_test_dir("conduit-mirror");
        let state_dir = dir.join("state");
        let remote_home = dir.join("remote_home");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(remote_home.join(".claude/projects/-home-u-demo")).unwrap();
        let calls_log = dir.join("conduit_spawns.log");
        let fake_ssh = dir.join("fake-ssh");
        write_executable(
            &fake_ssh,
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then exit 0; fi
printf 'SPAWN\n' >> "{log}"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
export HOME="{rhome}"
exec /bin/sh -c "$1"
"#,
                log = calls_log.display(),
                rhome = remote_home.display(),
            ),
        );
        let ssh = SshExec::new("demo@conduit-test", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()))
            .with_timeout(Duration::from_secs(10));
        let pool = SshConduitPool::new(ssh);
        let mirror = RemoteTranscriptMirror::new(state_dir);
        let cache = Mutex::new(TranscriptIndexCache::default());

        let tr_path = remote_home.join(".claude/projects/-home-u-demo/conduit-s1.jsonl");
        let pad = "a".repeat(2048);
        let mut body = String::new();
        for i in 0..1200 {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-11-25T09:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"turn-{i} {pad}\"}}}}\n"
            ));
        }
        assert!(body.len() as u64 > INITIAL_TAIL_WINDOW);
        fs::write(&tr_path, &body).unwrap();
        let rhome = remote_home.to_string_lossy().into_owned();
        let rpath = tr_path.to_string_lossy().into_owned();

        // 1. Cold tail window over persistent conduit (1 spawn).
        let p1 = mirror
            .read_page_and_metrics_conduit(
                &pool,
                ConduitLane::Interactive,
                &cache,
                "h-conduit",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                TranscriptQuery::tail(20),
                None,
                1000,
            )
            .await
            .unwrap();
        assert_eq!(p1.response.source, "transcript");
        assert!(p1.sync.base_offset > 0);
        assert!(p1.response.has_more);
        assert_eq!(
            fs::read_to_string(&calls_log).unwrap().lines().count(),
            1,
            "conduit should spawn ssh once"
        );

        // 2. Append delta and re-read via the SAME persistent conduit (still 1 ssh spawn!).
        let extra_line = "{\"type\":\"assistant\",\"timestamp\":\"2026-11-25T09:01:00Z\",\"message\":{\"id\":\"m-end\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"conduit delta ok\"}]}}\n";
        {
            let mut f = OpenOptions::new().append(true).open(&tr_path).unwrap();
            f.write_all(extra_line.as_bytes()).unwrap();
        }
        let p2 = mirror
            .read_page_and_metrics_conduit(
                &pool,
                ConduitLane::Interactive,
                &cache,
                "h-conduit",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                TranscriptQuery::tail(5),
                None,
                2000,
            )
            .await
            .unwrap();
        assert_eq!(
            p2.response.messages.last().unwrap().text,
            "conduit delta ok"
        );
        assert_eq!(
            fs::read_to_string(&calls_log).unwrap().lines().count(),
            1,
            "delta fetch must reuse existing conduit without spawning ssh"
        );

        // 3. Full history backfill (`all=true`) over the SAME persistent conduit (still 1 ssh spawn!).
        let p3 = mirror
            .read_page_and_metrics_conduit(
                &pool,
                ConduitLane::Interactive,
                &cache,
                "h-conduit",
                &rhome,
                AgentKind::Claude,
                &rpath,
                None,
                TranscriptQuery {
                    limit: 5000,
                    before: None,
                    all: true,
                },
                None,
                3000,
            )
            .await
            .unwrap();
        assert_eq!(p3.sync.base_offset, 0);
        assert!(p3.response.messages[0].text.starts_with("turn-0 "));
        assert_eq!(
            fs::read_to_string(&calls_log).unwrap().lines().count(),
            1,
            "prefix backfill must reuse existing conduit without spawning ssh"
        );

        pool.reset().await;
        let _ = fs::remove_dir_all(&dir);
    }
}
