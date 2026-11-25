//! Integration tests for v0.1.1:
//! - Legacy tmux pane discovery & real-hook deduplication/upgrade
//! - Relaxed interactive dispatch (`interactive: true`)
//! - Pane preview (`GET /api/panes/{pane_id}/preview`)
//! - Relaxed `POST /api/pipelines` and `POST /api/debug` (empty `project_root`,
//!   omitted model bindings, `auto_confirm` true/false)

use serde_json::{json, Value};
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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

fn make_env(tmux_bin: &str, tmux_socket: Option<String>) -> TestEnv {
    let root = std::env::temp_dir().join(format!("starfane-v011-{}", id::new_id("T")));
    let repo = root.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = std::fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut config = StarfaneConfig::default();
    if let Some(sock) = &tmux_socket {
        let wrapper = root.join("tmux-wrap.sh");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec {tmux_bin} -L {sock} \"$@\"\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        config.paths.tmux = wrapper.to_string_lossy().into_owned();
    } else {
        config.paths.tmux = tmux_bin.into();
    }
    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    TestEnv {
        root,
        repo,
        core,
        tmux_socket,
    }
}

async fn spawn_http(core: Core) -> (String, String, tokio::task::JoinHandle<()>) {
    let token = core.web_token().to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = starfane_daemon::web::router(core);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), token, handle)
}

async fn http_req(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let payload = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:27418\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(tok) = token {
        req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(payload);
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (head, body_str) = text.split_once("\r\n\r\n").expect("http response");
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let json = serde_json::from_str(body_str).unwrap_or(json!({"raw": body_str}));
    (status, json)
}

fn emit(agent: AgentKind, pane: &str, payload: Value) -> EmitRequest {
    EmitRequest {
        v: 1,
        agent,
        hook_event: None,
        payload,
        env: EnvCtx {
            tmux_pane: Some(pane.into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    }
}

#[tokio::test]
async fn real_hook_upgrades_legacy_tmux_session_without_duplicates() {
    let env = make_env("/usr/bin/false", None);

    // Seed a synthetic discovered-only session + inbox item on %12, as discovery would create.
    let legacy_sid = SessionId::new("tmux-%12");
    let pane = PaneId::new("%12");
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%12",
            json!({"session_id": "tmux-%12", "hook_event_name": "SessionStart", "cwd": env.repo}),
        ))
        .await;
    // Mark it discovered_only with tmux_target via another real hook test:
    // Better: test actual real-hook arrival via Core::ingest when a real session arrives on %12.
    let sessions_before: Vec<SessionRecord> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ListSessions {
                project: None,
                include_subagents: false,
                include_ended: false,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sessions_before.len(), 1);
    assert_eq!(sessions_before[0].session_id, legacy_sid);

    // Queue an inbox message onto tmux-%12.
    let _ = env
        .core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: legacy_sid.clone(),
            },
            text: "carry over inbox".into(),
            attachments: Vec::new(),
            allow: vec![DeliveryLevel::L0Inbox],
            interactive: false,
        })
        .await
        .unwrap();

    // Now a real hook arrives on the same pane %12 with real session_id `claude-real-12`.
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%12",
            json!({
                "session_id": "claude-real-12",
                "hook_event_name": "Stop",
                "cwd": env.repo,
                "last_assistant_message": "done"
            }),
        ))
        .await;

    let sessions_after: Vec<SessionRecord> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ListSessions {
                project: None,
                include_subagents: false,
                include_ended: false,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        sessions_after.len(),
        1,
        "legacy tmux-%12 must be merged into real session: {sessions_after:?}"
    );
    assert_eq!(sessions_after[0].session_id.as_str(), "claude-real-12");
    assert_eq!(sessions_after[0].pane.as_ref(), Some(&pane));
    assert!(!sessions_after[0].discovered_only);
    assert_eq!(sessions_after[0].unread_inbox, 1);

    let inbox: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-real-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].body_md, "carry over inbox");
}

