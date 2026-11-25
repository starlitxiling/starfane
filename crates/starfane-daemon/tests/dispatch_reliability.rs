//! Regression tests for message dispatch & delivery reliability (swallowed-message fixes):
//! - Auto-exiting tmux `copy-mode` on explicit `interactive: true` sends (reproducing `teleop-sharpa`)
//! - Never silently dumping blocked interactive sends into dead `L0Inbox` ghost entries
//! - Post-injection stuck-in-prompt detection, safe Enter retry, and honest `Unconfirmed` fallback
//! - Copy-mode race detection during injection
//! - Per-pane injection serialization (preventing interleaved paste + Enter)
//! - Expiring stale ad-hoc `InboxKind::Message` items so `poke_pending_inbox` never replays old prompts

use serde_json::json;
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(1);

struct TestEnv {
    root: PathBuf,
    repo: String,
    core: Core,
    tmux_socket: Option<String>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if let Some(sock) = &self.tmux_socket {
            let _ = Command::new("tmux")
                .args(["-L", sock, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn make_env(tmux_bin: &str, tmux_socket: Option<String>, paste_settle_ms: u64) -> TestEnv {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("starfane-disp-rel-{}-{n}", std::process::id()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_str = std::fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut cfg = StarfaneConfig::default();
    cfg.paths.tmux = tmux_bin.to_string();
    cfg.tmux.socket_name = tmux_socket.clone().unwrap_or_default();
    cfg.zellij.enabled = false;
    cfg.gate.paste_settle_ms = paste_settle_ms;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    TestEnv {
        root,
        repo: repo_str,
        core,
        tmux_socket,
    }
}

fn emit_agy_stop(pane: &str, conv_id: &str, cwd: &str) -> EmitRequest {
    EmitRequest {
        v: 1,
        agent: AgentKind::Agy,
        hook_event: Some("Stop".into()),
        payload: json!({
            "conversationId": conv_id,
            "cwd": cwd,
            "terminationReason": "NO_TOOL_CALL",
            "fullyIdle": true,
            "continuationActive": false
        }),
        env: EnvCtx {
            tmux_pane: Some(pane.into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    }
}

fn fake_pane_line(pane_id: &str, in_mode: u8, dead: u8, sync: u8, cursor_y: u32) -> String {
    [
        pane_id.to_string(),
        "teleop".to_string(),
        "@39".to_string(),
        "5".to_string(),
        "0".to_string(),
        "12345".to_string(),
        "agy".to_string(),
        "/tmp".to_string(),
        "1".to_string(),
        "1".to_string(),
        "1".to_string(),
        in_mode.to_string(),
        dead.to_string(),
        sync.to_string(),
        "0".to_string(),
        "2".to_string(),
        cursor_y.to_string(),
        "sharpa".to_string(),
        String::new(),
        "120".to_string(),
        "30".to_string(),
    ]
    .join("\x1f")
}

#[tokio::test]
async fn interactive_dispatch_auto_exits_real_tmux_copy_mode_and_delivers() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let sock = format!("starfane-test-copymode-{}-{n}", std::process::id());
    let env = make_env("tmux", Some(sock.clone()), 80);

    let ready = env.root.join("ready");
    let got = env.root.join("got.txt");
    let script = format!(
        ": > '{ready}'; IFS= read -r l1; IFS= read -r l2; printf '%s\\n%s' \"$l1\" \"$l2\" > '{got}'; sleep 30",
        ready = ready.display(),
        got = got.display()
    );
    let out = Command::new("tmux")
        .args([
            "-L",
            &sock,
            "new-session",
            "-d",
            "-s",
            "teleop",
            "-n",
            "sharpa",
            "-P",
            "-F",
            "#{pane_id}",
            "/bin/sh",
            "-c",
            &script,
        ])
        .output()
        .expect("tmux new-session");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    for _ in 0..100 {
        if ready.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready.exists(), "pane shell did not start");

    // Put pane into tmux copy-mode (exact root cause of the teleop-sharpa incident).
    let cm = Command::new("tmux")
        .args(["-L", &sock, "copy-mode", "-t", &pane_id])
        .status()
        .expect("tmux copy-mode");
    assert!(cm.success());

    env.core
        .ingest(emit_agy_stop(&pane_id, "sharpa-cm-test", &env.repo))
        .await;

    let report: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("sharpa-cm-test"),
                },
                text: "第一行：取消hybrid模式\n第二行：确认4踏板按键映射".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();

    assert_eq!(report.level, DeliveryLevel::L2Inject);
    assert_eq!(report.status, DeliveryStatus::Delivered, "{report:?}");
    assert!(
        report
            .ack
            .as_deref()
            .is_some_and(|a| a.contains("exited_copy_mode")),
        "expected exited_copy_mode ack, got {report:?}"
    );

    // Verify no stale ghost entry was dumped into inbox.
    let inbox: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("sharpa-cm-test"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        inbox.is_empty(),
        "must not leave ghost inbox item: {inbox:?}"
    );

    // Verify the multiline UTF-8 text actually reached the process in the pane.
    for _ in 0..100 {
        if got.exists()
            && std::fs::metadata(&got)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let received = std::fs::read_to_string(&got).unwrap_or_default();
    assert_eq!(
        received,
        "第一行：取消hybrid模式\n第二行：确认4踏板按键映射"
    );
}

#[tokio::test]
async fn interactive_dispatch_hard_gate_failure_returns_failed_without_ghost_inbox() {
    let env_dir = std::env::temp_dir().join(format!("starfane-hardgate-{}", id::new_id("T")));
    std::fs::create_dir_all(&env_dir).unwrap();
    let fake_tmux = env_dir.join("fake-tmux.sh");
    let pane_row = fake_pane_line("%45", 0, 0, 1, 2); // synchronized = 1 (hard block)

    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
if [ "$1" = "list-panes" ]; then
  printf '%s\n' '{pane}'
  exit 0
fi
exit 0
"#,
            pane = pane_row
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None, 10);
    env.core
        .ingest(emit_agy_stop("%45", "sharpa-sync-blocked", &env.repo))
        .await;

    let report: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("sharpa-sync-blocked"),
                },
                text: "绝不能被静默吞掉的重要消息".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();

    assert_eq!(report.level, DeliveryLevel::L2Inject);
    assert_eq!(report.status, DeliveryStatus::Failed, "{report:?}");
    let detail = report
        .gate
        .as_ref()
        .and_then(|g| g.detail.as_deref())
        .unwrap_or("");
    assert!(
        detail.contains("synchronize-panes"),
        "expected gate detail to explain blocker: {report:?}"
    );

    // Must NOT dump into L0Inbox (which would leave a ghost unread entry after user retries).
    let inbox: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("sharpa-sync-blocked"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        inbox.is_empty(),
        "expected zero ghost inbox items: {inbox:?}"
    );

    let _ = std::fs::remove_dir_all(&env_dir);
}

#[tokio::test]
async fn interactive_dispatch_retries_enter_when_stuck_and_reports_unconfirmed_if_persistent() {
    let dir = std::env::temp_dir().join(format!("starfane-stuck-{}", id::new_id("T")));
    std::fs::create_dir_all(&dir).unwrap();
    let enter_count_file = dir.join("enter_count");
    let mode_file = dir.join("mode");
    let fake_tmux = dir.join("fake-tmux.sh");
    let pane_row = fake_pane_line("%45", 0, 0, 0, 2);

    std::fs::write(&enter_count_file, "0").unwrap();
    std::fs::write(&mode_file, "recover_on_second").unwrap();
    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
CNT_FILE="{cnt}"
MODE_FILE="{mode}"
case "$1" in
  list-panes)
    printf '%s\n' '{pane}'
    ;;
  load-buffer)
    cat >/dev/null
    ;;
  paste-buffer)
    exit 0
    ;;
  send-keys)
    n=$(cat "$CNT_FILE")
    n=$((n + 1))
    echo "$n" > "$CNT_FILE"
    ;;
  capture-pane)
    n=$(cat "$CNT_FILE")
    m=$(cat "$MODE_FILE")
    if [ "$n" -eq 0 ]; then
      printf '前一条回复\n────────────────────\n>\n────────────────────\n? for shortcuts\n'
    elif [ "$m" = "recover_on_second" ] && [ "$n" -ge 2 ]; then
      printf '> 请帮我核对四踏板按键映射表\n────────────────────\n>\n────────────────────\n? for shortcuts\n'
    else
      printf '前一条回复\n────────────────────\n> 请帮我核对四踏板按键映射表\n────────────────────\n? for shortcuts\n'
    fi
    ;;
