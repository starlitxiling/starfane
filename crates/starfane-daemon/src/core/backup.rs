//! Native daemon bridge for `[tmux.backup]` periodic snapshots, startup restore,
//! and manual backup/restore APIs.

use super::metrics::{canonical_external_session_id, resolve_claude_transcript_by_pids};
use super::{lock, Core};
use starfane_proto::{AgentKind, ApiError, ErrorCode, PaneId, SessionState};
use starfane_tmux::{
    build_snapshot, list_snapshots, load_latest_snapshot, load_snapshot_file, save_snapshot_to_dir,
    BackupSaveOutcome, DiscoverOptions, PaneAgentHint, RestoreOptions, RestoreReport,
    SnapshotSummary,
};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

fn resolve_backup_file_path(dir: &Path, raw: &str) -> Result<PathBuf, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("backup file path cannot be empty"));
    }
    let p = Path::new(trimmed);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ApiError::bad_request("backup path must not contain `..`"));
    }
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if p.components().count() != 1 {
        return Err(ApiError::bad_request(
            "relative backup file must be a snapshot filename inside tmux-backups",
        ));
    }
    Ok(dir.join(p))
}

impl Core {
    pub fn tmux_backups_dir(&self) -> PathBuf {
        self.inner.state_dir.join("tmux-backups")
    }

    pub fn restore_options(&self, restore_agents: Option<bool>, dry_run: bool) -> RestoreOptions {
        let cfg = &self.inner.cfg;
        RestoreOptions {
            restore_agents: restore_agents.unwrap_or(cfg.tmux.backup.restore_agents),
            restore_bypass_permissions: cfg.tmux.backup.restore_bypass_permissions,
            dry_run,
            claude_bin: cfg.agent_bin(AgentKind::Claude),
            codex_bin: cfg.agent_bin(AgentKind::Codex),
        }
    }

    /// Capture the current tmux layout + active coding-agent resume IDs into `<state>/tmux-backups/`.
    pub async fn backup_tmux(&self, force: bool) -> Result<BackupSaveOutcome, ApiError> {
        self.backup_tmux_inner(force, false).await
    }

    /// Fast shutdown snapshot using in-memory daemon session hints without re-running `ps`/`lsof`.
    pub(crate) async fn backup_tmux_for_shutdown(&self) -> Result<BackupSaveOutcome, ApiError> {
        self.backup_tmux_inner(false, true).await
    }

    async fn backup_tmux_inner(
        &self,
        force: bool,
        fast_state_hints_only: bool,
    ) -> Result<BackupSaveOutcome, ApiError> {
        let hints_from_state: HashMap<PaneId, PaneAgentHint> = {
            let st = lock(&self.inner.state);
            let mut ranked: HashMap<PaneId, (bool, u64, PaneAgentHint)> = HashMap::new();
            for s in st.sessions.values() {
                if s.is_subagent || s.agent.is_shell() || s.state == SessionState::Ended {
                    continue;
                }
                let Some(pane) = s.pane.as_ref() else {
                    continue;
                };
                let is_real = !s.discovered_only;
                let resume_id = canonical_external_session_id(s);
                let replace = ranked
                    .get(pane)
                    .is_none_or(|(prev_real, prev_ms, prev_hint)| {
                        (is_real && !*prev_real)
                            || (is_real == *prev_real
                                && ((resume_id.is_some() && prev_hint.resume_id.is_none())
                                    || s.last_event_ms > *prev_ms))
                    });
                if replace {
                    ranked.insert(
                        pane.clone(),
                        (
                            is_real,
                            s.last_event_ms,
                            PaneAgentHint {
                                agent: s.agent,
                                resume_id,
                                model: s.model.clone(),
                            },
                        ),
                    );
                }
            }
            ranked.into_iter().map(|(k, (_, _, h))| (k, h)).collect()
        };

        let core = self.clone();
        let dir = self.tmux_backups_dir();
        let max_snapshots = self.inner.cfg.tmux.backup.max_snapshots;
        let exclude = self.inner.cfg.tmux.backup.exclude_sessions.clone();
        let now_ms = starfane_proto::id::now_ms();

        tokio::task::spawn_blocking(move || {
            let windows = core.inner.tmux.list_windows().map_err(|e| {
                ApiError::new(ErrorCode::Unavailable, format!("tmux list-windows: {e}"))
            })?;
            let panes = core.inner.tmux.list_panes().map_err(|e| {
                ApiError::new(ErrorCode::Unavailable, format!("tmux list-panes: {e}"))
            })?;
            let clients = core.inner.tmux.list_clients().map_err(|e| {
                ApiError::new(ErrorCode::Unavailable, format!("tmux list-clients: {e}"))
            })?;
            let mut hints = hints_from_state;
            if !fast_state_hints_only {
                let discovered = core.inner.tmux.discover_panes_from(
                    &panes,
                    DiscoverOptions {
                        include_shell_panes: false,
                    },
                );

                for dp in discovered {
                    if dp.pane_dead || dp.agent.is_shell() {
                        continue;
                    }
                    let fallback_resume = match dp.agent {
                        AgentKind::Claude => {
                            resolve_claude_transcript_by_pids(&dp.subtree_pids).map(|(sid, _)| sid)
                        }
                        AgentKind::Codex => dp.codex_resume_id.clone(),
                        AgentKind::Agy | AgentKind::Shell => None,
                    };
                    match hints.get_mut(&dp.pane_id) {
                        Some(existing) if existing.agent == dp.agent => {
                            if existing.resume_id.is_none() {
                                existing.resume_id = fallback_resume;
                            }
                            if existing.model.is_none() {
                                existing.model = dp.model;
                            }
                        }
                        Some(_) => {}
                        None => {
                            hints.insert(
                                dp.pane_id,
                                PaneAgentHint {
                                    agent: dp.agent,
                                    resume_id: fallback_resume,
                                    model: dp.model,
                                },
                            );
                        }
                    }
                }
            }

            let snap = build_snapshot(&windows, &panes, &clients, &hints, &exclude, now_ms);
            save_snapshot_to_dir(&dir, &snap, max_snapshots, force)
                .map_err(|e| ApiError::internal(format!("save tmux backup: {e}")))
        })
        .await
        .map_err(|e| ApiError::internal(format!("backup task failed: {e}")))?
    }

