//! End-to-end regression & structural invariant tests for session/transcript isolation:
//! 1. Exact reproduction of the live `local:%50` (`~/.agent_session`) hijacking bug:
//!    - Pre-existing historical `Ended` AGY/Claude/Codex conversations + `cc-switch.db` metrics in `~/.agent_session`
//!    - Live sibling AGY pane (`%46`) in `~/.agent_session` holding its own `presence/<uuid>.lock`
//!    - Newly spawned empty AGY (`gemini-4-neon` + `xhigh`), Codex, and Claude panes in `~/.agent_session`
//!      MUST report 0 messages (`source: "none"`), `metrics: None`, and preserve their exact requested
//!      `model` + `effort` (`gemini-4-neon-xhigh` / `xhigh`), never inheriting `gemini-4-neon-high` or `93.2 tok/s`.
//! 2. Self-healing of already-poisoned `state.json` snapshots on daemon startup (`sync_discovered_panes`):
//!    - A persisted `discovered_only` `tmux-%50` record carrying a stolen `4b808b49` `transcript_path`,
//!      stale `cc_switch` metrics, and wrong `high` effort is scrubbed clean on the first scan.
//! 3. Multi-pane same-`cwd` isolation & post-birth / hook binding:
//!    - Two unresolved panes sharing `~/.agent_session` have zero cross-contamination.
//!    - Once a genuinely NEW conversation created AFTER pane birth appears (or a real hook arrives),
//!      it binds cleanly and exclusively to the owning session.

use serde_json::{json, Value};
use starfane_daemon::core::TranscriptQuery;
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;