esac
exit 0
"#,
            cnt = enter_count_file.display(),
            mode = mode_file.display(),
            pane = pane_row
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None, 10);
    env.core
        .ingest(emit_agy_stop("%45", "sharpa-stuck-test", &env.repo))
        .await;

    // Case A: 1st Enter eaten by paste-burst, 2nd Enter submits cleanly -> Delivered!
    let rep_ok: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("sharpa-stuck-test"),
                },
                text: "请帮我核对四踏板按键映射表".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rep_ok.status, DeliveryStatus::Delivered, "{rep_ok:?}");
    let enters_a: u32 = std::fs::read_to_string(&enter_count_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(enters_a, 2, "expected initial Enter + 1 retry Enter");

    // Case B: Text stays stuck even after retries -> honestly returns Unconfirmed (`stuck_in_prompt`)!
    std::fs::write(&enter_count_file, "0").unwrap();
    std::fs::write(&mode_file, "always_stuck").unwrap();
    let rep_unconf: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("sharpa-stuck-test"),
                },
                text: "请帮我核对四踏板按键映射表".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        rep_unconf.status,
        DeliveryStatus::Unconfirmed,
        "{rep_unconf:?}"
    );
    assert_eq!(rep_unconf.ack.as_deref(), Some("stuck_in_prompt"));
    let enters_b: u32 = std::fs::read_to_string(&enter_count_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(enters_b, 3, "expected initial Enter + 2 retry Enters");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn interactive_dispatch_detects_copy_mode_race_during_injection() {
    let dir = std::env::temp_dir().join(format!("starfane-cmrace-{}", id::new_id("T")));
    std::fs::create_dir_all(&dir).unwrap();
    let pasted_flag = dir.join("pasted");
    let fake_tmux = dir.join("fake-tmux.sh");
    let pane_ok = fake_pane_line("%45", 0, 0, 0, 2);
    let pane_cm = fake_pane_line("%45", 1, 0, 0, 2);

    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
FLAG="{flag}"
case "$1" in
  list-panes)
    if [ -f "$FLAG" ]; then
      printf '%s\n' '{pane_cm}'
    else
      printf '%s\n' '{pane_ok}'
    fi
    ;;
  load-buffer)
    cat >/dev/null
    ;;
  paste-buffer)
    : > "$FLAG"
    ;;
  capture-pane)
    printf '前一条回复\n────────────────────\n>\n────────────────────\n'
    ;;
