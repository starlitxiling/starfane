//! End-to-end integration tests for Interactive Approval / Menu Key Controls
//! (`POST /api/sessions/{id}/key`, `Core::send_session_key`, `Tmux::send_key`,
//! `SshMuxNodeProvider::send_key`, and `PeerDaemonNodeProvider::send_key`):
//!
//! 1. Strict allowlist validation (`Enter`, `Escape`, `Up`, `Down`, `Left`, `Right`, `Tab`,
//!    `Space`, `y`, `n`, `1`, `2`, `3`, `4`, `C-c` accepted; shell injection payloads like
//!    `"Enter; rm -rf /"` or `"foo"` rejected with `400 BadRequest`).
//! 2. Admin-write auth enforcement: `401 Unauthorized` without token, `403 Forbidden` for
//!    `read_only` scoped tokens (never delivering any keystroke to the pane).
//! 3. Session validation: `404 NotFound` for unknown session, `400 BadRequest` for subagent
//!    or pane-less sessions, `503 Unavailable` when the underlying tmux pane is gone.
//! 4. Real local sandbox `tmux` menu script receiving `Down` + `Enter` (including automatic
//!    `copy-mode` cancellation before key delivery), plus `1` and `Escape`.
//! 5. Real `fake-ssh` persistent conduit `SshMuxNodeProvider::send_key` over
//!    `ConduitLane::Interactive` (single RTT, copy-mode cancel, persistent conduit reuse,
//!    and rapid post-key dirty sync clearing `NeedsPermission` -> `Idle`).
//! 6. Real `PeerDaemonNodeProvider::send_key` proxying `POST /api/sessions/{enc(remote_sid)}/key`
//!    from Hub -> Peer via `PeerKeepAlivePool`.

use serde_json::{json, Value};
use starfane_daemon::config::{
    NodeConfig, NodeKindConfig, ScopedTokenConfig, StarfaneConfig, WebConfig,
};
use starfane_daemon::{Core, DaemonConfig};
use starfane_proto::{
    attention, id, AgentKind, EmitRequest, EnvCtx, ErrorCode, SessionId, SessionState,
};
use starfane_tmux::{SpawnWindow, Tmux};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

fn has_tmux_and_python3() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
        && Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

