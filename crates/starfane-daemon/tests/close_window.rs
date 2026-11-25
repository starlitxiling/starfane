//! End-to-end integration tests for safe pane/window closing (`POST /api/sessions/{id}/close`
//! and `Core::close_session_window`):
//! - Closing a 1-pane tmux window kills the pane & closes the window cleanly, preserves sibling
//!   windows in the same session, marks the session `Ended` immediately, clears `last_error` &
//!   pending attention, persists `state.json`, and emits SSE `event: session` + `event: attention`.
//! - Closing 1 pane of a 2-pane split window kills ONLY that pane while the sibling pane stays alive.
//! - Rejecting unknown sessions (`404`), subagents (`400`), and pane-less sessions (`400`).
//! - Enforcing admin write auth: `read_only` scoped tokens receive `403 Forbidden` and never kill
//!   the target pane; unauthenticated calls receive `401 Unauthorized`.
//! - Proxying close requests across `PeerDaemonNodeProvider` (`Hub -> Peer /api/sessions/{id}/close`).

use serde_json::{json, Value};
use starfane_daemon::config::{
    NodeConfig, NodeKindConfig, ScopedTokenConfig, StarfaneConfig, WebConfig,
};
use starfane_daemon::{Core, DaemonConfig};
use starfane_proto::{
    attention, id, AgentKind, DaemonEvent, EmitRequest, EnvCtx, ErrorCode, PaneId, SessionId,
    SessionState,
};
use starfane_tmux::{SpawnWindow, Tmux};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct SandboxEnv {
    root: PathBuf,
    tmux_socket: Option<String>,
}