#[tokio::test]
async fn web_relaxed_pipelines_and_debug_auto_fallback_and_auto_confirm() {
    let env = make_env("/usr/bin/false", None);

    // Register idle Claude & Codex sessions in `env.repo` so default_project_root picks `env.repo`
    // and auto_confirm can adopt them for Stage 1 (Plan / Triage) without spawning tmux.
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%1",
            json!({"session_id": "c-idle", "hook_event_name": "SessionStart", "cwd": env.repo}),
        ))
        .await;
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%1",
            json!({"session_id": "c-idle", "hook_event_name": "Stop", "cwd": env.repo, "last_assistant_message": "ready"}),
        ))
        .await;
    env.core
        .ingest(emit(
            AgentKind::Codex,
            "%2",
            json!({"session_id": "x-idle", "hook_event_name": "SessionStart", "cwd": env.repo, "transcript_path": null}),
        ))
        .await;
    env.core
        .ingest(emit(
            AgentKind::Codex,
            "%2",
            json!({"session_id": "x-idle", "hook_event_name": "Stop", "cwd": env.repo, "turn_id": "t1", "last_assistant_message": "ready", "stop_hook_active": false, "permission_mode": "default", "transcript_path": null}),
        ))
        .await;

    let (addr, token, server) = spawn_http(env.core.clone()).await;

    // 1. Auth required on /api/panes/%1/preview
    let (st, _) = http_req(&addr, "GET", "/api/panes/%251/preview?lines=80", None, None).await;
    assert_eq!(st, 401);

    // 2. Relaxed POST /api/pipelines: empty project_root, no utterance, auto_confirm: false
    let (st, body) = http_req(
        &addr,
        "POST",
        "/api/pipelines",
        Some(&token),
        Some(r#"{"goal":"增加会话预览","project_root":"","auto_confirm":false}"#),
    )
    .await;
    assert_eq!(st, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["status"], "awaiting_binding_confirm");
    assert_eq!(body["data"]["project"]["root"], env.repo);

    // 3. Relaxed POST /api/pipelines: wrapped `{req, auto_confirm: true}` (default) -> immediately running!
    let (st, body) = http_req(
        &addr,
        "POST",
        "/api/pipelines",
        Some(&token),
        Some(r#"{"req":{"playbook":"feature","goal":"一键启动测试","project_root":""}}"#),
    )
    .await;
    assert_eq!(st, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["status"], "running");
    assert_eq!(body["data"]["stages"][0]["status"], "running");
    assert_eq!(body["data"]["stages"][0]["session_id"], "c-idle");

    // 4. Relaxed POST /api/debug: omitted project_root & utterance, default auto_confirm: true
    let (st, body) = http_req(
        &addr,
        "POST",
        "/api/debug",
        Some(&token),
        Some(r#"{"symptom":"页面偶发 502"}"#),
    )
    .await;
    assert_eq!(st, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["playbook"], "debug");
    assert_eq!(body["data"]["status"], "running");
    assert_eq!(body["data"]["project"]["root"], env.repo);

    server.abort();
}

#[tokio::test]
async fn isolated_tmux_preview_and_relaxed_interactive_dispatch() {
    // Only run tmux portion if `tmux` binary is installed in PATH.
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let sock = format!("starfane-test-v011-{}", std::process::id());
    let env = make_env("tmux", Some(sock.clone()));

    // Create a clean detached tmux session running `cat` (accepts pasted lines cleanly).
    let out = Command::new("tmux")
        .args([
            "-L",
            &sock,
            "new-session",
            "-d",
            "-s",
            "demo",
            "-c",
            &env.repo,
            "-P",
            "-F",
            "#{pane_id}",
            "sh -c 'echo READY_LINE; cat'",
        ])
        .output()
        .expect("tmux new-session");
    assert!(
        out.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(pane_id.starts_with('%'), "unexpected pane_id: {pane_id}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Register a session in Working state (which strict gate would reject!) bound to `pane_id`.
    env.core
        .ingest(emit(
            AgentKind::Claude,
            &pane_id,
            json!({
                "session_id": "busy-sess",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "cwd": env.repo
            }),
        ))
        .await;

    let (addr, token, server) = spawn_http(env.core.clone()).await;

    // 1. GET /api/panes/{pane_id}/preview returns live terminal output.
    let enc_pane = pane_id.replace('%', "%25");
    let (st, prev) = http_req(
        &addr,
        "GET",
        &format!("/api/panes/{enc_pane}/preview?lines=40"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 200, "preview: {prev}");
    assert_eq!(prev["ok"], true);
    assert_eq!(prev["data"]["pane_id"], pane_id);
    assert_eq!(prev["data"]["dead"], false);
    assert_eq!(prev["data"]["in_mode"], false);
    assert!(
        prev["data"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("READY_LINE"),
        "expected READY_LINE in preview: {prev}"
    );

    // 2. Non-existent pane returns 404.
    let (st404, _) = http_req(
        &addr,
        "GET",
        "/api/panes/%25999/preview?lines=20",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st404, 404);

    // 3. POST /api/dispatch with interactive: true succeeds even though session is Working
    //    and skips Hook ACK waiting (returns immediately as Delivered).
    let payload = json!({
        "target": { "type": "session", "id": "busy-sess" },
        "text": "PING_INTERACTIVE_OK",
        "interactive": true
    })
    .to_string();
    let (st, disp) = http_req(&addr, "POST", "/api/dispatch", Some(&token), Some(&payload)).await;
    assert_eq!(st, 200, "dispatch: {disp}");
    assert_eq!(disp["ok"], true);
    assert_eq!(disp["data"]["status"], "delivered");
    assert_eq!(disp["data"]["level"], "l2_inject");
    assert_eq!(disp["data"]["pane"], pane_id);

    // Verify the pasted text actually reached the tmux pane.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (st2, prev2) = http_req(
        &addr,
        "GET",
        &format!("/api/panes/{enc_pane}/preview?lines=40"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st2, 200);
    assert!(
        prev2["data"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("PING_INTERACTIVE_OK"),
        "expected injected text in pane: {prev2}"
    );

    server.abort();
}

#[tokio::test]
async fn web_session_transcript_reads_persisted_jsonl_and_falls_back_cleanly() {
    let env = make_env("/usr/bin/false", None);
    let tr_file = env.root.join("codex-rollout.jsonl");
    std::fs::write(
        &tr_file,
        concat!(
            r#"{"type":"response_item","timestamp":"2026-11-21T04:10:03.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"查一下日志报错"}]}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"2026-11-21T04:10:04.000Z","payload":{"type":"custom_tool_call","name":"exec","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-11-21T04:10:05.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"output_tokens":400},"last_token_usage":{"input_tokens":50000,"output_tokens":200,"total_tokens":50200},"model_context_window":200000}}}"#,
            "\n",
            r####"{"type":"response_item","timestamp":"2026-11-21T04:10:05.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"### 根因\n\n已定位到 `timeout` 配置过小。"}]}}"####,
            "\n"
        ),
    )
    .unwrap();
    let tr_path = tr_file.to_string_lossy().into_owned();

    // 1. Codex session reporting transcript_path via hook
    env.core
        .ingest(emit(
            AgentKind::Codex,
            "%7",
            json!({
                "session_id": "cx-tr-1",
                "hook_event_name": "Stop",
                "cwd": env.repo,
                "turn_id": "t1",
                "last_assistant_message": "done",
                "stop_hook_active": false,
                "permission_mode": "default",
                "transcript_path": tr_path
            }),
        ))
        .await;

    // 2. Claude session without any transcript_path on disk
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%8",
            json!({
                "session_id": "cl-no-tr",
                "hook_event_name": "Stop",
                "cwd": env.repo,
                "last_assistant_message": "hi"
            }),
        ))
        .await;

    let (addr, token, server) = spawn_http(env.core.clone()).await;

    // Auth required
    let (st, _) = http_req(
        &addr,
        "GET",
        "/api/sessions/cx-tr-1/transcript?limit=12",
        None,
        None,
    )
    .await;
    assert_eq!(st, 401);

    // Unknown session -> 404
    let (st404, _) = http_req(
        &addr,
        "GET",
        "/api/sessions/nonexistent/transcript",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st404, 404);

    // Real transcript_path persisted & parsed cleanly
    let (st_ok, body) = http_req(
        &addr,
        "GET",
        "/api/sessions/cx-tr-1/transcript?limit=12",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st_ok, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["source"], "transcript");
    assert_eq!(body["data"]["transcript_path"], tr_path);
    let msgs = body["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "msgs: {msgs:?}");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["text"], "查一下日志报错");
    assert_eq!(msgs[1]["role"], "assistant");
    assert!(msgs[1]["text"].as_str().unwrap().contains("### 根因"));
    assert_eq!(msgs[1]["tools"], json!(["exec"]));
    assert_eq!(body["data"]["metrics"]["source"], "transcript");
    assert_eq!(body["data"]["metrics"]["context_used_tokens"], 50_000);
    assert_eq!(body["data"]["metrics"]["context_window_tokens"], 200_000);
    assert_eq!(body["data"]["metrics"]["context_used_pct"], 25.0);
    assert_eq!(body["data"]["metrics"]["tps"], 100.0);

    // Session without transcript -> source: "none", messages: []
    let (st_none, none_body) = http_req(
        &addr,
        "GET",
        "/api/sessions/cl-no-tr/transcript?limit=12",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st_none, 200, "none_body: {none_body}");
    assert_eq!(none_body["data"]["source"], "none");
    assert_eq!(none_body["data"]["messages"], json!([]));

    // GET /api/models returns claude/codex/agy defaults + efforts + aggregated base models
    let (st_mod, mod_body) = http_req(&addr, "GET", "/api/models", Some(&token), None).await;
    assert_eq!(st_mod, 200, "models: {mod_body}");
    assert!(mod_body["data"]["claude"]["default"].as_str().is_some());
    assert!(mod_body["data"]["claude"]["default_effort"]
        .as_str()
        .is_some());
    assert!(!mod_body["data"]["claude"]["efforts"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!mod_body["data"]["claude"]["models"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(mod_body["data"]["codex"]["default"].as_str().is_some());
    assert!(mod_body["data"]["codex"]["default_effort"]
        .as_str()
        .is_some());
    let agy_models = mod_body["data"]["agy"]["models"].as_array().unwrap();
    assert!(!agy_models.is_empty());
    // Verify AGY Orchestrator fixed-effort entry has empty efforts array.
    let orch = agy_models
        .iter()
        .find(|m| {
            m["label"]
                .as_str()
                .is_some_and(|l| l.contains("Orchestrator"))
        })
        .expect("expected Orchestrator entry in AGY catalog");
    assert_eq!(orch["efforts"], json!([]));

    server.abort();
}

#[tokio::test]
async fn auth_free_tailscale_mode_exposes_health_state_models_and_subagent_transcript() {
    let root = std::env::temp_dir().join(format!("starfane-ts-{}", id::new_id("T")));
    let repo = root.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo_str = std::fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut config = StarfaneConfig::default();
    config.paths.tmux = "/usr/bin/false".into();
    config.web.bind = "0.0.0.0".into();
    config.web.port = 27418;
    config.web.auth_enabled = false;
    config.web.allow_tailscale = true;
    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Seed a subagent with its own JSONL transcript and verify /transcript returns its messages.
    let sub_tr = root.join("sub-agent.jsonl");
    std::fs::write(
        &sub_tr,
        concat!(
            r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-21T07:00:00Z","content":"<USER_REQUEST>\n检查子会话输出\n</USER_REQUEST>"}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-21T07:00:02Z","content":"子会话已完成检查。","tool_calls":[{"name":"read_file"}]}"#,
            "\n"
        ),
    )
    .unwrap();
    core.ingest(emit(
        AgentKind::Agy,
        "%41",
        json!({
            "conversationId": "sub-worker-1",
            "hook_event_name": "AfterAgent",
            "cwd": repo_str,
            "transcriptPath": sub_tr.to_string_lossy()
        }),
    ))
    .await;

    // Also verify Feishu quote-reply session mapping persistence helper.
    core.record_feishu_session_message(
        "om_test_card_99",
        starfane_daemon::notice::SessionReplyTarget {
            session_id: SessionId::new("sub-worker-1"),
            pane_id: Some(PaneId::new("%41")),
            tmux_label: Some("learning-workflow".into()),
            agent: AgentKind::Agy,
        },
    );
    let mapped = core
        .session_for_feishu_message("om_test_card_99")
        .expect("mapped session");
    assert_eq!(mapped.session_id.as_str(), "sub-worker-1");

    let (addr, _tok, server) = spawn_http(core).await;

    // 1. GET /api/health reports auth_enabled = false
    let (st_h, health) = http_req(&addr, "GET", "/api/health", None, None).await;
    assert_eq!(st_h, 200);
    assert_eq!(health["data"]["auth_enabled"], false);

    // 2. GET /api/state & /api/models succeed WITHOUT any token, including with Tailscale Host header!
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            b"GET /api/state HTTP/1.1\r\nHost: 100.64.10.20:27418\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let resp_str = String::from_utf8_lossy(&raw);
    assert!(resp_str.starts_with("HTTP/1.1 200"), "{resp_str}");

    // 3. Non-Tailscale LAN Host header is still rejected (403)!
    let mut stream_bad = TcpStream::connect(&addr).await.unwrap();
    stream_bad
        .write_all(
            b"GET /api/state HTTP/1.1\r\nHost: 192.168.1.5:27418\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw_bad = Vec::new();
    stream_bad.read_to_end(&mut raw_bad).await.unwrap();
    let bad_str = String::from_utf8_lossy(&raw_bad);
    assert!(bad_str.starts_with("HTTP/1.1 403"), "{bad_str}");

    // 4. Subagent transcript readable without token in auth_enabled = false mode
    let (st_sub, sub_body) = http_req(
        &addr,
        "GET",
        "/api/sessions/sub-worker-1/transcript?limit=24",
        None,
        None,
    )
    .await;
    assert_eq!(st_sub, 200, "{sub_body}");
    assert_eq!(sub_body["data"]["source"], "transcript");
    assert_eq!(
        sub_body["data"]["messages"][1]["text"],
        "子会话已完成检查。"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}

async fn raw_http_exchange(addr: &str, raw_request: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw_request).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    String::from_utf8_lossy(&raw).into_owned()
}

#[tokio::test]
async fn auth_free_easytier_only_mode_allows_easytier_and_blocks_tailscale_and_lan() {
    let root = std::env::temp_dir().join(format!("starfane-et-{}", id::new_id("T")));
    std::fs::create_dir_all(&root).unwrap();

    let mut config = StarfaneConfig {
        web: starfane_daemon::config::WebConfig {
            bind: "0.0.0.0".into(),
            port: 27418,
            auth_enabled: false,
            allow_easytier: true,
            easytier_cidrs: vec!["10.144.144.0/24".into()],
            allow_tailscale: false,
            ..starfane_daemon::config::WebConfig::default()
        },
        ..StarfaneConfig::default()
    };
    config.paths.tmux = "/usr/bin/false".into();

    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    let (addr, _tok, server) = spawn_http(core).await;

    // 1. EasyTier IP Host header succeeds without token.
    let resp_et = raw_http_exchange(
        &addr,
        b"GET /api/state HTTP/1.1\r\nHost: 10.144.144.3:27418\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp_et.starts_with("HTTP/1.1 200"), "{resp_et}");

    // 2. EasyTier same-origin POST succeeds.
    let post_body = br#"{"minutes":0}"#;
    let post_et = format!(
        "POST /api/feishu/mute HTTP/1.1\r\nHost: 10.144.144.3:27418\r\nOrigin: http://10.144.144.3:27418\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        post_body.len(),
        std::str::from_utf8(post_body).unwrap()
    );
    let resp_post_et = raw_http_exchange(&addr, post_et.as_bytes()).await;
    assert!(resp_post_et.starts_with("HTTP/1.1 200"), "{resp_post_et}");

    // 3. Tailscale IP, MagicDNS *.ts.net Host, Tailscale Origin, and LAN Host are rejected (403).
    let resp_ts_ip = raw_http_exchange(
        &addr,
        b"GET /api/state HTTP/1.1\r\nHost: 100.64.10.20:27418\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp_ts_ip.starts_with("HTTP/1.1 403"), "{resp_ts_ip}");

    let resp_ts_dns = raw_http_exchange(
        &addr,
        b"GET /api/state HTTP/1.1\r\nHost: devbox.ts.net:27418\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp_ts_dns.starts_with("HTTP/1.1 403"), "{resp_ts_dns}");

    let post_ts_origin = format!(
        "POST /api/feishu/mute HTTP/1.1\r\nHost: 10.144.144.3:27418\r\nOrigin: http://100.64.10.20:27418\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        post_body.len(),
        std::str::from_utf8(post_body).unwrap()
    );
    let resp_post_ts_origin = raw_http_exchange(&addr, post_ts_origin.as_bytes()).await;
    assert!(
        resp_post_ts_origin.starts_with("HTTP/1.1 403"),
        "{resp_post_ts_origin}"
    );

    let resp_lan = raw_http_exchange(
        &addr,
        b"GET /api/state HTTP/1.1\r\nHost: 10.17.0.50:27418\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp_lan.starts_with("HTTP/1.1 403"), "{resp_lan}");

    server.abort();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn v017_custom_mute_slash_commands_paste_dispatch_and_thread_migration() {
    use std::os::unix::fs::PermissionsExt;

    let script_dir = std::env::temp_dir().join(format!("starfane-v017-tmux-{}", id::new_id("T")));
    std::fs::create_dir_all(&script_dir).unwrap();
    let injected_log = script_dir.join("injected.txt");
    let fake_tmux = script_dir.join("fake-tmux.sh");
    // 18 fields separated by ASCII 0x1f matching starfane_tmux::model::PANE_FORMAT.
    let pane_row = [
        "%12", "main", "@1", "0", "0", "12345", "claude", "/tmp", "1", "1", "1", "0", "0", "0",
        "0", "0", "0", "dev",
    ]
    .join("\x1f");
    let pane_file = script_dir.join("pane_row.txt");
    std::fs::write(&pane_file, format!("{pane_row}\n")).unwrap();
    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
cmd="$1"
if [ "$cmd" = "list-panes" ]; then
  cat "{pane}"
  exit 0
fi
if [ "$cmd" = "load-buffer" ]; then
  cat >> "{log}"
  printf '\n---END---\n' >> "{log}"
  exit 0
fi
exit 0
"#,
            pane = pane_file.display(),
            log = injected_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None);

    // Seed a Feishu session thread on legacy `tmux-%12` and verify it migrates to `claude-real-12`.
    let legacy_sid = SessionId::new("tmux-%12");
    env.core
        .record_feishu_session_thread(&legacy_sid, "om_legacy_root_12");
    env.core.record_feishu_session_message(
        "om_legacy_root_12",
        starfane_daemon::notice::SessionReplyTarget {
            session_id: legacy_sid.clone(),
            pane_id: Some(PaneId::new("%12")),
            tmux_label: Some("main-dev".into()),
            agent: AgentKind::Claude,
        },
    );
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%12",
            json!({
                "session_id": "claude-real-12",
                "hook_event_name": "Stop",
                "cwd": env.repo,
                "last_assistant_message": "migrated"
            }),
        ))
        .await;
    assert_eq!(env.core.feishu_session_thread(&legacy_sid), None);
    assert_eq!(
        env.core
            .feishu_session_thread(&SessionId::new("claude-real-12"))
            .as_deref(),
        Some("om_legacy_root_12")
    );
    assert_eq!(
        env.core
            .session_for_feishu_message("om_legacy_root_12")
            .map(|t| t.session_id),
        Some(SessionId::new("claude-real-12"))
    );

    let state_dir = env.core.state_dir().to_path_buf();
    let (addr, tok, server) = spawn_http(env.core.clone()).await;

    // 1. Custom Feishu mute: minutes, until_ms, bounds errors, and unmute.
    let b1 = json!({"minutes": 45}).to_string();
    let (st, body) = http_req(&addr, "POST", "/api/feishu/mute", Some(&tok), Some(&b1)).await;
    assert_eq!(st, 200, "{body}");
    assert!(body["data"]["feishu_muted_until_ms"].as_u64().is_some());

    let target_until = starfane_proto::id::now_ms() + 3_600_000;
    let b2 = json!({"until_ms": target_until}).to_string();
    let (st, body) = http_req(&addr, "POST", "/api/feishu/mute", Some(&tok), Some(&b2)).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["data"]["feishu_muted_until_ms"].as_u64(),
        Some(target_until)
    );

    // Past until_ms rejected (400).
    let b_past = json!({"until_ms": 1_000}).to_string();
    let (st_past, _) = http_req(&addr, "POST", "/api/feishu/mute", Some(&tok), Some(&b_past)).await;
    assert_eq!(st_past, 400);

    // >7d minutes rejected (400).
    let b_over = json!({"minutes": 20_000}).to_string();
    let (st_over, _) = http_req(&addr, "POST", "/api/feishu/mute", Some(&tok), Some(&b_over)).await;
    assert_eq!(st_over, 400);

    // Unmute with minutes = 0.
    let b_zero = json!({"minutes": 0}).to_string();
    let (st_unmute, body_unmute) =
        http_req(&addr, "POST", "/api/feishu/mute", Some(&tok), Some(&b_zero)).await;
    assert_eq!(st_unmute, 200, "{body_unmute}");
    assert!(body_unmute["data"]["feishu_muted_until_ms"].is_null());

    // 2. GET /api/slash-commands for each agent + omitted + invalid.
    for (agent, expected_cmd) in [
        ("agy", "/swarm"),
        ("claude", "/context"),
        ("codex", "/permissions"),
    ] {
        let (st, body) = http_req(
            &addr,
            "GET",
            &format!("/api/slash-commands?agent={agent}"),
            Some(&tok),
            None,
        )
        .await;
        assert_eq!(st, 200, "{body}");
        assert_eq!(body["data"]["agent"], agent);
        let cmds = body["data"]["commands"].as_array().unwrap();
        assert!(
            cmds.iter()
                .any(|c| c["cmd"] == expected_cmd && c["group"] == "native"),
            "missing {expected_cmd} for {agent}: {body}"
        );
        assert!(
            cmds.iter()
                .any(|c| c["cmd"] == "/feature" && c["group"] == "starfane"),
            "missing starfane shortcut for {agent}"
        );
    }
    let (st_all, body_all) = http_req(&addr, "GET", "/api/slash-commands", Some(&tok), None).await;
    assert_eq!(st_all, 200, "{body_all}");
    let all_cmds = body_all["data"]["commands"].as_array().unwrap();
    assert!(all_cmds.iter().any(|c| c["cmd"] == "/swarm"));
    assert!(all_cmds.iter().any(|c| c["cmd"] == "/context"));
    assert!(all_cmds.iter().any(|c| c["cmd"] == "/permissions"));

    let (st_bad_agent, _) = http_req(
        &addr,
        "GET",
        "/api/slash-commands?agent=unknown",
        Some(&tok),
        None,
    )
    .await;
    assert_eq!(st_bad_agent, 400);

    // 3. POST /api/dispatch with paste attachments writes <state_dir>/inputs/paste-*.md
    //    and injects a clean single-line pointer!
    let b_att = json!({
        "target": {"type": "pane", "id": "%12"},
        "text": "请排查这个报错",
        "attachments": [
            {"title": "panic.log", "content": "thread main panicked\nat src/main.rs:42\nstack backtrace"}
        ],
        "interactive": true
    })
    .to_string();
    let (st_att, body_att) =
        http_req(&addr, "POST", "/api/dispatch", Some(&tok), Some(&b_att)).await;
    assert_eq!(st_att, 200, "{body_att}");
    assert_eq!(body_att["data"]["status"], "delivered");

    let inputs_dir = state_dir.join("inputs");
    let mut paste_files: Vec<_> = std::fs::read_dir(&inputs_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert_eq!(
        paste_files.len(),
        1,
        "expected 1 paste file in {inputs_dir:?}"
    );
    let saved_md = std::fs::read_to_string(&paste_files[0]).unwrap();
    assert!(saved_md.contains("## panic.log"));
    assert!(saved_md.contains("thread main panicked"));

    // Verify actual tmux load-buffer received the clean single-line pointer, and
    // interactive direct delivery did NOT leave a stale unread item in inbox (v0.1.11).
    let injected_str = std::fs::read_to_string(&injected_log).unwrap();
    let first_chunk = injected_str
        .split("\n---END---\n")
        .next()
        .unwrap_or("")
        .trim();
    assert!(!first_chunk.contains('\n'), "injected:\n{injected_str}");
    assert!(
        first_chunk.starts_with("请排查这个报错（附粘贴内容已保存至 "),
        "injected:\n{injected_str}"
    );
    assert!(
        first_chunk.contains(&paste_files[0].display().to_string()),
        "injected:\n{injected_str}"
    );
    let inbox: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-real-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        inbox.is_empty(),
        "direct L2 interactive dispatch must not leave unread inbox items: {inbox:?}"
    );

    // 4. POST /api/dispatch with >2400 chars long text (no attachments) also writes to inputs/.
    let long_text = "x".repeat(2500);
    let b_long = json!({
        "target": {"type": "pane", "id": "%12"},
        "text": long_text,
        "interactive": true
    })
    .to_string();
    let (st_long, body_long) =
        http_req(&addr, "POST", "/api/dispatch", Some(&tok), Some(&b_long)).await;
    assert_eq!(st_long, 200, "{body_long}");
    paste_files = std::fs::read_dir(&inputs_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert_eq!(paste_files.len(), 2, "expected 2 paste files");

    server.abort();
    let _ = std::fs::remove_dir_all(&script_dir);
}

#[tokio::test]
async fn v019_memories_filter_effort_and_image_upload_dispatch() {
    use std::os::unix::fs::PermissionsExt;

    let script_dir = std::env::temp_dir().join(format!("starfane-v019-tmux-{}", id::new_id("T")));
    std::fs::create_dir_all(&script_dir).unwrap();
    let injected_log = script_dir.join("injected.txt");
    let fake_tmux = script_dir.join("fake-tmux.sh");
    let pane_row = [
        "%16", "prome", "@2", "0", "0", "22345", "codex", "/tmp", "1", "1", "1", "0", "0", "0",
        "0", "0", "0", "ros",
    ]
    .join("\x1f");
    let pane_file = script_dir.join("pane_row.txt");
    std::fs::write(&pane_file, format!("{pane_row}\n")).unwrap();
    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
cmd="$1"
if [ "$cmd" = "list-panes" ]; then
  cat "{pane}"
  exit 0
fi
if [ "$cmd" = "load-buffer" ]; then
  cat >> "{log}"
  printf '\n---END---\n' >> "{log}"
  exit 0
fi
exit 0
"#,
            pane = pane_file.display(),
            log = injected_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None);

    // 1. Codex background memory task (`~/.codex/memories`) MUST be ignored completely.
    env.core
        .ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Codex,
            hook_event: Some("Stop".into()),
            payload: json!({
                "session_id": "codex-mem-noise-1",
                "hook_event_name": "Stop",
                "cwd": "/Users/demo/.codex/memories",
                "last-assistant-message": "memory summary"
            }),
            env: EnvCtx::default(),
            recv_ts_ms: id::now_ms(),
        })
        .await;
    let snap = env.core.snapshot();
    assert!(
        !snap
            .sessions
            .iter()
            .any(|s| s.session_id.as_str() == "codex-mem-noise-1"),
        "codex memories background task must be filtered out"
    );

    // 2. Real AGY session populates `effort` from model suffix (`gemini-4-neon-high` -> `high`).
    env.core
        .ingest(emit(
            AgentKind::Agy,
            "%16",
            json!({
                "conversationId": "agy-effort-16",
                "hook_event_name": "AfterAgent",
                "modelName": "gemini-4-neon-high",
                "cwd": env.repo
            }),
        ))
        .await;
    let agy_rec = env
        .core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "agy-effort-16")
        .expect("agy session");
    assert_eq!(agy_rec.effort.as_deref(), Some("high"));

    let (addr, tok, server) = spawn_http(env.core.clone()).await;

    // 3. POST /api/uploads/image with valid 1x1 PNG + GET /api/uploads/{id} preview.
    let sample_png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let up_body = json!({
        "filename": "screenshot.png",
        "mime": "image/png",
        "data_base64": sample_png_b64
    })
    .to_string();
    let (st_up, res_up) = http_req(
        &addr,
        "POST",
        "/api/uploads/image",
        Some(&tok),
        Some(&up_body),
    )
    .await;
    assert_eq!(st_up, 200, "{res_up}");
    let img_id = res_up["data"]["id"].as_str().unwrap().to_string();
    let img_path = res_up["data"]["path"].as_str().unwrap().to_string();
    let preview_url = res_up["data"]["preview_url"].as_str().unwrap().to_string();
    assert_eq!(preview_url, format!("/api/uploads/{img_id}"));
    assert_eq!(res_up["data"]["mime"], "image/png");

    // Fake non-image bytes rejected with 400.
    let bad_up = json!({
        "filename": "evil.png",
        "mime": "image/png",
        "data_base64": "aGVsbG8gd29ybGQ="
    })
    .to_string();
    let (st_bad, _) = http_req(
        &addr,
        "POST",
        "/api/uploads/image",
        Some(&tok),
        Some(&bad_up),
    )
    .await;
    assert_eq!(st_bad, 400);

    // GET preview returns HTTP 200 + `Content-Type: image/png` + exact PNG magic bytes.
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    let get_req = format!(
        "GET {preview_url} HTTP/1.1\r\nHost: 127.0.0.1:27418\r\nAuthorization: Bearer {tok}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(get_req.as_bytes()).await.unwrap();
    let mut raw_resp = Vec::new();
    stream.read_to_end(&mut raw_resp).await.unwrap();
    let header_end = raw_resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http headers");
    let head = String::from_utf8_lossy(&raw_resp[..header_end]);
    let img_bytes = &raw_resp[header_end + 4..];
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: image/png"),
        "{head}"
    );
    assert!(img_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

    // 4. POST /api/dispatch with uploaded image attachment injects clean single-line reference.
    let disp_img = json!({
        "target": {"type": "pane", "id": "%16"},
        "text": "请帮我看这张报错截图",
        "attachments": [
            {"kind": "image", "title": "screenshot.png", "path": img_path}
        ],
        "interactive": true
    })
    .to_string();
    let (st_d, body_d) =
        http_req(&addr, "POST", "/api/dispatch", Some(&tok), Some(&disp_img)).await;
    assert_eq!(st_d, 200, "{body_d}");
    assert_eq!(body_d["data"]["status"], "delivered");
    let injected = std::fs::read_to_string(&injected_log).unwrap();
    assert!(
        injected.contains("请帮我看这张报错截图（附图片："),
        "{injected}"
    );
    assert!(injected.contains(&img_id), "{injected}");

    // 4b. Inline anchored image token preserves cursor order (`先看：[图片 #1] 这里错了`).
    let disp_anchored = json!({
        "target": {"type": "pane", "id": "%16"},
        "text": "先看：[图片 #1] 这里错了",
        "attachments": [
            {"kind": "image", "title": "screenshot.png", "path": img_path, "token": "[图片 #1]"}
        ],
        "interactive": true
    })
    .to_string();
    let (st_da, body_da) = http_req(
        &addr,
        "POST",
        "/api/dispatch",
        Some(&tok),
        Some(&disp_anchored),
    )
    .await;
    assert_eq!(st_da, 200, "{body_da}");
    let injected_after = std::fs::read_to_string(&injected_log).unwrap();
    let second_chunk = injected_after.split("\n---END---\n").nth(1).unwrap_or("");
    let canon_img_path = std::fs::canonicalize(&img_path)
        .unwrap()
        .display()
        .to_string();
    assert!(
        second_chunk.contains(&format!("先看：[图片: {canon_img_path}] 这里错了")),
        "{second_chunk}"
    );

    // Dispatching an outside path like `/etc/hosts` MUST be rejected (400).
    let disp_bad = json!({
        "target": {"type": "pane", "id": "%16"},
        "text": "look",
        "attachments": [{"kind": "image", "path": "/etc/hosts"}],
        "interactive": true
    })
    .to_string();
    let (st_bad_d, _) = http_req(&addr, "POST", "/api/dispatch", Some(&tok), Some(&disp_bad)).await;
    assert_eq!(st_bad_d, 400);

    server.abort();
    let _ = std::fs::remove_dir_all(&script_dir);
}

#[tokio::test]
async fn v019_codex_hook_without_tmux_pane_merges_legacy_by_transcript_uuid() {
    let env = make_env("/usr/bin/false", None);
    let uuid = "01a18191-3525-7790-b3d3-36033faa913c";
    let rollout = format!("/tmp/rollout-2026-11-23T10-00-00-{uuid}.jsonl");

    // 1. Seed a synthetic discovered tmux-%16 session that already resolved its Codex rollout path.
    let legacy_sid = SessionId::new("tmux-%16");
    env.core
        .record_feishu_session_thread(&legacy_sid, "om_root_16");
    env.core
        .ingest(emit(
            AgentKind::Codex,
            "%16",
            json!({
                "session_id": "tmux-%16",
                "hook_event_name": "SessionStart",
                "cwd": env.repo,
                "transcript_path": rollout
            }),
        ))
        .await;
    // Mark `tmux-%16` as discovered_only with tmux metadata by restarting via snapshot with exact state.
    let state_dir = env.core.state_dir().to_path_buf();
    let legacy_snap = json!({
        "sessions": {
            "tmux-%16": {
                "session_id": "tmux-%16",
                "agent": "codex",
                "pane": "%16",
                "pane_resolution": "env",
                "cwd": env.repo,
                "git_root": env.repo,
                "model": "gpt-6.2-sol",
                "effort": "high",
                "state": "idle",
                "state_since_ms": 1000,
                "last_event_ms": 1000,
                "confidence": "heuristic",
                "tmux_target": "prome:0.0",
                "tmux_label": "prome-prometheus-ros",
                "discovered_only": true,
                "transcript_path": rollout
            },
            "stale-mem": {
                "session_id": "stale-mem",
                "agent": "codex",
                "pane_resolution": "unknown",
                "cwd": "/Users/demo/.codex/memories",
                "state": "idle",
                "state_since_ms": 1000,
                "last_event_ms": 1000,
                "confidence": "exact"
            }
        },
        "feishu_session_threads": {
            "tmux-%16": "om_root_16"
        },
        "feishu_session_thread_order": ["tmux-%16"]
    });
    std::fs::write(
        state_dir.join("state.json"),
        serde_json::to_string(&legacy_snap).unwrap(),
    )
    .unwrap();

    // Reload Core from snapshot: verifies legacy `/.codex/memories` dirty session is purged on startup!
    let core2 = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned2.sock"),
        config: StarfaneConfig::default(),
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    assert!(
        !core2
            .snapshot()
            .sessions
            .iter()
            .any(|s| s.session_id.as_str() == "stale-mem"),
        "startup snapshot load must purge /.codex/memories dirty records"
    );

    // 2. Now a Codex Stop hook arrives WITHOUT `$TMUX_PANE` (simulating Codex shared app-server),
    //    carrying `<oai-mem-citation>` in `last-assistant-message`.
    core2
        .ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Codex,
            hook_event: Some("Stop".into()),
            payload: json!({
                "session_id": uuid,
                "hook_event_name": "Stop",
                "cwd": env.repo,
                "transcript_path": rollout,
                "last_assistant_message": "已完成检查。\n\n<oai-mem-citation>\n<citation_entries>x</citation_entries>\n</oai-mem-citation>"
            }),
            env: EnvCtx::default(), // NO tmux_pane!
            recv_ts_ms: id::now_ms(),
        })
        .await;

    // Only ONE session should remain, having inherited %16 / prome-prometheus-ros / thread root,
    // and with `<oai-mem-citation>` stripped from `last_message`!
    let sessions = core2.snapshot().sessions;
    assert_eq!(
        sessions.len(),
        1,
        "expected merged single session: {sessions:?}"
    );
    let rec = &sessions[0];
    assert_eq!(rec.session_id.as_str(), uuid);
    assert!(!rec.discovered_only);
    assert_eq!(rec.pane.as_ref().map(|p| p.as_str()), Some("%16"));
    assert_eq!(rec.tmux_target.as_deref(), Some("prome:0.0"));
    assert_eq!(rec.tmux_label.as_deref(), Some("prome-prometheus-ros"));
    assert_eq!(rec.last_message.as_deref(), Some("已完成检查。"));
    assert_eq!(
        core2
            .feishu_session_thread(&SessionId::new(uuid))
            .as_deref(),
        Some("om_root_16")
    );
}

#[tokio::test]
async fn v0111_new_tmux_window_and_rename_emit_live_sse_session_and_appear_in_state() {
    use std::os::unix::fs::PermissionsExt;

    let script_dir = std::env::temp_dir().join(format!("starfane-v0111-win-{}", id::new_id("T")));
    std::fs::create_dir_all(&script_dir).unwrap();
    let pane_file = script_dir.join("panes.txt");
    let fake_tmux = script_dir.join("fake-tmux.sh");

    let mk_row = |id: &str, sess: &str, win_idx: &str, win_name: &str, cmd: &str| {
        [
            id, sess, "@1", win_idx, "0", "99999", cmd, "/tmp", "1", "1", "1", "0", "0", "0", "0",
            "0", "0", win_name,
        ]
        .join("\x1f")
    };
    std::fs::write(
        &pane_file,
        format!("{}\n", mk_row("%10", "prome", "0", "ros", "codex")),
    )
    .unwrap();
    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
if [ "$1" = "list-panes" ]; then
  cat "{pane}"
  exit 0
fi
if [ "$1" = "capture-pane" ]; then
  echo "ready"
  exit 0
fi
exit 0
"#,
            pane = pane_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None);
    env.core.sync_discovered_panes().await;
    assert_eq!(env.core.snapshot().sessions.len(), 1);

    let (addr, tok, server) = spawn_http(env.core.clone()).await;
    let mut sub = env.core.subscribe();

    // Dynamically create a brand-new window `%88` in tmux session `prome` (`prome:8.0`).
    std::fs::write(
        &pane_file,
        format!(
            "{}\n{}\n",
            mk_row("%10", "prome", "0", "ros", "codex"),
            mk_row("%88", "prome", "8", "new-win", "agy")
        ),
    )
    .unwrap();
    env.core.sync_discovered_panes().await;

    // 1. Live SSE broadcast MUST immediately deliver DaemonEvent::Session for the new window!
    let mut saw_new_win = false;
    while let Ok(ev) = sub.try_recv() {
        if let DaemonEvent::Session { session, .. } = ev {
            if session.session_id.as_str() == "tmux-%88"
                && session.tmux_target.as_deref() == Some("prome:8.0")
                && session.tmux_label.as_deref() == Some("prome-new-win")
            {
                saw_new_win = true;
            }
        }
    }
    assert!(
        saw_new_win,
        "new tmux window must emit live SSE Session event"
    );

    // 2. GET /api/state immediately includes the new window.
    let (st, state_body) = http_req(&addr, "GET", "/api/state", Some(&tok), None).await;
    assert_eq!(st, 200);
    let sessions = state_body["data"]["sessions"].as_array().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s["session_id"] == "tmux-%88" && s["tmux_label"] == "prome-new-win"),
        "GET /api/state missing new window: {state_body}"
    );

    // 3. Renaming the window emits an updated SSE Session event with the new tmux_label.
    std::fs::write(
        &pane_file,
        format!(
            "{}\n{}\n",
            mk_row("%10", "prome", "0", "ros", "codex"),
            mk_row("%88", "prome", "8", "renamed-win", "agy")
        ),
    )
    .unwrap();
    env.core.sync_discovered_panes().await;
    let mut saw_rename = false;
    while let Ok(ev) = sub.try_recv() {
        if let DaemonEvent::Session { session, .. } = ev {
            if session.session_id.as_str() == "tmux-%88"
                && session.tmux_label.as_deref() == Some("prome-renamed-win")
            {
                saw_rename = true;
            }
        }
    }
    assert!(
        saw_rename,
        "renamed tmux window must emit live SSE Session event"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&script_dir);
}

#[tokio::test]
async fn v0111_interactive_dispatch_no_stale_inbox_and_read_inbox_consumes_messages() {
    use std::os::unix::fs::PermissionsExt;

    let script_dir = std::env::temp_dir().join(format!("starfane-v0111-inb-{}", id::new_id("T")));
    std::fs::create_dir_all(&script_dir).unwrap();
    let pane_file = script_dir.join("pane.txt");
    let fake_tmux = script_dir.join("fake-tmux.sh");
    let pane_row = [
        "%12", "main", "@1", "0", "0", "12345", "claude", "/tmp", "1", "1", "1", "0", "0", "0",
        "0", "0", "0", "dev",
    ]
    .join("\x1f");
    std::fs::write(&pane_file, format!("{pane_row}\n")).unwrap();
    std::fs::write(
        &fake_tmux,
        format!(
            r#"#!/bin/bash
set -e
if [ "$1" = "list-panes" ]; then
  cat "{pane}"
  exit 0
fi
exit 0
"#,
            pane = pane_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = make_env(&fake_tmux.to_string_lossy(), None);
    env.core
        .ingest(emit(
            AgentKind::Claude,
            "%12",
            json!({
                "session_id": "claude-inbox-12",
                "hook_event_name": "Stop",
                "cwd": env.repo
            }),
        ))
        .await;

    // 1. Direct interactive L2 dispatch succeeds and leaves ZERO unread inbox items.
    let rep: DeliveryReport = serde_json::from_value(
        env.core
            .handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new("claude-inbox-12"),
                },
                text: "直接发到终端的消息".into(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rep.status, DeliveryStatus::Delivered);
    let rec = env
        .core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "claude-inbox-12")
        .unwrap();
    assert_eq!(rec.unread_inbox, 0);
    let items: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-inbox-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(items.is_empty());

    // 2. Queued L0 message is returned once by ReadInbox(mark_delivered: true), then permanently consumed.
    let _ = env
        .core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new("claude-inbox-12"),
            },
            text: "排队消息一条".into(),
            attachments: Vec::new(),
            allow: vec![DeliveryLevel::L0Inbox],
            interactive: false,
        })
        .await
        .unwrap();
    let first: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-inbox-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].body_md, "排队消息一条");
    let second: Vec<InboxItem> = serde_json::from_value(
        env.core
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-inbox-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        second.is_empty(),
        "second read_inbox must return empty: {second:?}"
    );

    // 3. Startup snapshot cleanup purges pre-v0.1.11 ghost already-poked Message entries.
    let state_dir = env.core.state_dir().to_path_buf();
    let ghost_snap = json!({
        "sessions": {
            "claude-inbox-12": {
                "session_id": "claude-inbox-12",
                "agent": "claude",
                "pane": "%12",
                "pane_resolution": "env",
                "cwd": env.repo,
                "state": "idle",
                "state_since_ms": 1000,
                "last_event_ms": 1000,
                "unread_inbox": 2,
                "confidence": "exact"
            }
        },
        "inbox": {
            "claude-inbox-12": [
                {
                    "item": {
                        "id": "M-ghost-1",
                        "kind": "message",
                        "created_ms": 1000,
                        "title": "old",
                        "body_md": "already injected days ago"
                    },
                    "delivered": false,
                    "poke_pending": false
                },
                {
                    "item": {
                        "id": "M-ghost-2",
                        "kind": "message",
                        "created_ms": 1001,
                        "title": "old2",
                        "body_md": "already read"
                    },
                    "delivered": true,
                    "poke_pending": false
                }
            ]
        }
    });
    std::fs::write(
        state_dir.join("state.json"),
        serde_json::to_string(&ghost_snap).unwrap(),
    )
    .unwrap();
    let reloaded = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned-reloaded.sock"),
        config: StarfaneConfig::default(),
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    let s_after = reloaded
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == "claude-inbox-12")
        .unwrap();
    assert_eq!(s_after.unread_inbox, 0);
    let after_items: Vec<InboxItem> = serde_json::from_value(
        reloaded
            .handle(DaemonCommand::ReadInbox {
                session_id: SessionId::new("claude-inbox-12"),
                mark_delivered: true,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(after_items.is_empty());

    let _ = std::fs::remove_dir_all(&script_dir);
}

#[tokio::test]
async fn v0112_agy_hook_upgrades_working_legacy_pane_without_misfolding_and_separates_same_cwd() {
    let env = make_env("/usr/bin/false", None);
    let tr17 = env.root.join("agy-17.jsonl");
    let tr44 = env.root.join("agy-44.jsonl");
    std::fs::write(
        &tr17,
        concat!(
            r#"{"step_index":1,"type":"USER_INPUT","created_at":"2026-11-23T11:00:00Z","content":"pane 17 task"}"#,
            "\n",
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T11:00:02Z","model":"gemini-4-neon-high","content":"pane 17 reply","usage":{"input_tokens":80000,"output_tokens":300}}"#,
            "\n"
        ),
    )
    .unwrap();
    std::fs::write(
        &tr44,
        concat!(
            r#"{"step_index":1,"type":"USER_INPUT","created_at":"2026-11-23T11:01:00Z","content":"pane 44 task"}"#,
            "\n",
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","created_at":"2026-11-23T11:01:03Z","model":"gemini-4-neon-medium","content":"pane 44 reply","usage":{"input_tokens":120000,"output_tokens":600}}"#,
            "\n"
        ),
    )
    .unwrap();

    // Seed synthetic `discovered_only: true`, `state: Working` legacy sessions for `%17` and `%44`
    // sharing the EXACT same `cwd`, reproducing the pre-v0.1.12 misfolding trigger.
    let state_dir = env.core.state_dir().to_path_buf();
    let snap = json!({
        "sessions": {
            "tmux-%17": {
                "session_id": "tmux-%17",
                "agent": "agy",
                "pane": "%17",
                "pane_resolution": "env",
                "cwd": env.repo,
                "state": "working",
                "state_since_ms": 1000,
                "last_event_ms": 1000,
                "tmux_target": "prome:0.0",
                "tmux_label": "prome-prometheus-ros",
                "discovered_only": true,
                "confidence": "heuristic"
            },
            "tmux-%44": {
                "session_id": "tmux-%44",
                "agent": "agy",
                "pane": "%44",
                "pane_resolution": "env",
                "cwd": env.repo,
                "state": "working",
                "state_since_ms": 1000,
                "last_event_ms": 1000,
                "tmux_target": "prome-agy:0.0",
                "tmux_label": "prome-agy-prometheus-ros",
                "discovered_only": true,
                "confidence": "heuristic"
            }
        }
    });
    std::fs::write(
        state_dir.join("state.json"),
        serde_json::to_string(&snap).unwrap(),
    )
    .unwrap();

    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned-v0112.sock"),
        config: StarfaneConfig::default(),
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Real AGY hooks arrive on `%17` and `%44` while both `tmux-%17` and `tmux-%44` are Working.
    core.ingest(emit(
        AgentKind::Agy,
        "%17",
        json!({
            "conversationId": "66fac8b3-93ac-4c7d-814c-b79bb0ec9c28",
            "hook_event_name": "PreToolUse",
            "toolCall": { "name": "read_file" },
            "cwd": env.repo,
            "modelName": "gemini-4-neon-high",
            "transcriptPath": tr17.to_string_lossy()
        }),
    ))
    .await;
    core.ingest(emit(
        AgentKind::Agy,
        "%44",
        json!({
            "conversationId": "f03f879a-451a-495e-ad84-90e4b6569501",
            "hook_event_name": "PreToolUse",
            "toolCall": { "name": "run_command" },
            "cwd": env.repo,
            "modelName": "gemini-4-neon-medium",
            "transcriptPath": tr44.to_string_lossy()
        }),
    ))
    .await;

    let all: Vec<SessionRecord> = serde_json::from_value(
        core.handle(DaemonCommand::ListSessions {
            project: None,
            include_subagents: true,
            include_ended: false,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        all.len(),
        2,
        "legacy tmux-%17 and tmux-%44 must both upgrade in-place with zero misfolding: {all:?}"
    );
    let s17 = all
        .iter()
        .find(|s| s.session_id.as_str() == "66fac8b3-93ac-4c7d-814c-b79bb0ec9c28")
        .expect("pane %17 root");
    let s44 = all
        .iter()
        .find(|s| s.session_id.as_str() == "f03f879a-451a-495e-ad84-90e4b6569501")
        .expect("pane %44 root");

    assert!(!s17.is_subagent);
    assert!(!s17.discovered_only);
    assert_eq!(s17.pane.as_ref().map(|p| p.as_str()), Some("%17"));
    assert_eq!(s17.tmux_label.as_deref(), Some("prome-prometheus-ros"));
    assert_eq!(s17.model.as_deref(), Some("gemini-4-neon-high"));
    assert_eq!(s17.effort.as_deref(), Some("high"));

    assert!(!s44.is_subagent);
    assert!(!s44.discovered_only);
    assert_eq!(s44.pane.as_ref().map(|p| p.as_str()), Some("%44"));
    assert_eq!(s44.tmux_label.as_deref(), Some("prome-agy-prometheus-ros"));
    assert_eq!(s44.model.as_deref(), Some("gemini-4-neon-medium"));
    assert_eq!(s44.effort.as_deref(), Some("medium"));

    // A genuine Swarm subagent firing on busy real root `%17` STILL folds properly under `s17`.
    core.ingest(emit(
        AgentKind::Agy,
        "%17",
        json!({
            "conversationId": "sub-worker-17-a",
            "hook_event_name": "PreToolUse",
            "toolCall": { "name": "grep_search" },
            "cwd": env.repo
        }),
    ))
    .await;
    let all_with_sub: Vec<SessionRecord> = serde_json::from_value(
        core.handle(DaemonCommand::ListSessions {
            project: None,
            include_subagents: true,
            include_ended: false,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let sub = all_with_sub
        .iter()
        .find(|s| s.session_id.as_str() == "sub-worker-17-a")
        .expect("genuine subagent");
    assert!(sub.is_subagent);
    assert_eq!(
        sub.parent_session_id.as_ref().map(|id| id.as_str()),
        Some("66fac8b3-93ac-4c7d-814c-b79bb0ec9c28")
    );
}