fn real_bin(name: &str) -> String {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .unwrap_or_else(|e| panic!("locate {name}: {e}"));
    assert!(out.status.success(), "{name} must be installed on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn mk_dir(tag: &str) -> PathBuf {
    let raw = std::env::temp_dir().join(format!("starfane-key-{tag}-{}", id::new_id("T")));
    std::fs::create_dir_all(&raw).unwrap();
    std::fs::canonicalize(&raw).unwrap_or(raw)
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

#[cfg(unix)]
fn write_tmux_wrapper(dir: &Path, sock: &str) -> PathBuf {
    let path = dir.join("tmux-wrap.sh");
    write_executable(&path, &format!("#!/bin/sh\nexec tmux -L {sock} \"$@\"\n"));
    path
}

fn write_menu_script(dir: &Path) -> PathBuf {
    let script = dir.join("menu_tui.py");
    std::fs::write(
        &script,
        r#"import os, select, sys, termios, tty

log_path = sys.argv[1]
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
idx = 0
options = ["Continue without using this MCP server", "Use this MCP server"]

def render():
    sys.stdout.write("\x1b[2J\x1b[H")
    sys.stdout.write("New MCP server found in this project: ida-pro\r\n")
    for i, opt in enumerate(options):
        prefix = "❯ " if i == idx else "  "
        sys.stdout.write(f"{prefix}{opt}\r\n")
    sys.stdout.write("Enter to confirm · Esc to cancel\r\n")
    sys.stdout.flush()

def log(ev):
    with open(log_path, "a") as f:
        f.write(ev + "\n")
        f.flush()

try:
    log("READY")
    render()
    while True:
        b = os.read(fd, 1)
        if not b:
            break
        if b == b"\x1b":
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                b2 = os.read(fd, 1)
                if b2 in (b"[", b"O"):
                    b3 = os.read(fd, 1)
                    if b3 == b"B":
                        idx = min(len(options) - 1, idx + 1)
                        log("KEY:Down")
                        render()
                        continue
                    elif b3 == b"A":
                        idx = max(0, idx - 1)
                        log("KEY:Up")
                        render()
                        continue
            log("KEY:Escape")
        elif b in (b"\r", b"\n"):
            log(f"KEY:Enter:SELECTED:{idx}:{options[idx]}")
            sys.stdout.write("\x1b[2J\x1b[HApproved.\r\n❯ \r\n? for shortcuts\r\n")
            sys.stdout.flush()
        elif b == b"\x03":
            log("KEY:C-c")
        elif b == b"\t":
            log("KEY:Tab")
        elif b == b" ":
            log("KEY:Space")
        else:
            log("KEY:" + b.decode("utf-8", "replace"))
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"#,
    )
    .unwrap();
    script
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

async fn post_json(
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> (u16, Value) {
    let body_str = serde_json::to_string(body).unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        addr.port(),
        addr.port(),
        body_str.len()
    );
    if let Some(tok) = token {
        req.push_str(&format!("X-Starfane-Token: {tok}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    req.push_str(&body_str);

    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    let full = String::from_utf8_lossy(&buf).into_owned();
    let (head, rest) = full.split_once("\r\n\r\n").unwrap_or((&full, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let val = serde_json::from_str(rest).unwrap_or_else(|_| json!({ "raw": rest }));
    (status, val)
}

async fn wait_for_log_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.contains(needle) {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?} in {}, got:\n{content}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn validation_auth_and_session_guards_reject_invalid_requests() {
    if !has_tmux_and_python3() {
        return;
    }

    let root = mk_dir("guards");
    let sock = format!("sf-key-guards-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let menu_py = write_menu_script(&root);
    let log_path = root.join("menu.log");
    let pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("guards".into()),
            create_session: true,
            window_name: "w1".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec![
                "python3".into(),
                "-u".into(),
                menu_py.to_string_lossy().into_owned(),
                log_path.to_string_lossy().into_owned(),
            ],
        })
        .unwrap();
    wait_for_log_contains(&log_path, "READY", Duration::from_secs(3)).await;

    let tmux_wrap = write_tmux_wrapper(&root, &sock);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    // Seed: (1) normal pane-backed session, (2) pane-less session, (3) subagent session.
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("PermissionRequest".into()),
        payload: json!({
            "session_id": "s-live",
            "hook_event_name": "PermissionRequest",
            "cwd": root.to_string_lossy(),
            "tool_name": "mcp",
        }),
        env: EnvCtx {
            tmux_pane: Some(pane.0.clone()),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("Stop".into()),
        payload: json!({
            "session_id": "s-no-pane",
            "hook_event_name": "Stop",
            "cwd": root.to_string_lossy(),
        }),
        env: EnvCtx::default(),
        recv_ts_ms: id::now_ms(),
    })
    .await;
    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("SubagentStart".into()),
        payload: json!({
            "session_id": "s-live",
            "agent_id": "sub-worker-1",
            "hook_event_name": "SubagentStart",
            "cwd": root.to_string_lossy(),
        }),
        env: EnvCtx {
            tmux_pane: Some(pane.0.clone()),
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
        .expect("subagent exists")
        .session_id;

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let tok = core.web_token();

    // 1. Missing token -> 401 Unauthorized.
    let (st_unauth, _) = post_json(
        addr,
        "/api/sessions/s-live/key",
        None,
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_unauth, 401);

    // 2. Read-only scoped token -> 403 Forbidden (must never deliver key).
    let (st_ro, body_ro) = post_json(
        addr,
        "/api/sessions/s-live/key",
        Some("ro-viewer-token"),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_ro, 403, "{body_ro}");
    assert_eq!(body_ro["ok"], false);
    assert_eq!(body_ro["error"]["code"], "forbidden");

    // 3. Shell injection & non-allowlisted keys -> 400 BadRequest.
    for bad_key in [
        "Enter; rm -rf /",
        "foo",
        "",
        "enter",
        "C-m",
        "y\n",
        "$(id)",
        "5",
    ] {
        let (st_bad, body_bad) = post_json(
            addr,
            "/api/sessions/s-live/key",
            Some(tok),
            &json!({"key": bad_key}),
        )
        .await;
        assert_eq!(
            st_bad, 400,
            "expected 400 for bad key {bad_key:?}, got {st_bad}: {body_bad}"
        );
        assert_eq!(body_bad["ok"], false);
        assert_eq!(body_bad["error"]["code"], "bad_request");
    }

    // 4. Unknown session -> 404 NotFound.
    let (st_404, body_404) = post_json(
        addr,
        "/api/sessions/does-not-exist/key",
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_404, 404, "{body_404}");
    assert_eq!(body_404["error"]["code"], "not_found");

    // 5. Subagent -> 400 BadRequest.
    let enc_sub = pct_encode(sub_sid.as_str());
    let (st_sub, body_sub) = post_json(
        addr,
        &format!("/api/sessions/{enc_sub}/key"),
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_sub, 400, "{body_sub}");
    assert_eq!(body_sub["error"]["code"], "bad_request");

    // 6. Pane-less session -> 400 BadRequest.
    let (st_np, body_np) = post_json(
        addr,
        "/api/sessions/s-no-pane/key",
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_np, 400, "{body_np}");
    assert_eq!(body_np["error"]["code"], "bad_request");

    // Ensure NONE of the rejected calls delivered any keystroke to the pane.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let log_after_rejects = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(
        log_after_rejects.trim(),
        "READY",
        "rejected requests must never deliver keystrokes: {log_after_rejects}"
    );

    // 7. Kill the pane behind Core's back -> 503 Unavailable.
    sb_tmux.kill_pane(&pane).unwrap();
    let (st_503, body_503) = post_json(
        addr,
        "/api/sessions/s-live/key",
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_503, 503, "{body_503}");
    assert_eq!(body_503["error"]["code"], "unavailable");

    server.abort();
}

#[tokio::test]
async fn local_sandbox_tmux_delivers_menu_keys_cancels_copy_mode_and_selects_option() {
    if !has_tmux_and_python3() {
        return;
    }

    let root = mk_dir("local-menu");
    let sock = format!("sf-key-local-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let menu_py = write_menu_script(&root);
    let log_path = root.join("local_menu.log");
    let pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("local-menu".into()),
            create_session: true,
            window_name: "claude-mcp".into(),
            cwd: root.clone(),
            env: vec![],
            argv: vec![
                "python3".into(),
                "-u".into(),
                menu_py.to_string_lossy().into_owned(),
                log_path.to_string_lossy().into_owned(),
            ],
        })
        .unwrap();
    wait_for_log_contains(&log_path, "READY", Duration::from_secs(3)).await;

    let tmux_wrap = write_tmux_wrapper(&root, &sock);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    core.ingest(EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("PermissionRequest".into()),
        payload: json!({
            "session_id": "s-menu-local",
            "hook_event_name": "PermissionRequest",
            "cwd": root.to_string_lossy(),
            "tool_name": "mcp",
        }),
        env: EnvCtx {
            tmux_pane: Some(pane.0.clone()),
            tmux: Some("/tmp/tmux,1,0".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    })
    .await;

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    let tok = core.web_token();

    // Put the pane in tmux copy-mode first to verify `Tmux::send_key` cancels copy-mode cleanly.
    let enter_copy = Command::new("tmux")
        .args(["-L", &sock, "copy-mode", "-t", pane.as_str()])
        .status()
        .unwrap();
    assert!(enter_copy.success());
    assert_eq!(sb_tmux.pane_mode(&pane).unwrap(), "copy-mode");

    // 1. Send `Down` -> exits copy-mode and moves cursor to option 2.
    let (st_down, body_down) = post_json(
        addr,
        "/api/sessions/s-menu-local/key",
        Some(tok),
        &json!({"key": "Down"}),
    )
    .await;
    assert_eq!(st_down, 200, "{body_down}");
    assert_eq!(body_down["ok"], true);
    assert_eq!(body_down["data"]["session_id"], "s-menu-local");
    assert_eq!(body_down["data"]["key"], "Down");
    assert_eq!(body_down["data"]["sent"], true);
    wait_for_log_contains(&log_path, "KEY:Down", Duration::from_secs(3)).await;

    // 2. Send `Enter` -> confirms option 2 ("Use this MCP server").
    let (st_enter, body_enter) = post_json(
        addr,
        "/api/sessions/s-menu-local/key",
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_enter, 200, "{body_enter}");
    assert_eq!(body_enter["data"]["key"], "Enter");
    assert_eq!(body_enter["data"]["sent"], true);
    let log = wait_for_log_contains(
        &log_path,
        "KEY:Enter:SELECTED:1:Use this MCP server",
        Duration::from_secs(3),
    )
    .await;
    assert!(log.contains("KEY:Down"));

    // 3. Also verify literal single-char (`"1"`) and `"Escape"` delivery.
    let (st_one, _) = post_json(
        addr,
        "/api/sessions/s-menu-local/key",
        Some(tok),
        &json!({"key": "1"}),
    )
    .await;
    assert_eq!(st_one, 200);
    wait_for_log_contains(&log_path, "KEY:1", Duration::from_secs(3)).await;

    let (st_esc, _) = post_json(
        addr,
        "/api/sessions/s-menu-local/key",
        Some(tok),
        &json!({"key": "Escape"}),
    )
    .await;
    assert_eq!(st_esc, 200);
    wait_for_log_contains(&log_path, "KEY:Escape", Duration::from_secs(3)).await;

    server.abort();
}

#[tokio::test]
async fn ssh_conduit_send_key_delivers_menu_keys_cancels_copy_mode_and_clears_attention() {
    if !has_tmux_and_python3() {
        return;
    }

    let root = mk_dir("ssh-key");
    let sock = format!("sf-key-ssh-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let real_tmux = real_bin("tmux");
    let sb_tmux = Tmux::new(PathBuf::from(&real_tmux), Some(sock.clone()));
    let remote_home = root.join("remote_home");
    let proj_cwd = remote_home.join("openkernel");
    std::fs::create_dir_all(&proj_cwd).unwrap();

    let menu_py = write_menu_script(&root);
    let log_path = root.join("remote_menu.log");
    let ssh_log = root.join("ssh_events.log");

    // Launch menu script under a process whose argv[0] is `claude` (with `; true` so bash does not
    // exec-optimize itself away) so SSH discovery classifies it as Claude AND detects the rendered
    // MCP approval modal as `NeedsPermission`!
    let bash_body = format!(
        "exec -a claude /bin/bash -c 'python3 -u \"{}\" \"{}\"; true' claude",
        menu_py.display(),
        log_path.display()
    );
    let pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("work".into()),
            create_session: true,
            window_name: "openkernel-claude".into(),
            cwd: proj_cwd.clone(),
            env: vec![],
            argv: vec!["/bin/bash".into(), "-c".into(), bash_body],
        })
        .unwrap();
    wait_for_log_contains(&log_path, "READY", Duration::from_secs(3)).await;

    // `fake-ssh` supporting persistent `/bin/sh -s` conduit and logging SPAWN / EXEC.
    let fake_ssh = root.join("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            r#"#!/bin/sh
LOG="{log}"
export HOME="{rhome}"
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  printf 'EXIT\n' >> "$LOG"
  exit 0
fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
printf 'SPAWN|%s\n' "$1" >> "$LOG"
if [ "$1" = "/bin/sh -s" ]; then
  FIFO="{root}/c.$$.fifo"
  rm -f "$FIFO"
  mkfifo "$FIFO"
  /bin/sh -s <"$FIFO" &
  SH_PID=$!
  exec 8>"$FIFO"
  rm -f "$FIFO"
  trap 'exec 8>&- 2>/dev/null || true; kill -TERM "$SH_PID" 2>/dev/null || true; exit 0' INT TERM HUP
  FIRST=1
  while IFS= read -r _line; do
    if [ "$FIRST" -eq 1 ]; then
      FIRST=0
      printf '%s\n' "$_line" >&8 || break
      continue
    fi
    _op=${{_line%% *}}
    if [ "$_op" = "EXEC" ]; then
      printf 'EXEC\n' >> "$LOG"
    fi
    printf '%s\n' "$_line" >&8 || break
  done
  exec 8>&- 2>/dev/null || true
  wait "$SH_PID"
  exit $?
fi
exec /bin/sh -c "$1"
"#,
            log = ssh_log.display(),
            rhome = remote_home.display(),
            root = root.display(),
        ),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let node_cfg = NodeConfig {
        id: "mac-ssh".into(),
        display_name: Some("Mac SSH".into()),
        kind: NodeKindConfig::Ssh,
        enabled: true,
        push_watch: false,
        ssh_target: "dev@mac-ssh.example".into(),
        ssh_bin: fake_ssh.to_string_lossy().into_owned(),
        tmux_bin: real_tmux,
        tmux_socket: sock.clone(),
        connect_timeout_secs: 5,
        ..NodeConfig::default()
    };
    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: addr.port(),
            auth_enabled: true,
            ..WebConfig::default()
        },
        nodes: vec![node_cfg],
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;

    let state_dir = root.join("hub-state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.sync_remote_nodes().await;

    let remote_session = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.effective_host_id() == "mac-ssh" && !s.is_subagent)
        .expect("remote SSH Claude session discovered");
    assert_eq!(
        remote_session.state,
        SessionState::NeedsPermission,
        "remote MCP approval modal should be detected as NeedsPermission, got {remote_session:?}"
    );
    assert!(
        core.snapshot().attention.iter().any(|a| {
            a.kind == attention::BLOCKED
                && a.session_id.as_ref() == Some(&remote_session.session_id)
        }),
        "pending attention must contain remote NeedsPermission session"
    );

    let app = starfane_daemon::web::router(core.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    let tok = core.web_token();
    let enc_sid = pct_encode(remote_session.session_id.as_str());

    // Enter copy-mode on remote pane before sending `Down`.
    let copy_st = Command::new("tmux")
        .args(["-L", &sock, "copy-mode", "-t", pane.as_str()])
        .status()
        .unwrap();
    assert!(copy_st.success());

    let spawns_before_keys = std::fs::read_to_string(&ssh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("SPAWN|"))
        .count();

    // 1. Invalid key must fail with 400 without spawning or executing SSH.
    let (st_bad, body_bad) = post_json(
        addr,
        &format!("/api/sessions/{enc_sid}/key"),
        Some(tok),
        &json!({"key": "Enter; rm -rf /"}),
    )
    .await;
    assert_eq!(st_bad, 400, "{body_bad}");

    // 2. Send `Down` over persistent `ConduitLane::Interactive` -> cancels copy-mode + moves cursor!
    let (st_down, body_down) = post_json(
        addr,
        &format!("/api/sessions/{enc_sid}/key"),
        Some(tok),
        &json!({"key": "Down"}),
    )
    .await;
    assert_eq!(st_down, 200, "{body_down}");
    assert_eq!(body_down["ok"], true);
    assert_eq!(
        body_down["data"]["session_id"],
        remote_session.session_id.as_str()
    );
    assert_eq!(body_down["data"]["key"], "Down");
    assert_eq!(body_down["data"]["sent"], true);
    wait_for_log_contains(&log_path, "KEY:Down", Duration::from_secs(3)).await;

    // 3. Send `Enter` -> confirms option 2 and transitions screen to idle `❯ ` prompt!
    let (st_enter, body_enter) = post_json(
        addr,
        &format!("/api/sessions/{enc_sid}/key"),
        Some(tok),
        &json!({"key": "Enter"}),
    )
    .await;
    assert_eq!(st_enter, 200, "{body_enter}");
    assert_eq!(body_enter["data"]["sent"], true);
    wait_for_log_contains(
        &log_path,
        "KEY:Enter:SELECTED:1:Use this MCP server",
        Duration::from_secs(3),
    )
    .await;

    // 4. Post-key targeted dirty sync (~120ms settle) clears `NeedsPermission` -> `Working`/`Idle`,
    //    clears `last_error`, and removes the session from the pending attention queue automatically!
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let snap = core.snapshot();
        let rec = snap
            .sessions
            .iter()
            .find(|s| s.session_id == remote_session.session_id);
        let cleared = !snap
            .attention
            .iter()
            .any(|a| a.session_id.as_ref() == Some(&remote_session.session_id));
        if rec.is_some_and(|r| {
            matches!(r.state, SessionState::Working | SessionState::Idle) && r.last_error.is_none()
        }) && cleared
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for post-key dirty sync to clear NeedsPermission; rec={rec:?}, attention={:?}",
            snap.attention
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 5. Persistent interactive conduit was reused across both key sends (at most 1 Interactive lane spawn).
    let spawns_after_keys = std::fs::read_to_string(&ssh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("SPAWN|"))
        .count();
    assert!(
        spawns_after_keys <= spawns_before_keys + 1,
        "expected persistent Interactive conduit reuse across keys (before={spawns_before_keys}, after={spawns_after_keys})"
    );

    server.abort();
}

#[tokio::test]
async fn peer_daemon_node_provider_proxies_send_key_over_http() {
    if !has_tmux_and_python3() {
        return;
    }

    let root = mk_dir("peer-key");
    let peer_dir = root.join("peer");
    let hub_dir = root.join("hub");
    std::fs::create_dir_all(&peer_dir).unwrap();
    std::fs::create_dir_all(&hub_dir).unwrap();

    let sock = format!("sf-key-peer-{}", id::new_id("S").to_lowercase());
    let _guard = SandboxEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let sb_tmux = Tmux::new(PathBuf::from("tmux"), Some(sock.clone()));
    let menu_py = write_menu_script(&peer_dir);
    let log_path = peer_dir.join("peer_menu.log");
    let peer_pane = sb_tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("peer-work".into()),
            create_session: true,
            window_name: "w1".into(),
            cwd: peer_dir.clone(),
            env: vec![],
            argv: vec![
                "python3".into(),
                "-u".into(),
                menu_py.to_string_lossy().into_owned(),
                log_path.to_string_lossy().into_owned(),
            ],
        })
        .unwrap();
    wait_for_log_contains(&log_path, "READY", Duration::from_secs(3)).await;

    let tmux_wrap = write_tmux_wrapper(&peer_dir, &sock);
    let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_listener.local_addr().unwrap();
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
            hook_event: Some("PermissionRequest".into()),
            payload: json!({
                "session_id": "peer-cl-1",
                "hook_event_name": "PermissionRequest",
                "cwd": peer_dir.to_string_lossy(),
                "tool_name": "mcp",
            }),
            env: EnvCtx {
                tmux_pane: Some(peer_pane.0.clone()),
                tmux: Some("/tmp/tmux,1,0".into()),
                ..EnvCtx::default()
            },
            recv_ts_ms: id::now_ms(),
        })
        .await;

    let peer_app = starfane_daemon::web::router(peer_core.clone());
    let peer_srv = tokio::spawn(async move {
        let _ = axum::serve(
            peer_listener,
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

    let fed_sid = SessionId::new("mac-peer:peer-cl-1");
    let res_down = hub_core.send_session_key(&fed_sid, "Down").await.unwrap();
    assert_eq!(res_down["session_id"], fed_sid.as_str());
    assert_eq!(res_down["key"], "Down");
    assert_eq!(res_down["sent"], true);

    let res_enter = hub_core.send_session_key(&fed_sid, "Enter").await.unwrap();
    assert_eq!(res_enter["key"], "Enter");
    assert_eq!(res_enter["sent"], true);

    wait_for_log_contains(
        &log_path,
        "KEY:Enter:SELECTED:1:Use this MCP server",
        Duration::from_secs(3),
    )
    .await;

    // Invalid key rejected locally before proxying to peer.
    let err = hub_core
        .send_session_key(&fed_sid, "Enter; rm -rf /")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);

    peer_srv.abort();
}