impl Drop for SandboxEnv {
    fn drop(&mut self) {
        if let Some(sock) = &self.tmux_socket {
            let _ = Command::new("tmux")
                .args(["-L", sock, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn mk_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("starfane-close-{tag}-{}", id::new_id("T")));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[cfg(unix)]
fn write_tmux_wrapper(dir: &Path, sock: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("tmux-wrap.sh");
    std::fs::write(&path, format!("#!/bin/sh\nexec tmux -L {sock} \"$@\"\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn raw_http(addr: SocketAddr, req: &str) -> (u16, String, String) {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    let full = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = full.split_once("\r\n\r\n").unwrap_or((&full, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
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

fn split_tmux_pane(sock: &str, target_pane: &PaneId, cwd: &Path) -> PaneId {
    let out = Command::new("tmux")
        .args([
            "-L",
            sock,
            "split-window",
            "-h",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            target_pane.as_str(),
            "-c",
            &cwd.to_string_lossy(),
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ])
        .output()
        .expect("tmux split-window");
    assert!(out.status.success(), "{out:?}");
    PaneId::new(String::from_utf8_lossy(&out.stdout).trim())
}

#[tokio::test]
async fn close_single_pane_window_kills_pane_marks_ended_persists_and_emits_sse() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let root = mk_dir("single-win");
    let sock = format!("sf-close-single-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let p_keep = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("teleop".into()),
            create_session: true,
            window_name: "keep".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        })
        .unwrap();
    let p_target = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("teleop".into()),
            create_session: false,
            window_name: "target".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        })
        .unwrap();

    let tmux_wrap = write_tmux_wrapper(&root, &sock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: addr.port(),
            auth_enabled: true,
            ..WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    cfg.zellij.enabled = false;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Seed two real hook-backed sessions: `s-keep` on `p_keep` and `s-target` (in NeedsPermission
    // with an error / pending attention item) on `p_target`.
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("Stop".into()),
        payload: json!({
            "session_id": "s-keep",
            "hook_event_name": "Stop",
            "cwd": root.to_string_lossy(),
        }),
        env: EnvCtx {
            tmux_pane: Some(p_keep.0.clone()),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("PermissionRequest".into()),
        payload: json!({
            "session_id": "s-target",
            "hook_event_name": "PermissionRequest",
            "cwd": root.to_string_lossy(),
            "tool_name": "Bash",
            "message": "waiting for permission",
        }),
        env: EnvCtx {
            tmux_pane: Some(p_target.0.clone()),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;

    let before_snap = core.snapshot();
    assert!(
        before_snap.attention.iter().any(|a| {
            a.kind == attention::BLOCKED
                && a.session_id.as_ref().map(|x| x.as_str()) == Some("s-target")
        }),
        "expected pending blocked attention item for s-target before close"
    );

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let mut sse_rx = core.subscribe();
    let (status, _, body) = raw_http(
        addr,
        &format!(
            "POST /api/sessions/s-target/close HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nX-Starfane-Token: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port(),
            addr.port(),
            core.web_token()
        ),
    )
    .await;
    assert_eq!(status, 200, "unexpected close response: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["closed"], true);
    assert_eq!(v["data"]["session_id"], "s-target");
    assert_eq!(v["data"]["pane"], p_target.as_str());
    assert!(v["data"].get("tmux_target").is_some());

    // 1. Target pane & window are gone in tmux, while `keep` window in the same session stays alive!
    let live_panes = sb_tmux.list_panes().unwrap();
    assert!(
        !live_panes.iter().any(|p| p.pane_id == p_target),
        "target pane must be killed"
    );
    assert!(
        live_panes.iter().any(|p| p.pane_id == p_keep),
        "sibling window in same tmux session must remain alive"
    );
    let live_windows = sb_tmux.list_windows().unwrap();
    assert_eq!(live_windows.len(), 1);
    assert_eq!(live_windows[0].window_name, "keep");

    // 2. Daemon state immediately marks `s-target` Ended, clears `last_error` & pending attention!
    let after_snap = core.snapshot();
    let ended = after_snap
        .sessions
        .iter()
        .find(|s| s.session_id.as_str() == "s-target")
        .expect("s-target remains in snapshot as Ended");
    assert_eq!(ended.state, SessionState::Ended);
    assert_eq!(ended.last_error, None);
    assert!(
        !after_snap
            .attention
            .iter()
            .any(|a| a.session_id.as_ref().map(|x| x.as_str()) == Some("s-target")),
        "pending attention for s-target must be cleared immediately"
    );

    // 3. SSE emitted `event: session` (Ended) and `event: attention` update immediately!
    let mut saw_ended_sse = false;
    let mut saw_attention_clear = false;
    while let Ok(ev) = sse_rx.try_recv() {
        match ev {
            DaemonEvent::Session { session, .. }
                if session.session_id.as_str() == "s-target"
                    && session.state == SessionState::Ended =>
            {
                saw_ended_sse = true;
            }
            DaemonEvent::Attention { items, .. }
                if !items
                    .iter()
                    .any(|a| a.session_id.as_ref().map(|x| x.as_str()) == Some("s-target")) =>
            {
                saw_attention_clear = true;
            }
            _ => {}
        }
    }
    assert!(saw_ended_sse, "expected immediate SSE session Ended event");
    assert!(
        saw_attention_clear,
        "expected immediate SSE attention clear event"
    );

    // 4. Snapshot persisted on disk (`<state_dir>/state.json`) immediately!
    let persisted: Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
            .unwrap();
    assert_eq!(persisted["sessions"]["s-target"]["state"], "ended");

    // 5. Re-closing an already-closed pane is idempotent (`200 OK`).
    let (status2, _, body2) = raw_http(
        addr,
        &format!(
            "POST /api/sessions/s-target/close HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nX-Starfane-Token: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port(),
            addr.port(),
            core.web_token()
        ),
    )
    .await;
    assert_eq!(status2, 200, "idempotent close should succeed: {body2}");

    server.abort();
}

#[tokio::test]
async fn close_one_pane_of_two_pane_split_window_keeps_sibling_pane_alive() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let root = mk_dir("split-win");
    let sock = format!("sf-close-split-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let p_left = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("teleop".into()),
            create_session: true,
            window_name: "work".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        })
        .unwrap();
    let p_right = split_tmux_pane(&sock, &p_left, &root);

    let tmux_wrap = write_tmux_wrapper(&root, &sock);
    let mut cfg = StarfaneConfig::default();
    cfg.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    cfg.discovery.include_shell_panes = true;
    cfg.zellij.enabled = false;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    core.sync_discovered_panes().await;
    let left_sid = SessionId::new(format!("tmux-{}", p_left.as_str()));
    let right_sid = SessionId::new(format!("tmux-{}", p_right.as_str()));

    let res = core.close_session_window(&left_sid).await.unwrap();
    assert_eq!(res["closed"], true);
    assert_eq!(res["session_id"], left_sid.as_str());
    assert_eq!(res["pane"], p_left.as_str());
    assert!(
        res["tmux_target"]
            .as_str()
            .is_some_and(|t| t.starts_with("teleop:")),
        "expected teleop target, got {res}"
    );

    // Only `p_left` was closed; sibling `p_right` MUST still be alive in tmux & daemon state!
    let live_panes = sb_tmux.list_panes().unwrap();
    assert_eq!(live_panes.len(), 1);
    assert_eq!(live_panes[0].pane_id, p_right);
    assert_eq!(live_panes[0].session_name, "teleop");
    assert_eq!(live_panes[0].window_name, "work");

    let snap = core.snapshot();
    let left_rec = snap
        .sessions
        .iter()
        .find(|s| s.session_id == left_sid)
        .unwrap();
    let right_rec = snap
        .sessions
        .iter()
        .find(|s| s.session_id == right_sid)
        .unwrap();
    assert_eq!(left_rec.state, SessionState::Ended);
    assert_ne!(right_rec.state, SessionState::Ended);
}

#[tokio::test]
async fn close_rejects_unknown_subagent_and_paneless_sessions() {
    let root = mk_dir("reject-invalid");
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: None,
    };

    let mut cfg = StarfaneConfig::default();
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // 1. Unknown session -> 404 NotFound.
    let err_missing = core
        .close_session_window(&SessionId::new("does-not-exist"))
        .await
        .unwrap_err();
    assert_eq!(err_missing.code, ErrorCode::NotFound);

    // 2. Pane-less root session -> 400 BadRequest.
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("Stop".into()),
        payload: json!({
            "session_id": "no-pane-root",
            "hook_event_name": "Stop",
            "cwd": root.to_string_lossy(),
        }),
        env: EnvCtx::default(),
        recv_ts_ms: id::now_ms(),
    })
    .await;
    let err_no_pane = core
        .close_session_window(&SessionId::new("no-pane-root"))
        .await
        .unwrap_err();
    assert_eq!(err_no_pane.code, ErrorCode::BadRequest);
    assert_eq!(
        err_no_pane.message,
        "cannot close subagent or session without terminal pane"
    );

    // 3. Subagent session (even when parent has a bound pane) -> 400 BadRequest.
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("SubagentStart".into()),
        payload: json!({
            "session_id": "parent-1",
            "agent_id": "sub-1",
            "hook_event_name": "SubagentStart",
            "cwd": root.to_string_lossy(),
        }),
        env: EnvCtx {
            tmux_pane: Some("%7".into()),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;
    let sub_sid = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.is_subagent)
        .expect("subagent recorded")
        .session_id;
    let err_sub = core.close_session_window(&sub_sid).await.unwrap_err();
    assert_eq!(err_sub.code, ErrorCode::BadRequest);
    assert_eq!(
        err_sub.message,
        "cannot close subagent or session without terminal pane"
    );
}

#[tokio::test]
async fn close_endpoint_enforces_admin_write_auth_and_blocks_readonly_tokens_with_403() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let root = mk_dir("auth-403");
    let sock = format!("sf-close-auth-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("auth-sess".into()),
            create_session: true,
            window_name: "win1".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        })
        .unwrap();

    let tmux_wrap = write_tmux_wrapper(&root, &sock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: addr.port(),
            auth_enabled: true,
            scoped_tokens: vec![ScopedTokenConfig {
                label: "viewer".into(),
                token: "ro-viewer-token".into(),
                token_env: String::new(),
                read_only: true,
            }],
            ..WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    cfg.discovery.include_shell_panes = true;
    cfg.zellij.enabled = false;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.sync_discovered_panes().await;
    let sid = format!("tmux-{}", pane.as_str());
    let enc_sid = pct_encode(&sid);

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // 1. Missing token -> 401 Unauthorized, pane untouched.
    let (st_unauth, _, _) = raw_http(
        addr,
        &format!(
            "POST /api/sessions/{enc_sid}/close HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port(),
            addr.port(),
        ),
    )
    .await;
    assert_eq!(st_unauth, 401);
    assert!(sb_tmux
        .list_panes()
        .unwrap()
        .iter()
        .any(|p| p.pane_id == pane));

    // 2. Read-only scoped token -> 403 Forbidden, pane untouched!
    let (st_ro, _, body_ro) = raw_http(
        addr,
        &format!(
            "POST /api/sessions/{enc_sid}/close HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nX-Starfane-Token: ro-viewer-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port(),
            addr.port(),
        ),
    )
    .await;
    assert_eq!(st_ro, 403, "{body_ro}");
    let ro_json: Value = serde_json::from_str(&body_ro).unwrap();
    assert_eq!(ro_json["ok"], false);
    assert_eq!(ro_json["error"]["code"], "forbidden");
    assert!(
        sb_tmux
            .list_panes()
            .unwrap()
            .iter()
            .any(|p| p.pane_id == pane),
        "read-only token must NEVER close the pane"
    );

    // 3. Admin token -> 200 OK, pane closed!
    let (st_admin, _, body_admin) = raw_http(
        addr,
        &format!(
            "POST /api/sessions/{enc_sid}/close HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port(),
            addr.port(),
            core.web_token()
        ),
    )
    .await;
    assert_eq!(st_admin, 200, "{body_admin}");
    assert!(sb_tmux.list_panes().unwrap().is_empty());

    server.abort();
}

#[tokio::test]
async fn close_proxies_through_peer_daemon_node_provider() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let root = mk_dir("peer-close");
    let peer_dir = root.join("peer");
    let hub_dir = root.join("hub");
    std::fs::create_dir_all(&peer_dir).unwrap();
    std::fs::create_dir_all(&hub_dir).unwrap();

    let sock = format!("sf-close-peer-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let peer_pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("remote-work".into()),
            create_session: true,
            window_name: "w1".into(),
            cwd: peer_dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        })
        .unwrap();
    let tmux_wrap = write_tmux_wrapper(&peer_dir, &sock);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let mut peer_cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: peer_addr.port(),
            auth_enabled: true,
            scoped_tokens: vec![ScopedTokenConfig {
                label: "hub".into(),
                token: "fed-admin-tok".into(),
                token_env: String::new(),
                read_only: false,
            }],
            ..WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    peer_cfg.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    peer_cfg.discovery.include_shell_panes = true;
    peer_cfg.zellij.enabled = false;