struct IsolatedEnv {
    root: PathBuf,
    tmux_socket: Option<String>,
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        if let Some(sock) = &self.tmux_socket {
            let _ = Command::new("tmux")
                .args(["-L", sock, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_sqlite(db: &Path, sql: &str) -> bool {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Seed `~/.agent_session` history across AGY (`conversation_summaries.db` + `brain`),
/// Codex (`state_5.sqlite` + rollout), Claude (`projects`), and `cc-switch.db`.
fn seed_historical_agent_session_data(home: &Path) -> PathBuf {
    let agent_dir = home.join(".agent_session");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let cwd_str = agent_dir.to_string_lossy().into_owned();

    // 1. Historical AGY conversation `4b808b49` from yesterday ("教我配置一下gh吧...", gemini-4-neon-high, 93.2 tok/s)
    //    plus active sibling conversation `b02fefac` ("Analyze Anyverse Setup Script").
    let old_agy_id = "4b808b49-48a6-4aaa-9eab-15d68e18f657";
    let sib_agy_id = "b02fefac-25fe-4c35-a6f2-c73ff6d128c1";
    for (cid, prompt, reply) in [
        (
            old_agy_id,
            "教我配置一下gh吧，然后不能影响我的glab",
            "在公司电脑上配置 gh，核心做好 3 层隔离",
        ),
        (sib_agy_id, "分析一下 setup 脚本", "已完成脚本依赖检查"),
    ] {
        let tr = home
            .join(".gemini/antigravity-cli/brain")
            .join(cid)
            .join(".system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(tr.parent().unwrap()).unwrap();
        std::fs::write(
            &tr,
            format!(
                concat!(
                    r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-24T02:55:39Z","content":"<USER_REQUEST>\n{}\n</USER_REQUEST>"}}"#,
                    "\n",
                    r#"{{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-24T02:56:28Z","model":"gemini-4-neon-high","input_tokens":72000,"cache_read_tokens":736,"output_tokens":1061,"content":"{}"}}"#,
                    "\n"
                ),
                prompt, reply
            ),
        )
        .unwrap();
    }

    let agy_db = home.join(".gemini/antigravity-cli/conversation_summaries.db");
    let _ = run_sqlite(
        &agy_db,
        &format!(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                preview TEXT NOT NULL DEFAULT '',
                step_count INTEGER NOT NULL DEFAULT 0,
                last_modified_time TEXT NOT NULL,
                workspace_uris TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT '',
                agent_name TEXT NOT NULL DEFAULT '',
                parent_conversation_id TEXT NOT NULL DEFAULT '',
                nesting_depth INTEGER NOT NULL DEFAULT 0,
                killed INTEGER NOT NULL DEFAULT 0,
                last_user_input_time TEXT NOT NULL DEFAULT ''
            );
            INSERT INTO conversation_summaries VALUES
              ('{old_agy_id}', 'GitHub CLI Configuration', '', 65, '2026-11-24 09:39:32.148245+00:00', '[\"file://{cwd_str}\"]', 'CASCADE_RUN_STATUS_IDLE', '', '', 0, 0, '2026-11-24 09:39:06.702198+00:00'),
              ('{sib_agy_id}', 'Analyze Anyverse Setup', '', 9, '2026-11-24 02:55:01.314398+00:00', '[\"file://{cwd_str}\"]', 'CASCADE_RUN_STATUS_IDLE', '', '', 0, 0, '2026-11-24 02:54:14.810844+00:00');"
        ),
    );

    // 2. Historical Codex rollout + `state_5.sqlite` row in `~/.agent_session`.
    let old_cx_id = "01a18088-9999-7820-9614-90298fe15a06";
    let old_cx_tr = home
        .join(".codex/sessions/2026/11/24")
        .join(format!("rollout-2026-11-24T10-55-00-{old_cx_id}.jsonl"));
    std::fs::create_dir_all(old_cx_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &old_cx_tr,
        concat!(
            r#"{"timestamp":"2026-11-24T02:55:00.000Z","type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"medium"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-24T02:55:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"旧的 codex 会话"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-11-24T02:55:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"旧的 codex 回复"}]}}"#,
            "\n"
        ),
    )
    .unwrap();
    let cx_db = home.join(".codex/state_5.sqlite");
    let _ = run_sqlite(
        &cx_db,
        &format!(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                cwd TEXT NOT NULL,
                name TEXT,
                archived INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO threads VALUES
              ('{old_cx_id}', '{}', 1795488900, 1795488950, '{cwd_str}', '旧的 codex 会话', 0);",
            old_cx_tr.display()
        ),
    );

    // 3. Historical Claude transcript in `~/.claude/projects/<encoded>`.
    let enc_cwd: String = cwd_str
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let old_cl_tr = home
        .join(".claude/projects")
        .join(enc_cwd)
        .join("old-claude-session.jsonl");
    std::fs::create_dir_all(old_cl_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &old_cl_tr,
        r#"{"type":"user","timestamp":"2026-11-24T02:55:00Z","message":{"role":"user","content":"旧的 claude 会话"}}
"#,
    )
    .unwrap();

    // 4. Historical `cc-switch.db` metrics for `4b808b49` (`CTX 14.2% · 93.2 tok/s`) & `old_cx_id`.
    let cc_db = home.join(".cc-switch/cc-switch.db");
    let _ = run_sqlite(
        &cc_db,
        &format!(
            "CREATE TABLE proxy_request_logs (
                session_id TEXT,
                app_type TEXT,
                model TEXT,
                status_code INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER,
                latency_ms INTEGER,
                first_token_ms INTEGER,
                duration_ms INTEGER,
                input_token_semantics INTEGER,
                created_at INTEGER
            );
            INSERT INTO proxy_request_logs VALUES
              ('{old_agy_id}', 'agy', 'gemini-4-neon-high', 200, 72000, 1061, 736, 0, 25367, 13985, 25367, 2, 1795513146),
              ('{old_cx_id}', 'codex', 'gpt-6.1-sol', 200, 120000, 500, 100000, 0, 8000, 3000, 8000, 1, 1795488950);"
        ),
    );

    agent_dir
}

#[tokio::test]
async fn newly_spawned_empty_agy_codex_and_claude_sessions_never_hijack_old_agent_session_history()
{
    if Command::new("tmux").arg("-V").output().is_err()
        || Command::new("sqlite3").arg("--version").output().is_err()
    {
        return;
    }

    let root = std::env::temp_dir().join(format!("sf-iso-spawn-{}", id::new_id("T")));
    let home = root.join("home");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let sock = format!("sf-iso-{}", std::process::id());
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let agent_dir = seed_historical_agent_session_data(&home);
    let cwd_str = agent_dir.to_string_lossy().into_owned();

    // Stub interactive agent binaries (`agy`, `codex`, `claude`) that stay alive cleanly in tmux
    // without emitting hooks yet (simulating a freshly opened TUI waiting at an empty prompt).
    for name in ["agy", "codex", "claude"] {
        write_executable(
            &bin_dir.join(name),
            "#!/bin/sh\nprintf 'empty %s tui ready\\n' \"$0\"\nsleep 60\n",
        );
    }
    let tmux_wrap = bin_dir.join("tmux-wrap.sh");
    write_executable(
        &tmux_wrap,
        &format!("#!/bin/sh\nexec tmux -L {sock} \"$@\"\n"),
    );

    let mut config = StarfaneConfig::default();
    config.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    config.paths.agy = bin_dir.join("agy").to_string_lossy().into_owned();
    config.paths.codex = bin_dir.join("codex").to_string_lossy().into_owned();
    config.paths.claude = bin_dir.join("claude").to_string_lossy().into_owned();
    config.zellij.enabled = false;

    // Pre-seed `state.json` with:
    // - `4b808b49` as an `Ended` historical AGY session in `~/.agent_session`
    // - `b02fefac` as a live sibling AGY session on `%999` in `~/.agent_session`
    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let initial_state = json!({
        "sessions": {
            "4b808b49-48a6-4aaa-9eab-15d68e18f657": {
                "session_id": "4b808b49-48a6-4aaa-9eab-15d68e18f657",
                "agent": "agy",
                "pane_resolution": "unknown",
                "cwd": cwd_str,
                "model": "gemini-4-neon-high",
                "effort": "high",
                "state": "ended",
                "state_since_ms": id::now_ms().saturating_sub(3_600_000),
                "last_event_ms": id::now_ms().saturating_sub(3_600_000),
                "confidence": "exact",
                "discovered_only": false,
                "transcript_path": home.join(".gemini/antigravity-cli/brain/4b808b49-48a6-4aaa-9eab-15d68e18f657/.system_generated/logs/transcript.jsonl").to_string_lossy()
            }
        }
    });
    std::fs::write(
        state_dir.join("state.json"),
        serde_json::to_string(&initial_state).unwrap(),
    )
    .unwrap();

    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.set_home_dir_override(Some(home.clone()));

    // 1. Spawn a brand-new AGY interactive session in default `~/.agent_session` with `gemini-4-neon` + `xhigh`!
    let spawned_agy: Value = core
        .spawn_direct_session(
            AgentKind::Agy,
            None,
            None, // defaults to `<home>/.agent_session`
            None, // defaults to `agent-session`
            Some("gemini-4-neon".into()),
            Some(Effort::Xhigh),
            None,
            None,
        )
        .await
        .expect("spawn empty agy session");
    let agy_sid = SessionId::new(spawned_agy["session_id"].as_str().unwrap());

    // 2. Also spawn empty Codex and Claude sessions in the same `~/.agent_session`.
    let spawned_cx = core
        .spawn_direct_session(
            AgentKind::Codex,
            None,
            None,
            None,
            Some("gpt-6.2-sol".into()),
            Some(Effort::Xhigh),
            None,
            None,
        )
        .await
        .expect("spawn empty codex session");
    let cx_sid = SessionId::new(spawned_cx["session_id"].as_str().unwrap());

    let spawned_cl = core
        .spawn_direct_session(
            AgentKind::Claude,
            None,
            None,
            None,
            Some("claude-opus-5-5".into()),
            Some(Effort::Max),
            None,
            None,
        )
        .await
        .expect("spawn empty claude session");
    let cl_sid = SessionId::new(spawned_cl["session_id"].as_str().unwrap());

    // Run multiple discovery ticks (as the 3s background scanner does).
    core.sync_discovered_panes().await;
    core.sync_discovered_panes().await;

    let sessions = core.snapshot().sessions;
    let agy_rec = sessions
        .iter()
        .find(|s| s.session_id == agy_sid)
        .expect("agy session present");
    assert_eq!(
        agy_rec.transcript_path, None,
        "empty spawned AGY session must NOT attach 4b808b49 or b02fefac: {agy_rec:?}"
    );
    assert_eq!(
        agy_rec.metrics, None,
        "empty spawned AGY session must NOT inherit 93.2 tok/s cc-switch metrics: {agy_rec:?}"
    );
    assert_eq!(agy_rec.model.as_deref(), Some("gemini-4-neon-xhigh"));
    assert_eq!(agy_rec.effort.as_deref(), Some("xhigh"));

    let agy_tr = core
        .session_transcript(&agy_sid, TranscriptQuery::tail(60))
        .await
        .unwrap();
    assert_eq!(agy_tr.source, "none");
    assert!(agy_tr.messages.is_empty());
    assert!(agy_tr.metrics.is_none());

    for (sid, expected_model, expected_effort) in [
        (&cx_sid, "gpt-6.2-sol", "xhigh"),
        (&cl_sid, "claude-opus-5-5", "max"),
    ] {
        let rec = sessions.iter().find(|s| &s.session_id == sid).unwrap();
        assert_eq!(rec.transcript_path, None, "{rec:?}");
        assert_eq!(rec.metrics, None, "{rec:?}");
        assert_eq!(rec.model.as_deref(), Some(expected_model));
        assert_eq!(rec.effort.as_deref(), Some(expected_effort));
        let tr = core
            .session_transcript(sid, TranscriptQuery::tail(60))
            .await
            .unwrap();
        assert_eq!(tr.source, "none");
        assert!(tr.messages.is_empty());
    }

    // 3. Spawn a SECOND AGY pane in `~/.agent_session` -> both AGY panes share `~/.agent_session`
    //    with zero cross-contamination!
    let spawned_agy2 = core
        .spawn_direct_session(
            AgentKind::Agy,
            None,
            None,
            None,
            Some("gemini-4-carbon".into()),
            Some(Effort::High),
            None,
            None,
        )
        .await
        .expect("spawn second agy session");
    let agy2_sid = SessionId::new(spawned_agy2["session_id"].as_str().unwrap());
    let agy2_pane = spawned_agy2["pane_id"].as_str().unwrap().to_string();

    core.sync_discovered_panes().await;
    let sessions2 = core.snapshot().sessions;
    let rec2 = sessions2.iter().find(|s| s.session_id == agy2_sid).unwrap();
    assert_eq!(rec2.transcript_path, None);
    assert_eq!(rec2.model.as_deref(), Some("gemini-4-carbon-high"));
    assert_eq!(rec2.effort.as_deref(), Some("high"));

    // 4. Now simulate Turn #1 arriving via hook on ONLY the second AGY pane (`agy2_pane`):
    //    `agy2` upgrades cleanly to its real session UUID & transcript, while `agy1` stays 100% empty!
    let real_new_id = "new-turn-conv-0001-2222-3333-444444444444";
    let real_new_tr = home
        .join(".gemini/antigravity-cli/brain")
        .join(real_new_id)
        .join(".system_generated/logs/transcript.jsonl");
    std::fs::create_dir_all(real_new_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &real_new_tr,
        r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-25T07:00:00Z","content":"<USER_REQUEST>\n全新会话的第一句话\n</USER_REQUEST>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T07:00:02Z","model":"gemini-4-carbon-high","content":"全新会话的回复"}
"#,
    )
    .unwrap();

    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Agy,
        hook_event: Some("AfterAgent".into()),
        payload: json!({
            "conversationId": real_new_id,
            "hook_event_name": "AfterAgent",
            "modelName": "gemini-4-carbon-high",
            "cwd": cwd_str,
            "transcriptPath": real_new_tr.to_string_lossy()
        }),
        env: EnvCtx {
            tmux_pane: Some(agy2_pane),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;

    core.sync_discovered_panes().await;

    // `agy1` MUST STILL BE 100% EMPTY!
    let agy1_after = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id == agy_sid)
        .expect("agy1 still present");
    assert_eq!(agy1_after.transcript_path, None);
    assert_eq!(agy1_after.metrics, None);
    assert_eq!(agy1_after.model.as_deref(), Some("gemini-4-neon-xhigh"));
    assert_eq!(agy1_after.effort.as_deref(), Some("xhigh"));

    // `real_new_id` has its exact 2 messages!
    let new_tr_resp = core
        .session_transcript(&SessionId::new(real_new_id), TranscriptQuery::tail(60))
        .await
        .unwrap();
    assert_eq!(new_tr_resp.messages.len(), 2);
    assert_eq!(new_tr_resp.messages[0].text, "全新会话的第一句话");
}

#[tokio::test]
async fn startup_self_heals_poisoned_discovered_only_snapshot_and_binds_unique_post_birth_turn() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let root = std::env::temp_dir().join(format!("sf-iso-heal-{}", id::new_id("T")));
    let home = root.join("home");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: None,
    };

    let agent_dir = seed_historical_agent_session_data(&home);
    let cwd_str = agent_dir.to_string_lossy().into_owned();
    let poisoned_tp = home
        .join(".gemini/antigravity-cli/brain/4b808b49-48a6-4aaa-9eab-15d68e18f657/.system_generated/logs/transcript.jsonl")
        .to_string_lossy()
        .into_owned();

    // Fake tmux reporting `%50` running `agy --model gemini-4-neon-xhigh --effort xhigh`
    // under the current test process PID (whose OS birth time is NOW, >20h after `4b808b49`!).
    let self_pid = std::process::id();
    let pane_file = root.join("panes.txt");
    let row = [
        "%50",
        "agent-session",
        "@1",
        "0",
        "0",
        &self_pid.to_string(),
        "/Users/demo/.local/bin/agy --model gemini-4-neon-xhigh --effort xhigh",
        &cwd_str,
        "1",
        "1",
        "1",
        "0",
        "0",
        "0",
        "0",
        "0",
        "0",
        "agy-1",
    ]
    .join("\x1f");
    std::fs::write(&pane_file, format!("{row}\n")).unwrap();

    let fake_tmux = bin_dir.join("fake-tmux.sh");
    write_executable(
        &fake_tmux,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"list-panes\" ]; then cat \"{}\"; exit 0; fi\nif [ \"$1\" = \"capture-pane\" ]; then echo \"Antigravity CLI\"; exit 0; fi\nexit 0\n",
            pane_file.display()
        ),
    );

    // Seed an ALREADY-POISONED `state.json` identical to live `tmux-%50` before the fix!
    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now_ms = id::now_ms();
    let ended_since_ms = now_ms.saturating_sub(2 * 3_600_000);
    let ended_event_ms = now_ms.saturating_sub(3 * 3_600_000);
    let poisoned_snapshot = json!({
        "sessions": {
            "4b808b49-48a6-4aaa-9eab-15d68e18f657": {
                "session_id": "4b808b49-48a6-4aaa-9eab-15d68e18f657",
                "agent": "agy",
                "pane_resolution": "unknown",
                "cwd": cwd_str,
                "model": "gemini-4-neon-high",
                "effort": "high",
                "state": "ended",
                "state_since_ms": ended_since_ms,
                "last_event_ms": ended_event_ms,
                "confidence": "exact",
                "discovered_only": false,
                "transcript_path": poisoned_tp
            },
            "tmux-%50": {
                "session_id": "tmux-%50",
                "agent": "agy",
                "pane": "%50",
                "pane_resolution": "env",
                "cwd": cwd_str,
                "model": "gemini-4-neon-high",
                "effort": "high",
                "state": "idle",
                "state_since_ms": now_ms,
                "last_event_ms": now_ms,
                "confidence": "heuristic",
                "tmux_target": "agent-session:0.0",
                "tmux_label": "agent-session-agy-1",
                "discovered_only": true,
                "transcript_path": poisoned_tp,
                "metrics": {
                    "context_used_tokens": 72736,
                    "context_window_tokens": 512000,
                    "context_used_pct": 14.2,
                    "tps": 93.2,
                    "source": "cc_switch",
                    "updated_ms": ended_event_ms
                }
            }
        }
    });
    std::fs::write(
        state_dir.join("state.json"),
        serde_json::to_string(&poisoned_snapshot).unwrap(),
    )
    .unwrap();

    let mut config = StarfaneConfig::default();
    config.paths.tmux = fake_tmux.to_string_lossy().into_owned();
    config.zellij.enabled = false;

    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.set_home_dir_override(Some(home.clone()));

    // Even BEFORE `sync_discovered_panes()` runs, `session_transcript` blocks reading the stolen `Ended` session's file!
    let pre_sync_tr = core
        .session_transcript(&SessionId::new("tmux-%50"), TranscriptQuery::tail(60))
        .await
        .unwrap();
    assert_eq!(pre_sync_tr.source, "none");
    assert!(pre_sync_tr.messages.is_empty());

    // Now run `sync_discovered_panes()` -> `tmux-%50` is scrubbed clean & restored to `gemini-4-neon-xhigh`!
    core.sync_discovered_panes().await;
    let healed = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "tmux-%50")
        .expect("tmux-%50 present");
    assert_eq!(
        healed.transcript_path, None,
        "must scrub poisoned transcript_path"
    );
    assert_eq!(
        healed.metrics, None,
        "must scrub poisoned cc-switch metrics"
    );
    assert_eq!(healed.model.as_deref(), Some("gemini-4-neon-xhigh"));
    assert_eq!(healed.effort.as_deref(), Some("xhigh"));

