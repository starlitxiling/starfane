//! Native tmux layout snapshot, rolling retention, non-destructive restore,
//! and optional `claude` / `codex` session resume.

use crate::error::TmuxError;
use crate::model::{ClientInfo, PaneInfo, WindowInfo};
use crate::Tmux;
use serde::{Deserialize, Serialize};
use starfane_proto::{AgentKind, PaneId};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

pub const BACKUP_VERSION: u32 = 1;
const LATEST_FILE: &str = "latest.json";
const SNAPSHOT_PREFIX: &str = "snapshot-";
const SNAPSHOT_EXT: &str = ".json";
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxBackupSnapshot {
    pub v: u32,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session: Option<String>,
    #[serde(default)]
    pub sessions: Vec<SavedSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    pub name: String,
    #[serde(default)]
    pub active_window_index: u32,
    #[serde(default)]
    pub windows: Vec<SavedWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub index: u32,
    pub name: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub panes: Vec<SavedPane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPane {
    pub index: u32,
    pub cwd: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Optional live coding-agent attribution attached to a pane when capturing a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAgentHint {
    pub agent: AgentKind,
    pub resume_id: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSaveOutcome {
    pub saved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub latest_path: PathBuf,
    pub session_count: usize,
    pub window_count: usize,
    pub pane_count: usize,
    pub resumable_agent_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub file_name: String,
    pub path: PathBuf,
    pub created_ms: u64,
    pub session_count: usize,
    pub window_count: usize,
    pub pane_count: usize,
    pub resumable_agent_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOptions {
    pub restore_agents: bool,
    pub restore_bypass_permissions: bool,
    pub dry_run: bool,
    pub claude_bin: String,
    pub codex_bin: String,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        RestoreOptions {
            restore_agents: true,
            restore_bypass_permissions: false,
            dry_run: false,
            claude_bin: "claude".into(),
            codex_bin: "codex".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<PathBuf>,
    #[serde(default)]
    pub restored_sessions: Vec<String>,
    #[serde(default)]
    pub skipped_sessions: Vec<String>,
    pub restored_windows: usize,
    pub restored_panes: usize,
    #[serde(default)]
    pub resumed_agents: Vec<ResumedAgentInfo>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumedAgentInfo {
    pub session_name: String,
    pub window_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub agent: AgentKind,
    pub resume_id: String,
    pub command: String,
}

impl TmuxBackupSnapshot {
    /// True when layout, directories, active flags, and agent/resume metadata match `other`.
    pub fn same_topology(&self, other: &Self) -> bool {
        self.v == other.v
            && self.active_session == other.active_session
            && self.sessions == other.sessions
    }

    pub fn window_count(&self) -> usize {
        self.sessions.iter().map(|s| s.windows.len()).sum()
    }

    pub fn pane_count(&self) -> usize {
        self.sessions
            .iter()
            .flat_map(|s| s.windows.iter())
            .map(|w| w.panes.len())
            .sum()
    }

    pub fn resumable_agent_count(&self) -> usize {
        self.sessions
            .iter()
            .flat_map(|s| s.windows.iter())
            .flat_map(|w| w.panes.iter())
            .filter(|p| {
                p.agent
                    .is_some_and(|a| matches!(a, AgentKind::Claude | AgentKind::Codex))
                    && p.resume_id.as_deref().is_some_and(validate_resume_id)
            })
            .count()
    }
}

/// Safe session/resume identifier check: non-empty ASCII `[A-Za-z0-9_-]`, max 128 bytes.
pub fn validate_resume_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// POSIX shell single-quote escaping (`'` -> `'\''`).
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build an interactive CLI resume command for `claude` or `codex`, validating `resume_id`.
pub fn build_resume_command(
    agent: AgentKind,
    resume_id: &str,
    opts: &RestoreOptions,
) -> Option<String> {
    let rid = resume_id.trim();
    if !validate_resume_id(rid) {
        return None;
    }
    match agent {
        AgentKind::Claude => {
            let bin = non_empty_or(&opts.claude_bin, "claude");
            let mut cmd = format!(
                "{} --resume {}",
                shell_single_quote(&bin),
                shell_single_quote(rid)
            );
            if opts.restore_bypass_permissions {
                cmd.push_str(" --dangerously-skip-permissions");
            }
            Some(cmd)
        }
        AgentKind::Codex => {
            let bin = non_empty_or(&opts.codex_bin, "codex");
            let mut cmd = format!(
                "{} resume {}",
                shell_single_quote(&bin),
                shell_single_quote(rid)
            );
            if opts.restore_bypass_permissions {
                cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
            }
            Some(cmd)
        }
        AgentKind::Agy | AgentKind::Shell => None,
    }
}

fn non_empty_or(raw: &str, fallback: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// Construct a deterministic snapshot of live tmux sessions, windows, panes, and agent hints.
pub fn build_snapshot(
    windows: &[WindowInfo],
    panes: &[PaneInfo],
    clients: &[ClientInfo],
    agent_hints: &HashMap<PaneId, PaneAgentHint>,
    exclude_sessions: &[String],
    now_ms: u64,
) -> TmuxBackupSnapshot {
    let excluded: HashSet<&str> = exclude_sessions
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let mut win_meta: HashMap<(&str, u32), &WindowInfo> = HashMap::new();
    for w in windows {
        if !excluded.contains(w.session_name.as_str()) {
            win_meta.insert((w.session_name.as_str(), w.window_index), w);
        }
    }

    let mut grouped: BTreeMap<String, BTreeMap<u32, Vec<&PaneInfo>>> = BTreeMap::new();
    for p in panes {
        if p.dead || excluded.contains(p.session_name.as_str()) {
            continue;
        }
        grouped
            .entry(p.session_name.clone())
            .or_default()
            .entry(p.window_index)
            .or_default()
            .push(p);
    }

    let mut sessions = Vec::with_capacity(grouped.len());
    for (sess_name, win_map) in grouped {
        let mut saved_windows = Vec::with_capacity(win_map.len());
        let mut active_window_index: Option<u32> = None;

        for (win_idx, mut pane_list) in win_map {
            pane_list.sort_by_key(|p| p.pane_index);
            let meta = win_meta.get(&(sess_name.as_str(), win_idx)).copied();
            let first = pane_list[0];
            let win_name = meta
                .map(|m| m.window_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    if first.window_name.is_empty() {
                        format!("win{win_idx}")
                    } else {
                        first.window_name.clone()
                    }
                });
            let win_layout = meta.map(|m| m.window_layout.clone()).unwrap_or_default();
            let win_active = meta
                .map(|m| m.window_active)
                .unwrap_or_else(|| pane_list.iter().any(|p| p.window_active));
            if win_active && active_window_index.is_none() {
                active_window_index = Some(win_idx);
            }

            let saved_panes = pane_list
                .into_iter()
                .map(|p| {
                    let hint = agent_hints.get(&p.pane_id);
                    let resume_id = hint
                        .and_then(|h| h.resume_id.as_deref())
                        .map(str::trim)
                        .filter(|id| validate_resume_id(id))
                        .map(str::to_string);
                    SavedPane {
                        index: p.pane_index,
                        cwd: p.current_path.clone(),
                        command: p.current_command.clone(),
                        active: p.pane_active,
                        agent: hint.map(|h| h.agent),
                        resume_id,
                        model: hint.and_then(|h| h.model.clone()),
                    }
                })
                .collect();

            saved_windows.push(SavedWindow {
                index: win_idx,
                name: win_name,
                layout: win_layout,
                active: win_active,
                panes: saved_panes,
            });
        }

        if saved_windows.is_empty() {
            continue;
        }
        let active_window_index = active_window_index.unwrap_or(saved_windows[0].index);
        sessions.push(SavedSession {
            name: sess_name,
            active_window_index,
            windows: saved_windows,
        });
    }

    let valid_names: HashSet<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    let active_session = clients
        .iter()
        .filter(|c| valid_names.contains(c.session_name.as_str()))
        .max_by_key(|c| c.activity_s)
        .map(|c| c.session_name.clone());

    TmuxBackupSnapshot {
        v: BACKUP_VERSION,
        created_ms: now_ms,
        active_session,
        sessions,
    }
}

fn write_atomic_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = {
            let _ = &OpenOptions::new();
            fs::File::create(&tmp)?
        };
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn unique_snapshot_path(dir: &Path, created_ms: u64) -> PathBuf {
    let primary = dir.join(format!("{SNAPSHOT_PREFIX}{created_ms}{SNAPSHOT_EXT}"));
    if !primary.exists() {
        return primary;
    }
    let mut seq = 1u32;
    loop {
        let cand = dir.join(format!("{SNAPSHOT_PREFIX}{created_ms}-{seq}{SNAPSHOT_EXT}"));
        if !cand.exists() {
            return cand;
        }
        seq += 1;
    }
}

fn summarize_snapshot(path: &Path, snap: &TmuxBackupSnapshot) -> SnapshotSummary {
    SnapshotSummary {
        file_name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        path: path.to_path_buf(),
        created_ms: snap.created_ms,
        session_count: snap.sessions.len(),
        window_count: snap.window_count(),
        pane_count: snap.pane_count(),
        resumable_agent_count: snap.resumable_agent_count(),
    }
}

/// Save `snap` atomically into `dir`, updating `latest.json` and pruning old `snapshot-*.json` files.
/// Refuses to overwrite `latest.json` with an empty snapshot (`sessions.is_empty()`), and skips
/// identical topologies unless `force` is true.
pub fn save_snapshot_to_dir(
    dir: &Path,
    snap: &TmuxBackupSnapshot,
    max_snapshots: usize,
    force: bool,
) -> io::Result<BackupSaveOutcome> {
    let latest_path = dir.join(LATEST_FILE);
    let session_count = snap.sessions.len();
    let window_count = snap.window_count();
    let pane_count = snap.pane_count();
    let resumable_agent_count = snap.resumable_agent_count();

    if snap.sessions.is_empty() {
        return Ok(BackupSaveOutcome {
            saved: false,
            skipped_reason: Some("empty_tmux".to_string()),
            path: None,
            latest_path,
            session_count,
            window_count,
            pane_count,
            resumable_agent_count,
        });
    }

    if !force && latest_path.is_file() {
        if let Ok(prev) = load_snapshot_file(&latest_path) {
            if prev.same_topology(snap) {
                return Ok(BackupSaveOutcome {
                    saved: false,
                    skipped_reason: Some("unchanged".to_string()),
                    path: None,
                    latest_path,
                    session_count,
                    window_count,
                    pane_count,
                    resumable_agent_count,
                });
            }
        }
    }

    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(snap)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let snap_path = unique_snapshot_path(dir, snap.created_ms);
    write_atomic_private(&snap_path, &bytes)?;
    write_atomic_private(&latest_path, &bytes)?;

    let keep = max_snapshots.max(1);
    let summaries = list_snapshots(dir)?;
    for stale in summaries.into_iter().skip(keep) {
        let _ = fs::remove_file(stale.path);
    }

    Ok(BackupSaveOutcome {
        saved: true,
        skipped_reason: None,
        path: Some(snap_path),
        latest_path,
        session_count,
        window_count,
        pane_count,
        resumable_agent_count,
    })
}

pub fn load_snapshot_file(path: &Path) -> io::Result<TmuxBackupSnapshot> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn load_latest_snapshot(dir: &Path) -> io::Result<Option<(PathBuf, TmuxBackupSnapshot)>> {
    let latest_path = dir.join(LATEST_FILE);
    match load_snapshot_file(&latest_path) {
        Ok(snap) => Ok(Some((latest_path, snap))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut listed = list_snapshots(dir)?;
            let Some(newest) = listed.drain(..).next() else {
                return Ok(None);
            };
            let snap = load_snapshot_file(&newest.path)?;
            Ok(Some((newest.path, snap)))
        }
        Err(e) => Err(e),
    }
}

pub fn list_snapshots(dir: &Path) -> io::Result<Vec<SnapshotSummary>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(SNAPSHOT_PREFIX) || !name.ends_with(SNAPSHOT_EXT) {
            continue;
        }
        if let Ok(snap) = load_snapshot_file(&path) {
            items.push(summarize_snapshot(&path, &snap));
        }
    }
    items.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| b.file_name.cmp(&a.file_name))
    });
    Ok(items)
}

fn resolve_restore_cwd(raw: &str, warnings: &mut Vec<String>) -> String {
    let p = Path::new(raw);
    if !raw.trim().is_empty() && p.is_dir() {
        return raw.to_string();
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if home.is_dir() {
            warnings.push(format!(
                "saved directory {raw:?} missing; fell back to {}",
                home.display()
            ));
            return home.to_string_lossy().into_owned();
        }
    }
    warnings.push(format!("saved directory {raw:?} missing; fell back to /"));
    "/".to_string()
}

struct PendingResume {
    session_name: String,
    window_name: String,
    pane_id: String,
    agent: AgentKind,
    resume_id: String,
    command: String,
}

impl Tmux {
    /// Restore tmux sessions, windows, split layouts, and optionally resume `claude`/`codex`
    /// agents from `snap`. Existing tmux sessions with matching names are left untouched.
    pub fn restore_snapshot(
        &self,
        snap: &TmuxBackupSnapshot,
        opts: &RestoreOptions,
    ) -> Result<RestoreReport, TmuxError> {
        let existing: HashSet<String> = self.list_sessions()?.into_iter().collect();
        let mut report = RestoreReport {
            dry_run: opts.dry_run,
            snapshot_path: None,
            restored_sessions: Vec::new(),
            skipped_sessions: Vec::new(),
            restored_windows: 0,
            restored_panes: 0,
            resumed_agents: Vec::new(),
            warnings: Vec::new(),
        };
        let mut pending_resumes: Vec<PendingResume> = Vec::new();

        for sess in &snap.sessions {
            if sess.name.trim().is_empty() || sess.windows.is_empty() {
                continue;
            }
            if existing.contains(&sess.name) {
                report.skipped_sessions.push(sess.name.clone());
                continue;
            }
            report.restored_sessions.push(sess.name.clone());

            if opts.dry_run {
                for w in &sess.windows {
                    report.restored_windows += 1;
                    for p in &w.panes {
                        report.restored_panes += 1;
                        if opts.restore_agents {
                            if let (Some(agent), Some(rid)) = (p.agent, p.resume_id.as_deref()) {
                                if let Some(cmd) = build_resume_command(agent, rid, opts) {
                                    report.resumed_agents.push(ResumedAgentInfo {
                                        session_name: sess.name.clone(),
                                        window_name: w.name.clone(),
                                        pane_id: None,
                                        agent,
                                        resume_id: rid.to_string(),
                                        command: cmd,
                                    });
                                }
                            }
                        }
                    }
                }
                continue;
            }

            let mut active_window_pane: Option<String> = None;
            let mut active_fallback_pane: Option<String> = None;

            for (w_idx, w) in sess.windows.iter().enumerate() {
                let Some(first_saved_pane) = w.panes.first() else {
                    continue;
                };
                let first_cwd = resolve_restore_cwd(&first_saved_pane.cwd, &mut report.warnings);
                let first_pane_id = if w_idx == 0 {
                    self.run(
                        &[
                            "new-session",
                            "-d",
                            "-P",
                            "-F",
                            "#{pane_id}",
                            "-s",
                            &sess.name,
                            "-n",
                            &w.name,
                            "-c",
                            &first_cwd,
                        ],
                        None,
                    )?
                } else {
                    let target = format!("={}:", sess.name);
                    self.run(
                        &[
                            "new-window",
                            "-d",
                            "-P",
                            "-F",
                            "#{pane_id}",
                            "-t",
                            &target,
                            "-n",
                            &w.name,
                            "-c",
                            &first_cwd,
                        ],
                        None,
                    )?
                };
                let first_pane_id = first_pane_id.trim().to_string();
                if active_fallback_pane.is_none() {
                    active_fallback_pane = Some(first_pane_id.clone());
                }
                if w.index == sess.active_window_index || w.active {
                    active_window_pane = Some(first_pane_id.clone());
                }

                let mut created_panes = Vec::with_capacity(w.panes.len());
                created_panes.push((first_pane_id.clone(), first_saved_pane));

                for extra_pane in w.panes.iter().skip(1) {
                    let cwd = resolve_restore_cwd(&extra_pane.cwd, &mut report.warnings);
                    let pid = self.run(
                        &[
                            "split-window",
                            "-d",
                            "-P",
                            "-F",
                            "#{pane_id}",
                            "-t",
                            &first_pane_id,
                            "-c",
                            &cwd,
                        ],
                        None,
                    )?;
                    created_panes.push((pid.trim().to_string(), extra_pane));
                }

                if created_panes.len() > 1 && !w.layout.trim().is_empty() {
                    if let Err(e) =
                        self.run(&["select-layout", "-t", &first_pane_id, &w.layout], None)
                    {
                        report.warnings.push(format!(
                            "layout restore failed for {}:{} ({}): {e}",
                            sess.name, w.name, w.index
                        ));
                    }
                }

                if let Some((active_pid, _)) = created_panes.iter().find(|(_, sp)| sp.active) {
                    let _ = self.run(&["select-pane", "-t", active_pid], None);
                }

                report.restored_windows += 1;
                report.restored_panes += created_panes.len();

                if opts.restore_agents {
                    for (pane_id, saved_pane) in created_panes {
                        if let (Some(agent), Some(rid)) =
                            (saved_pane.agent, saved_pane.resume_id.as_deref())
                        {
                            if let Some(cmd) = build_resume_command(agent, rid, opts) {
                                pending_resumes.push(PendingResume {
                                    session_name: sess.name.clone(),
                                    window_name: w.name.clone(),
                                    pane_id,
                                    agent,
                                    resume_id: rid.to_string(),
                                    command: cmd,
                                });
                            }
                        }
                    }
                }
            }

            if let Some(win_pane) = active_window_pane.or(active_fallback_pane) {
                let _ = self.run(&["select-window", "-t", &win_pane], None);
            }
        }

        if !pending_resumes.is_empty() {
            // Brief settle pause so freshly spawned interactive shells finish tty setup before input.
            thread::sleep(Duration::from_millis(120));
            for item in pending_resumes {
                if let Err(e) = self
                    .run(
                        &["send-keys", "-t", &item.pane_id, "-l", &item.command],
                        None,
                    )
                    .and_then(|_| self.run(&["send-keys", "-t", &item.pane_id, "C-m"], None))
                {
                    report.warnings.push(format!(
                        "failed to send resume command to {} ({}): {e}",
                        item.pane_id, item.session_name
                    ));
                    continue;
                }
                report.resumed_agents.push(ResumedAgentInfo {
                    session_name: item.session_name,
                    window_name: item.window_name,
                    pane_id: Some(item.pane_id),
                    agent: item.agent,
                    resume_id: item.resume_id,
                    command: item.command,
                });
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_win(sess: &str, idx: u32, name: &str, layout: &str, active: bool) -> WindowInfo {
        WindowInfo {
            session_name: sess.into(),
            window_index: idx,
            window_name: name.into(),
            window_layout: layout.into(),
            window_active: active,
        }
    }

    fn mk_pane(
        id: &str,
        sess: &str,
        widx: u32,
        pidx: u32,
        wname: &str,
        cmd: &str,
        cwd: &str,
    ) -> PaneInfo {
        PaneInfo {
            pane_id: PaneId::new(id),
            session_name: sess.into(),
            window_id: format!("@{widx}"),
            window_index: widx,
            pane_index: pidx,
            pane_pid: 1000 + pidx,
            current_command: cmd.into(),
            current_path: cwd.into(),
            pane_active: true,
            window_active: true,
            session_attached: 1,
            in_mode: false,
            dead: false,
            synchronized: false,
            alternate_on: false,
            cursor_x: 0,
            cursor_y: 0,
            window_name: wname.into(),
            session_group: String::new(),
            history_size: 0,
            pane_height: 24,
        }
    }

    #[test]
    fn builds_snapshot_respects_exclusions_dead_panes_and_topology_eq() {
        let windows = vec![
            mk_win("work", 0, "editor", "layout-0", false),
            mk_win("work", 1, "agent", "layout-1", true),
            mk_win("scratch", 0, "tmp", "layout-2", true),
        ];
        let mut dead_pane = mk_pane("%99", "work", 0, 1, "editor", "zsh", "/repo");
        dead_pane.pane_active = false;
        dead_pane.dead = true;
        let panes = vec![
            mk_pane("%1", "work", 0, 0, "editor", "zsh", "/repo"),
            dead_pane,
            mk_pane("%2", "work", 1, 0, "agent", "codex", "/repo"),
            mk_pane("%3", "scratch", 0, 0, "tmp", "zsh", "/tmp"),
        ];
        let clients = vec![ClientInfo {
            tty: "/dev/ttys001".into(),
            session_name: "work".into(),
            activity_s: 500,
            flags: "attached".into(),
            session_group: String::new(),
            window_id: String::new(),
        }];
        let mut hints = HashMap::new();
        hints.insert(
            PaneId::new("%2"),
            PaneAgentHint {
                agent: AgentKind::Codex,
                resume_id: Some("019ab-cd23-4567".into()),
                model: Some("gpt-5".into()),
            },
        );
        let snap1 = build_snapshot(&windows, &panes, &clients, &hints, &["scratch".into()], 100);
        let snap2 = build_snapshot(&windows, &panes, &clients, &hints, &["scratch".into()], 999);

        assert_eq!(snap1.sessions.len(), 1);
        assert_eq!(snap1.sessions[0].name, "work");
        assert_eq!(snap1.sessions[0].active_window_index, 1);
        assert_eq!(snap1.window_count(), 2);
        assert_eq!(snap1.pane_count(), 2);
        assert_eq!(snap1.resumable_agent_count(), 1);
        assert_eq!(snap1.active_session.as_deref(), Some("work"));
        assert!(snap1.same_topology(&snap2));
    }

    #[test]
    fn save_empty_dedup_force_and_prunes_retention() {
        let dir = std::env::temp_dir().join(format!("sf-bk-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Save valid initial snapshot.
        let s1 = TmuxBackupSnapshot {
            v: BACKUP_VERSION,
            created_ms: 1000,
            active_session: Some("a".into()),
            sessions: vec![SavedSession {
                name: "a".into(),
                active_window_index: 0,
                windows: vec![SavedWindow {
                    index: 0,
                    name: "sh".into(),
                    layout: String::new(),
                    active: true,
                    panes: vec![SavedPane {
                        index: 0,
                        cwd: "/tmp".into(),
                        command: "zsh".into(),
                        active: true,
                        agent: None,
                        resume_id: None,
                        model: None,
                    }],
                }],
            }],
        };
        let out1 = save_snapshot_to_dir(&dir, &s1, 2, false).unwrap();
        assert!(out1.saved);

        // Empty snapshot never overwrites latest.json.
        let empty = TmuxBackupSnapshot {
            v: BACKUP_VERSION,
            created_ms: 2000,
            active_session: None,
            sessions: Vec::new(),
        };
        let out_empty = save_snapshot_to_dir(&dir, &empty, 2, true).unwrap();
        assert!(!out_empty.saved);
        assert_eq!(out_empty.skipped_reason.as_deref(), Some("empty_tmux"));
        assert!(load_latest_snapshot(&dir).unwrap().is_some());

        // Identical topology skipped unless forced.
        let mut s2 = s1.clone();
        s2.created_ms = 3000;
        let out_same = save_snapshot_to_dir(&dir, &s2, 2, false).unwrap();
        assert!(!out_same.saved);
        assert_eq!(out_same.skipped_reason.as_deref(), Some("unchanged"));

        assert!(save_snapshot_to_dir(&dir, &s2, 2, true).unwrap().saved);
        let mut s3 = s1.clone();
        s3.created_ms = 4000;
        assert!(save_snapshot_to_dir(&dir, &s3, 2, true).unwrap().saved);

        let listed = list_snapshots(&dir).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].created_ms, 4000);
        assert_eq!(listed[1].created_ms, 3000);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_id_validation_and_shell_quoting_block_injection() {
        assert!(validate_resume_id("11111111-2222-4333-8444-555555555555"));
        assert!(validate_resume_id("abc_123-DEF"));
        assert!(!validate_resume_id(""));
        assert!(!validate_resume_id("bad; rm -rf /"));
        assert!(!validate_resume_id("$(whoami)"));
        assert!(!validate_resume_id("has space"));

        let opts = RestoreOptions {
            restore_agents: true,
            restore_bypass_permissions: true,
            dry_run: false,
            claude_bin: "/path/with'quote/claude".into(),
            codex_bin: "/usr/local/bin/codex".into(),
        };
        let cl = build_resume_command(AgentKind::Claude, "abc-123", &opts).unwrap();
        assert_eq!(
            cl,
            "'/path/with'\\''quote/claude' --resume 'abc-123' --dangerously-skip-permissions"
        );
        let cx = build_resume_command(AgentKind::Codex, "uuid-456", &opts).unwrap();
        assert_eq!(
            cx,
            "'/usr/local/bin/codex' resume 'uuid-456' --dangerously-bypass-approvals-and-sandbox"
        );
        assert!(build_resume_command(AgentKind::Codex, "uuid;evil", &opts).is_none());
        assert!(build_resume_command(AgentKind::Agy, "uuid-456", &opts).is_none());
    }
}