    let peer_core = Core::new(&DaemonConfig {
        state_dir: peer_dir.join("state"),
        socket: peer_dir.join("state/starfaned.sock"),
        config: peer_cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    peer_core.sync_discovered_panes().await;

    let peer_app = starfane_daemon::web::router(peer_core.clone());
    let peer_srv = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            peer_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let mut hub_cfg = StarfaneConfig {
        nodes: vec![NodeConfig {
            id: "mac-peer".into(),
            display_name: Some("Mac Peer".into()),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec![format!("http://{peer_addr}")],
            token: "fed-admin-tok".into(),
            ..NodeConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    hub_cfg.paths.tmux = "/usr/bin/false".into();
    hub_cfg.zellij.enabled = false;

    let hub_core = Core::new(&DaemonConfig {
        state_dir: hub_dir.join("state"),
        socket: hub_dir.join("state/starfaned.sock"),
        config: hub_cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    hub_core.sync_remote_nodes().await;

    let fed_sid = SessionId::new(format!("mac-peer:tmux-{}", peer_pane.as_str()));
    let mut hub_sse = hub_core.subscribe();
    let res = hub_core.close_session_window(&fed_sid).await.unwrap();
    assert_eq!(res["closed"], true);
    assert_eq!(res["session_id"], fed_sid.as_str());
    assert_eq!(res["pane"], format!("mac-peer:{}", peer_pane.as_str()));

    assert!(
        sb_tmux.list_panes().unwrap().is_empty(),
        "peer tmux pane must be closed via proxied POST /api/sessions/{{id}}/close"
    );
    let hub_rec = hub_core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id == fed_sid)
        .unwrap();
    assert_eq!(hub_rec.state, SessionState::Ended);

    let mut saw_hub_sse = false;
    while let Ok(ev) = hub_sse.try_recv() {
        if let DaemonEvent::Session { session, .. } = ev {
            if session.session_id == fed_sid && session.state == SessionState::Ended {
                saw_hub_sse = true;
            }
        }
    }
    assert!(saw_hub_sse, "hub must broadcast SSE session Ended event");

    peer_srv.abort();
}