esac
exit 0
"#,
            flag = pasted_flag.display(),
            pane_ok = pane_ok,
            pane_cm = pane_cm
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None, 10);
    env.core
        .ingest(emit_agy_stop("%45", "sharpa-cm-race", &env.repo))
        .await;

    let rep: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("sharpa-cm-race"),
                },
                text: "测试复制模式竞态检测".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rep.status, DeliveryStatus::Unconfirmed, "{rep:?}");
    assert_eq!(rep.ack.as_deref(), Some("copy_mode_race"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn concurrent_interactive_dispatches_to_same_pane_are_serialized() {
    let dir = std::env::temp_dir().join(format!("starfane-serial-{}", id::new_id("T")));
    std::fs::create_dir_all(&dir).unwrap();
    let ops_log = dir.join("ops.log");
    let fake_tmux = dir.join("fake-tmux.sh");
    let pane_row = fake_pane_line("%45", 0, 0, 0, 2);

    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
LOG="{log}"
case "$1" in
  list-panes)
    printf '%s\n' '{pane}'
    ;;
  load-buffer)
    body=$(cat)
    echo "PASTE:$body" >> "$LOG"
    ;;
  paste-buffer)
    exit 0
    ;;
  send-keys)
    echo "ENTER" >> "$LOG"
    ;;
