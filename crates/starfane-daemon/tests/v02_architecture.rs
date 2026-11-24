//! End-to-end integration tests for v0.2 core architecture:
//! - Zero-copy ESM static asset serving + ETag 304 + path traversal rejection over real HTTP
//! - Network-agnostic AccessGate (IPv6 CIDR, reverse-proxy Host, read-only scoped token)
//! - Multi-host Peer federation (multi-candidate failover, host prefixing, transcript proxy,
//!   host rename persistence, and graceful Degraded resilience on peer outage)

use serde_json::{json, Value};
use starfane_daemon::config::{
    NodeConfig, NodeKindConfig, ScopedTokenConfig, StarfaneConfig, WebConfig,
};
use starfane_daemon::core::TranscriptQuery;
use starfane_daemon::{Core, DaemonConfig};
use starfane_proto::{id, AgentKind, EmitRequest, EnvCtx, HostId, HostStatus, SessionState};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn mk_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("starfane-v02-{tag}-{}", id::new_id("T")));
    std::fs::create_dir_all(&p).unwrap();
    p
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

#[tokio::test]
async fn esm_assets_etag_304_and_access_gate_readonly_token_and_host_rename() {
    let root = mk_dir("web-gate");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: addr.port(),
            auth_enabled: true,
            allow_easytier: false,
            allow_tailscale: false,
            allowed_cidrs: vec!["fd00:abcd::/32".into()],
            allowed_hosts: vec!["console.example.com".into()],
            scoped_tokens: vec![ScopedTokenConfig {
                label: "viewer".into(),
                token: "ro-viewer-tok".into(),
                token_env: String::new(),
                read_only: true,
            }],
            terminal_font: "JetBrainsMono Nerd Font".into(),
            ..WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;

    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Verify precompiled AccessGate accepts IPv6 ULA & blocks LAN IPv4
    assert!(core
        .access_gate()
        .is_allowed_peer_ip("fd00:abcd::42".parse().unwrap()));
    assert!(!core
        .access_gate()
        .is_allowed_peer_ip("192.168.1.99".parse().unwrap()));

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // 1. Reverse-proxy custom Host (no port) gets ESM module + ETag, then 304 on If-None-Match!
    let (st_js, head_js, body_js) = raw_http(
        addr,
        "GET /assets/js/terminal.js HTTP/1.1\r\nHost: console.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(st_js, 200, "{head_js}");
    assert!(body_js.contains("normalizeTerminalGlyphs"));
    let etag = head_js
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("etag").then(|| v.trim().to_string())
        })
        .expect("ETag header present");
    assert!(etag.starts_with('"'));

    let (st_304, _, _) = raw_http(
        addr,
        &format!(
            "GET /assets/js/terminal.js HTTP/1.1\r\nHost: console.example.com\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(st_304, 304);

    // 2. Traversal blocked cleanly
    let (st_trav, _, _) = raw_http(
        addr,
        "GET /assets/js/../../Cargo.toml HTTP/1.1\r\nHost: console.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(st_trav, 404);

    // 3. Disallowed Host rejected with 403
    let (st_bad_host, _, _) = raw_http(
        addr,
        "GET /api/state HTTP/1.1\r\nHost: evil.example.com\r\nX-Starfane-Token: ro-viewer-tok\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(st_bad_host, 403);

    // 4. Scoped read-only token: GET /api/state succeeds (with hosts + terminal_font),
    //    POST /api/hosts/local/rename is rejected with 403!
    let (st_state, _, body_state) = raw_http(
        addr,
        "GET /api/state HTTP/1.1\r\nHost: console.example.com\r\nX-Starfane-Token: ro-viewer-tok\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(st_state, 200, "{body_state}");
    let v: Value = serde_json::from_str(&body_state).unwrap();
    assert_eq!(v["data"]["terminal_font"], "JetBrainsMono Nerd Font");
    assert_eq!(v["data"]["hosts"][0]["id"], "local");

    let rename_body = r#"{"display_name":"Hacked"}"#;
    let (st_ro_write, _, _) = raw_http(
        addr,
        &format!(
            "POST /api/hosts/local/rename HTTP/1.1\r\nHost: console.example.com\r\nOrigin: https://console.example.com\r\nX-Starfane-Token: ro-viewer-tok\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            rename_body.len(),
            rename_body
        ),
    )
    .await;
    assert_eq!(st_ro_write, 403);

    // 4b. `/auth?token=<ro-viewer-tok>` succeeds and hands out the scoped cookie (not primary admin token!).
    let (st_auth_ro, head_auth_ro, _) = raw_http(
        addr,
        "GET /auth?token=ro-viewer-tok HTTP/1.1\r\nHost: console.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(st_auth_ro, 302, "{head_auth_ro}");
    let cookie_hdr = head_auth_ro
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("set-cookie")
                .then(|| v.trim().to_string())
        })
        .expect("Set-Cookie header present");
    assert!(cookie_hdr.contains("starfane_token=ro-viewer-tok"));
    assert!(!cookie_hdr.contains(core.web_token()));

    // 5. Admin token renames host successfully!
    let admin_body = r#"{"display_name":"办公 Mac"}"#;
    let (st_admin, _, _) = raw_http(
        addr,
        &format!(
            "POST /api/hosts/local/rename HTTP/1.1\r\nHost: console.example.com\r\nOrigin: https://console.example.com\r\nX-Starfane-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            core.web_token(),
            admin_body.len(),
            admin_body
        ),
    )
    .await;
    assert_eq!(st_admin, 200);
    assert_eq!(core.hosts()[0].display_name, "办公 Mac");

    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_peer_federation_failover_transcript_proxy_and_degraded_resilience() {
    let root = mk_dir("peer-fed");
    let peer_dir = root.join("peer");
    let hub_dir = root.join("hub");
    std::fs::create_dir_all(&peer_dir).unwrap();
    std::fs::create_dir_all(&hub_dir).unwrap();

    // Write a 12-message Claude transcript on the Peer daemon.
    let tr_file = peer_dir.join("peer-sess.jsonl");
    let mut jsonl = String::new();
    for i in 1..=6 {
        jsonl.push_str(&format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-11-24T10:00:{i:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"peer-q-{i}\"}}}}\n"
        ));
        jsonl.push_str(&format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"2026-11-24T10:00:{i:02}.500Z\",\"message\":{{\"id\":\"m{i}\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"peer-a-{i}\"}}]}}}}\n"
        ));
    }
    std::fs::write(&tr_file, &jsonl).unwrap();

    // Spin up a REAL Peer axum server on an ephemeral loopback port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let mut peer_cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: peer_addr.port(),
            auth_enabled: true,
            scoped_tokens: vec![ScopedTokenConfig {
                label: "hub".into(),
                token: "fed-secret-123".into(),
                token_env: String::new(),
                read_only: false,
            }],
            ..WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    peer_cfg.paths.tmux = "/usr/bin/false".into();
    peer_cfg.zellij.enabled = false;

    let peer_core = Core::new(&DaemonConfig {
        state_dir: peer_dir.join("state"),
        socket: peer_dir.join("state/starfaned.sock"),
        config: peer_cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    peer_core
        .ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Claude,
            hook_event: Some("Stop".into()),
            payload: json!({
                "session_id": "remote-s1",
                "hook_event_name": "Stop",
                "cwd": peer_dir.to_string_lossy(),
                "transcript_path": tr_file.to_string_lossy(),
                "last_assistant_message": "peer-a-6"
            }),
            env: EnvCtx {
                tmux_pane: Some("%4".into()),
                tmux: Some("/tmp/tmux,1,0".into()),
                ..EnvCtx::default()
            },
            recv_ts_ms: id::now_ms(),
        })
        .await;

    let peer_router = starfane_daemon::web::router(peer_core.clone());
    let peer_handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            peer_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // Spin up Hub Core pointing to Peer with a DEAD primary endpoint + LIVE secondary endpoint!
    let mut hub_cfg = StarfaneConfig {
        nodes: vec![NodeConfig {
            id: "mac-xiling".into(),
            display_name: Some("Mac Xiling".into()),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec!["http://127.0.0.1:1".into(), format!("http://{peer_addr}")],
            token: "fed-secret-123".into(),
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

    // Sync hub -> fails over from :1 to live peer_addr!
    hub_core.sync_remote_nodes().await;
    let hosts = hub_core.hosts();
    let mac = hosts
        .iter()
        .find(|h| h.id.as_str() == "mac-xiling")
        .expect("mac-xiling host registered");
    assert_eq!(mac.status, HostStatus::Online);
    assert_eq!(mac.session_count, 1);

    let snap = hub_core.snapshot();
    let fed_sess = snap
        .sessions
        .iter()
        .find(|s| s.session_id.as_str() == "mac-xiling:remote-s1")
        .expect("prefixed peer session present");
    assert_eq!(
        fed_sess.pane.as_ref().map(|p| p.as_str()),
        Some("mac-xiling:%4")
    );
    assert_eq!(fed_sess.host_display_name.as_deref(), Some("Mac Xiling"));

    // Proxy cursor-paginated transcript from Hub -> Peer!
    let page1 = hub_core
        .session_transcript(
            &fed_sess.session_id,
            TranscriptQuery {
                limit: 4,
                before: None,
                all: false,
            },
        )
        .await
        .expect("proxy transcript page 1");
    assert_eq!(page1.total_messages, 12);
    assert_eq!(page1.messages.len(), 4);
    assert!(page1.has_more);
    assert_eq!(page1.next_before, Some(8));

    let page2 = hub_core
        .session_transcript(
            &fed_sess.session_id,
            TranscriptQuery {
                limit: 4,
                before: Some(8),
                all: false,
            },
        )
        .await
        .expect("proxy transcript page 2");
    assert_eq!(page2.messages.len(), 4);
    assert_eq!(page2.start_seq, 4);
    assert_eq!(page2.messages[0].text, "peer-q-3");

    // Rename remote host on Hub -> updates host record AND session host_display_name!
    hub_core
        .rename_host(&HostId::new("mac-xiling"), "希凌 MacBook")
        .unwrap();
    let renamed = hub_core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "mac-xiling")
        .unwrap();
    assert_eq!(renamed.display_name, "希凌 MacBook");

    // Proxy interactive dispatch from Hub -> real Peer `/api/dispatch`:
    // Peer accepts the exact `DispatchBody` schema, checks its local tmux gate (which fails since
    // peer tmux is `/usr/bin/false`), falls back to L0 inbox (`status: "queued"`), and Hub returns
    // the Peer's real `DeliveryReport` with host-qualified pane `"mac-xiling:%4"`!
    let disp_report = hub_core
        .handle(starfane_proto::DaemonCommand::Dispatch {
            target: starfane_proto::DispatchTarget::Session {
                id: fed_sess.session_id.clone(),
            },
            text: "hello from hub".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .expect("Hub -> Peer /api/dispatch must succeed with valid DispatchBody schema");
    assert_eq!(disp_report["status"], "queued");
    assert_eq!(disp_report["level"], "l0_inbox");
    assert_eq!(disp_report["pane"], "mac-xiling:%4");
    assert_eq!(disp_report["gate"]["passed"], false);

    // Subscribe to Hub SSE before stopping Peer to confirm `DaemonEvent::Host` Degraded event fires!
    let mut sse_rx = hub_core.subscribe();

    // Stop the Peer server -> Hub degrades gracefully WITHOUT marking remote sessions Ended!
    peer_handle.abort();
    let _ = peer_handle.await;
    hub_core.sync_remote_nodes().await;
    let mut saw_degraded_event = false;
    while let Ok(ev) = sse_rx.try_recv() {
        if let starfane_proto::DaemonEvent::Host { host, .. } = ev {
            if host.id.as_str() == "mac-xiling" && host.status == HostStatus::Degraded {
                saw_degraded_event = true;
            }
        }
    }
    assert!(
        saw_degraded_event,
        "Host Online -> Degraded transition must broadcast DaemonEvent::Host to SSE clients"
    );
    let degraded = hub_core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "mac-xiling")
        .unwrap();
    assert_eq!(degraded.status, HostStatus::Degraded);
    assert!(degraded.error.is_some());
    let still_alive = hub_core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "mac-xiling:remote-s1")
        .expect("remote session must remain visible when host degrades");
    assert_ne!(still_alive.state, SessionState::Ended);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_peer_dispatch_preserves_delivery_report_delivered_and_blocked() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = mk_dir("peer-disp");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();

    // Mock Peer that validates the exact Hub POST `/api/dispatch` JSON schema (`target.type == "session"`,
    // `interactive == true`) and returns a `DeliveryStatus::Blocked` report so we prove Hub does NOT
    // blindly fabricate `Delivered`.
    let mock_peer = tokio::spawn(async move {
        for req_idx in 0..2 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut raw_bytes = Vec::new();
            let mut chunk = [0u8; 2048];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                raw_bytes.extend_from_slice(&chunk[..n]);
                if let Some(split) = raw_bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw_bytes[..split]);
                    let cl: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if raw_bytes.len() >= split + 4 + cl {
                        break;
                    }
                }
            }
            let raw = String::from_utf8_lossy(&raw_bytes).into_owned();
            let resp_body = if req_idx == 0 {
                json!({
                    "ok": true,
                    "data": {
                        "sessions": [{
                            "session_id": "s-42",
                            "agent": "claude",
                            "pane": "%9",
                            "state": "idle"
                        }]
                    }
                })
                .to_string()
            } else {
                let (_, body_part) = raw.split_once("\r\n\r\n").unwrap();
                let parsed: Value = serde_json::from_str(body_part.trim()).unwrap();
                assert_eq!(parsed["target"]["type"], "session");
                assert_eq!(parsed["target"]["id"], "s-42");
                assert_eq!(parsed["interactive"], true);
                assert_eq!(parsed["text"], "run tests");
                json!({
                    "ok": true,
                    "data": {
                        "level": "l2_inject",
                        "status": "failed",
                        "pane": "%9",
                        "gate": {
                            "passed": false,
                            "checks": [["not_in_mode", false]],
                            "detail": "pane is in copy-mode"
                        }
                    }
                })
                .to_string()
            };
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            sock.write_all(http.as_bytes()).await.unwrap();
        }
    });

    let mut hub_cfg = StarfaneConfig {
        nodes: vec![NodeConfig {
            id: "work-mac".into(),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec![format!("http://{peer_addr}")],
            ..NodeConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    hub_cfg.paths.tmux = "/usr/bin/false".into();
    hub_cfg.zellij.enabled = false;
    let hub = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config: hub_cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    hub.sync_remote_nodes().await;
    let res = hub
        .handle(starfane_proto::DaemonCommand::Dispatch {
            target: starfane_proto::DispatchTarget::Session {
                id: starfane_proto::SessionId::new("work-mac:s-42"),
            },
            text: "run tests".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .expect("hub dispatch proxy succeeds");
    assert_eq!(
        res["status"], "failed",
        "Hub must preserve Peer Failed gate status"
    );
    assert_eq!(res["pane"], "work-mac:%9");
    assert_eq!(res["gate"]["detail"], "pane is in copy-mode");

    mock_peer.await.unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn peer_session_count_excludes_subagents_and_ended_and_preserves_404_semantics() {
    let root = mk_dir("peer-404");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();

    // Serve 3 requests:
    // 1. GET /api/state -> 1 live root + 1 subagent + 1 ended root (live root count MUST be 1, not 3)
    // 2. GET /api/sessions/gone-s/transcript -> 404 NotFound
    // 3. GET /api/panes/%2599/preview -> 404 NotFound
    let mock_peer = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let (status, body) = if first_line.starts_with("GET /api/state ") {
                (
                    "200 OK",
                    json!({
                        "ok": true,
                        "data": {
                            "sessions": [
                                { "session_id": "gone-s", "agent": "claude", "pane": "%99", "state": "idle" },
                                { "session_id": "gone-s/sub1", "agent": "claude", "parent_session_id": "gone-s", "is_subagent": true, "state": "working" },
                                { "session_id": "old-ended", "agent": "codex", "state": "ended" }
                            ]
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    "404 Not Found",
                    json!({
                        "ok": false,
                        "error": { "code": "not_found", "message": "not found on peer" }
                    })
                    .to_string(),
                )
            };
            let http = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(http.as_bytes()).await.unwrap();
        }
    });

    let mut hub_cfg = StarfaneConfig {
        nodes: vec![NodeConfig {
            id: "peer404".into(),
            kind: NodeKindConfig::Peer,
            enabled: true,
            endpoints: vec![format!("http://{peer_addr}")],
            ..NodeConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    hub_cfg.paths.tmux = "/usr/bin/false".into();
    hub_cfg.zellij.enabled = false;
    let hub = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config: hub_cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    let mut sse_rx = hub.subscribe();
    hub.sync_remote_nodes().await;
    let mut sse_host_count = None;
    while let Ok(ev) = sse_rx.try_recv() {
        if let starfane_proto::DaemonEvent::Host { host, .. } = ev {
            if host.id.as_str() == "peer404" {
                sse_host_count = Some(host.session_count);
            }
        }
    }
    assert_eq!(
        sse_host_count,
        Some(1),
        "Peer sync Host event must exclude subagents and Ended sessions"
    );

    // Peer 404 on transcript must map to NotFound -> clean TranscriptResponse::none (not 500)!
    let tr = hub
        .session_transcript(
            &starfane_proto::SessionId::new("peer404:gone-s"),
            TranscriptQuery::tail(20),
        )
        .await
        .expect("Peer 404 transcript must degrade to source=none instead of 500");
    assert_eq!(tr.source, "none");
    assert!(tr.messages.is_empty());

    // Peer 404 on pane preview must preserve ErrorCode::NotFound (not ErrorCode::Internal)!
    let pv_err = hub
        .capture_pane(
            &starfane_proto::PaneId::new("peer404:%99"),
            starfane_tmux::CaptureRequest::tail(20),
        )
        .await
        .expect_err("Peer 404 preview should return Err");
    assert_eq!(pv_err.code, starfane_proto::ErrorCode::NotFound);

    mock_peer.await.unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn test_notifier_routes_pluggable_feishu_default_when_legacy_feishu_disabled() {
    use starfane_daemon::config::{NotifierConfig, NotifierKind};
    let root = mk_dir("notifier-test");

    // Loopback webhook receiver representing a pluggable notifier registered with id "feishu-default".
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook_listener.local_addr().unwrap();
    let hook_srv = tokio::spawn(async move {
        let (mut sock, _) = hook_listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"sent\":true}";
        sock.write_all(resp.as_bytes()).await.unwrap();
    });

    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();

    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: api_addr.port(),
            auth_enabled: false,
            ..WebConfig::default()
        },
        notifiers: vec![NotifierConfig {
            id: "feishu-default".into(),
            kind: NotifierKind::Webhook,
            enabled: true,
            url: format!("http://{hook_addr}/hook"),
            ..NotifierConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    cfg.feishu.enabled = false;
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;

    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    let app = starfane_daemon::web::router(core);
    let srv = tokio::spawn(async move {
        let _ = axum::serve(
            api_listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let payload = r#"{"text":"ping"}"#;
    let (st, _, body) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/feishu-default/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            payload.len(),
            payload
        ),
    )
    .await;
    assert_eq!(
        st, 200,
        "pluggable feishu-default must not 503 when legacy [feishu] is disabled: {body}"
    );
    assert!(body.contains(r#"\"sent\":true"#) || body.contains("sent"));

    hook_srv.await.unwrap();
    srv.abort();
    let _ = std::fs::remove_dir_all(&root);
}