    /// Restore tmux sessions, windows, split layouts, and optional `claude`/`codex` agents.
    pub async fn restore_tmux(
        &self,
        file: Option<String>,
        restore_agents: Option<bool>,
        dry_run: bool,
    ) -> Result<RestoreReport, ApiError> {
        let dir = self.tmux_backups_dir();
        let (path, snap) = match file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let p = resolve_backup_file_path(&dir, raw)?;
                let snap = load_snapshot_file(&p).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        ApiError::not_found(format!("tmux backup not found: {}", p.display()))
                    } else {
                        ApiError::bad_request(format!("unreadable backup {}: {e}", p.display()))
                    }
                })?;
                (p, snap)
            }
            None => load_latest_snapshot(&dir)
                .map_err(|e| ApiError::internal(format!("load latest tmux backup: {e}")))?
                .ok_or_else(|| ApiError::not_found("no tmux backup snapshot found"))?,
        };

        let opts = self.restore_options(restore_agents, dry_run);
        let core = self.clone();
        let mut report = tokio::task::spawn_blocking(move || {
            core.inner
                .tmux
                .restore_snapshot(&snap, &opts)
                .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("tmux restore: {e}")))
        })
        .await
        .map_err(|e| ApiError::internal(format!("restore task failed: {e}")))??;
        report.snapshot_path = Some(path);

        if !dry_run && (!report.restored_sessions.is_empty() || report.restored_panes > 0) {
            self.sync_discovered_panes().await;
        }
        Ok(report)
    }

    pub async fn list_tmux_backups(&self) -> Result<Vec<SnapshotSummary>, ApiError> {
        let dir = self.tmux_backups_dir();
        tokio::task::spawn_blocking(move || {
            list_snapshots(&dir).map_err(|e| ApiError::internal(format!("list tmux backups: {e}")))
        })
        .await
        .map_err(|e| ApiError::internal(format!("list backup task failed: {e}")))?
    }

    /// Automatically restore `latest.json` on startup when enabled and tmux has zero sessions.
    pub(crate) async fn maybe_auto_restore_on_start(&self) {
        let bk = &self.inner.cfg.tmux.backup;
        if !bk.enabled || !bk.auto_restore_on_start {
            return;
        }
        let core = self.clone();
        let sessions =
            match tokio::task::spawn_blocking(move || core.inner.tmux.list_sessions()).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    log::warn!("tmux auto-restore skipped (list-sessions failed: {e})");
                    return;
                }
                Err(e) => {
                    log::warn!("tmux auto-restore skipped (task failed: {e})");
                    return;
                }
            };
        if !sessions.is_empty() {
            log::debug!(
                "tmux auto-restore skipped: {} live session(s) already running",
                sessions.len()
            );
            return;
        }
        let dir = self.tmux_backups_dir();
        if load_latest_snapshot(&dir).ok().flatten().is_none() {
            return;
        }
        match self.restore_tmux(None, None, false).await {
            Ok(r) => {
                log::info!(
                    "tmux auto-restore completed: sessions={:?} windows={} panes={} resumed_agents={} warnings={}",
                    r.restored_sessions,
                    r.restored_windows,
                    r.restored_panes,
                    r.resumed_agents.len(),
                    r.warnings.len()
                );
            }
            Err(e) => log::warn!("tmux auto-restore failed: {}", e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_blocks_traversal_and_nested_relatives() {
        let dir = Path::new("/tmp/sf-backups");
        assert!(resolve_backup_file_path(dir, "../secret.json").is_err());
        assert!(resolve_backup_file_path(dir, "nested/snap.json").is_err());
        assert!(resolve_backup_file_path(dir, "   ").is_err());
        assert_eq!(
            resolve_backup_file_path(dir, "snapshot-123.json").unwrap(),
            PathBuf::from("/tmp/sf-backups/snapshot-123.json")
        );
        assert_eq!(
            resolve_backup_file_path(dir, "/var/backups/snap.json").unwrap(),
            PathBuf::from("/var/backups/snap.json")
        );
    }
}