esac
exit 0
"#,
            log = ops_log.display(),
            pane = pane_row
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None, 120);
    env.core
        .ingest(emit_agy_stop("%45", "sharpa-serial", &env.repo))
        .await;

    let c1 = env.core.clone();
    let c2 = env.core.clone();
    let (r1, r2) = tokio::join!(
        c1.handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new("sharpa-serial"),
            },
            text: "MSG_ONE".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        }),
        c2.handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new("sharpa-serial"),
            },
            text: "MSG_TWO".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
    );
    assert!(r1.is_ok() && r2.is_ok());

    let lines: Vec<String> = std::fs::read_to_string(&ops_log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 4, "ops={lines:?}");
    assert!(lines[0].starts_with("PASTE:"), "ops={lines:?}");
    assert_eq!(
        lines[1], "ENTER",
        "must not interleave two pastes: {lines:?}"
    );
    assert!(lines[2].starts_with("PASTE:"), "ops={lines:?}");
    assert_eq!(lines[3], "ENTER", "ops={lines:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stale_inbox_message_is_expired_and_not_replayed_on_turn_complete() {
    let dir = std::env::temp_dir().join(format!("starfane-stale-poke-{}", id::new_id("T")));
    std::fs::create_dir_all(dir.join("state")).unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let poke_log = dir.join("poke.log");
    let fake_tmux = dir.join("fake-tmux.sh");
    let pane_row = fake_pane_line("%45", 0, 0, 0, 2);

    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
LOG="{log}"
case "$1" in
  list-panes)
    printf '%s\n' '{pane}'
    ;;
  list-clients)
    exit 0
    ;;
  capture-pane)
    printf '>\n'
    ;;
  load-buffer|paste-buffer|send-keys)
    echo "$*" >> "$LOG"
    cat >/dev/null || true
    ;;
esac
exit 0
"#,
            log = poke_log.display(),
            pane = pane_row
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Seed a persisted snapshot containing a 20-minute-old ghost inbox message (like teleop-sharpa).
    let old_ts = id::now_ms().saturating_sub(20 * 60 * 1000);
    let snap = json!({
        "sessions": {
            "sharpa-stale": {
                "session_id": "sharpa-stale",
                "agent": "agy",
                "pane": "%45",
                "pane_resolution": "env",
                "cwd": repo.to_string_lossy(),
                "state": "working",
                "state_since_ms": old_ts,
                "last_event_ms": old_ts,
                "is_subagent": false,
                "subagent_count": 0,
                "active_subagents": 0,
                "unread_inbox": 1,
                "confidence": "exact",
                "discovered_only": false
            }
        },
        "inbox": {
            "sharpa-stale": [
                {
                    "item": {
                        "id": "M-OLD-GHOST",
                        "kind": "message",
                        "created_ms": old_ts,
                        "title": "old swallowed prompt",
                        "body_md": "old swallowed prompt that user already retyped"
                    },
                    "delivered": false,
                    "poke_pending": true
                }
            ]
        }
    });
    std::fs::write(
        dir.join("state/state.json"),
        serde_json::to_string_pretty(&snap).unwrap(),
    )
    .unwrap();

    let mut cfg = StarfaneConfig::default();
    cfg.paths.tmux = fake_tmux.to_string_lossy().into_owned();
    cfg.zellij.enabled = false;
    cfg.gate.paste_settle_ms = 10;
    let core = Core::new(&DaemonConfig {
        state_dir: dir.join("state"),
        socket: dir.join("state/starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Trigger TurnComplete -> poke_pending_inbox runs -> must expire the stale message, NOT replay it!
    core.ingest(emit_agy_stop(
        "%45",
        "sharpa-stale",
        &repo.to_string_lossy(),
    ))
    .await;

    // Confirm Stop hook actually transitioned sharpa-stale from Working -> Idle.
    let rec = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "sharpa-stale")
        .expect("sharpa-stale exists");
    assert_eq!(rec.state, SessionState::Idle);

    tokio::time::sleep(Duration::from_millis(700)).await;
    let poked = std::fs::read_to_string(&poke_log).unwrap_or_default();
    assert!(
        poked.trim().is_empty(),
        "stale 20-min-old inbox message must NOT be replayed on Stop: {poked}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