    // Now simulate a genuinely NEW AGY conversation created AFTER `%50`'s birth in `~/.agent_session`
    // (even without hooks or `lsof` lock!) -> binds cleanly!
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now_iso = "2099-01-01T00:00:05Z";
    let post_id = "post-birth-conv-aaaa-bbbb-cccccccccccc";
    let post_tr = home
        .join(".gemini/antigravity-cli/brain")
        .join(post_id)
        .join(".system_generated/logs/transcript.jsonl");
    std::fs::create_dir_all(post_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &post_tr,
        format!(
            r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"{now_iso}","content":"<USER_REQUEST>\n新建后的真实提问\n</USER_REQUEST>"}}
"#
        ),
    )
    .unwrap();
    assert!(run_sqlite(
        &home.join(".gemini/antigravity-cli/conversation_summaries.db"),
        &format!(
            "INSERT INTO conversation_summaries VALUES ('{post_id}', 'New Post-Birth Chat', '', 1, '2099-01-01 00:00:05.000000+00:00', '[\"file://{cwd_str}\"]', 'CASCADE_RUN_STATUS_IDLE', '', '', 0, 0, '2099-01-01 00:00:05.000000+00:00');"
        ),
    ));

    core.sync_discovered_panes().await;
    let bound = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "tmux-%50")
        .unwrap();
    assert_eq!(
        bound.transcript_path.as_deref(),
        Some(post_tr.to_string_lossy().as_ref())
    );
    let bound_tr = core
        .session_transcript(&SessionId::new("tmux-%50"), TranscriptQuery::tail(60))
        .await
        .unwrap();
    assert_eq!(bound_tr.messages.len(), 1);
    assert_eq!(bound_tr.messages[0].text, "新建后的真实提问");
}

#[tokio::test]
async fn codex_title_collision_two_pane_ambiguity_and_same_pane_pid_restart_isolation() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let root = std::env::temp_dir().join(format!("sf-iso-codex-restart-{}", id::new_id("T")));
    let home = root.join("home");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: None,
    };

    let agent_dir = seed_historical_agent_session_data(&home);
    let cwd_str = agent_dir.to_string_lossy().into_owned();

    let self_pid = std::process::id();
    let pane_file = root.join("panes.txt");
    let tail_file = root.join("tail.txt");
    // Even if tmux scrollback still shows yesterday's Codex status title `旧的 codex 会话`,
    // a newly born Codex process (`self_pid`) MUST NOT attach yesterday's `1795488950` rollout!
    std::fs::write(
        &tail_file,
        "› Ask Codex to do anything\n  GPT-6.2-Sol xhigh · ~/.agent_session · 旧的 codex 会话\n",
    )
    .unwrap();
    let mk_row = |pane: &str, pid: u32, cmd: &str, win: &str| {
        [
            pane,
            "agent-session",
            "@1",
            "0",
            "0",
            &pid.to_string(),
            cmd,
            &cwd_str,
            "1",
            "1",
            "1",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            win,
        ]
        .join("\x1f")
    };
    std::fs::write(
        &pane_file,
        format!(
            "{}\n",
            mk_row(
                "%61",
                self_pid,
                "codex -m gpt-6.2-sol -c model_reasoning_effort=\"xhigh\"",
                "codex-1"
            )
        ),
    )
    .unwrap();

    let fake_tmux = bin_dir.join("fake-tmux.sh");
    write_executable(
        &fake_tmux,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"list-panes\" ]; then cat \"{}\"; exit 0; fi\nif [ \"$1\" = \"capture-pane\" ]; then cat \"{}\"; exit 0; fi\nexit 0\n",
            pane_file.display(),
            tail_file.display()
        ),
    );

    let mut config = StarfaneConfig::default();
    config.paths.tmux = fake_tmux.to_string_lossy().into_owned();
    config.zellij.enabled = false;
    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.set_home_dir_override(Some(home.clone()));

    // 1. Pre-birth Codex thread with matching status title is rejected!
    core.sync_discovered_panes().await;
    let s1 = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "tmux-%61")
        .unwrap();
    assert_eq!(s1.transcript_path, None);
    assert_eq!(s1.metrics, None);
    assert_eq!(s1.model.as_deref(), Some("gpt-6.2-sol"));
    assert_eq!(s1.effort.as_deref(), Some("xhigh"));

    // 2. Insert a genuinely post-birth Codex thread (`created_at` & `updated_at` in future/now) ->
    //    when TWO unresolved Codex panes share `~/.agent_session`, neither steals it; when ONE
    //    unresolved pane is in `~/.agent_session`, it binds cleanly!
    let new_cx_id = "01b29999-aaaa-7820-9614-111122223333";
    let new_cx_tr = home
        .join(".codex/sessions/2026/11/25")
        .join(format!("rollout-2026-11-25T15-00-00-{new_cx_id}.jsonl"));
    std::fs::create_dir_all(new_cx_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &new_cx_tr,
        r#"{"timestamp":"2099-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"全新的 codex 提问"}]}}
"#,
    )
    .unwrap();
    let future_sec = (id::now_ms() / 1000) + 60;
    assert!(run_sqlite(
        &home.join(".codex/state_5.sqlite"),
        &format!(
            "INSERT INTO threads VALUES ('{new_cx_id}', '{}', {future_sec}, {future_sec}, '{cwd_str}', '全新的 codex 提问', 0);",
            new_cx_tr.display()
        ),
    ));
    std::fs::write(
        &tail_file,
        "› Ask Codex to do anything\n  GPT-6.2-Sol xhigh · ~/.agent_session · 全新的 codex 提问\n",
    )
    .unwrap();

    // Two Codex panes (`%61` and `%62`) in same `cwd` -> ambiguous -> neither guesses!
    std::fs::write(
        &pane_file,
        format!(
            "{}\n{}\n",
            mk_row("%61", self_pid, "codex -m gpt-6.2-sol", "codex-1"),
            mk_row("%62", self_pid, "codex -m gpt-6.2-sol", "codex-2")
        ),
    )
    .unwrap();
    core.sync_discovered_panes().await;
    for sid in ["tmux-%61", "tmux-%62"] {
        let rec = core
            .snapshot()
            .sessions
            .into_iter()
            .find(|s| s.session_id.as_str() == sid)
            .unwrap();
        assert_eq!(
            rec.transcript_path, None,
            "ambiguous same-cwd panes must not guess"
        );
    }

    // Close `%62` -> sole Codex pane `%61` binds its post-birth thread cleanly!
    std::fs::write(
        &pane_file,
        format!(
            "{}\n",
            mk_row("%61", self_pid, "codex -m gpt-6.2-sol", "codex-1")
        ),
    )
    .unwrap();
    core.sync_discovered_panes().await;
    let bound = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "tmux-%61")
        .unwrap();
    assert_eq!(
        bound.transcript_path.as_deref(),
        Some(new_cx_tr.to_string_lossy().as_ref())
    );

    // 3. Simulate exiting `codex` (PID `self_pid`) and launching a new `codex` process (different PID)
    //    inside the SAME pane `%61` with an empty prompt -> `restarted_panes` clears the old transcript!
    std::fs::write(
        &tail_file,
        "› Ask Codex to do anything\n  GPT-5-Codex low · ~/.agent_session\n",
    )
    .unwrap();
    std::fs::write(
        &pane_file,
        format!(
            "{}\n",
            mk_row(
                "%61",
                self_pid.saturating_add(77_777),
                "codex -m gpt-5-codex -c model_reasoning_effort=\"low\"",
                "codex-1"
            )
        ),
    )
    .unwrap();
    core.sync_discovered_panes().await;
    let restarted = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "tmux-%61")
        .unwrap();
    assert_eq!(
        restarted.transcript_path, None,
        "new agent_pid in same pane must reset stale transcript_path"
    );
    assert_eq!(restarted.metrics, None);
    assert_eq!(restarted.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(restarted.effort.as_deref(), Some("low"));
}
