//! End-to-end deterministic integration tests for Starfane Remote Architecture v2:
//! 1. Persistent dual-lane SSH conduit reuse, true `rtt_ms` vs `sync_ms` separation & FD 3/4 isolation
//! 2. Discovery v3 optimizations (wrap-safe `awk` PID tree, batched `stat`, shell-pane skip, inline `<=64KiB` hot delta -> Hub `/api/events` SSE with 0 extra SSH RTTs)
//! 3. Zero-install `tmux -C` push watcher (`SshTmuxControlWatcher`) + `Core` supervisor targeted `sync_dirty_panes` + `/api/events` SSE
//! 4. Peer v2 `/api/events` SSE push + `Last-Event-Id` reconnect + HTTP/1.1 Keep-Alive pool + `SshLoopbackTunnel` bootstrap/forward/cancel

use serde_json::{json, Value};
use starfane_daemon::config::{NodeConfig, NodeKindConfig, ScopedTokenConfig, WebConfig};
use starfane_daemon::nodes::{NodeProvider, PeerDaemonNodeProvider};
use starfane_daemon::peer_client::{
    PeerKeepAlivePool, PeerSseEndpoint, PeerSseSignal, PeerSseSubscriber, SshLoopbackTunnel,
};
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use starfane_tmux::{ConduitLane, SshConduitPool, SshExec, Tmux};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

static V2_COUNTER: AtomicU64 = AtomicU64::new(0);

fn has_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
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

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn encode_claude_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mk_temp_root(label: &str) -> (PathBuf, TempDirGuard) {
    let seq = V2_COUNTER.fetch_add(1, Ordering::Relaxed);
    let raw = std::env::temp_dir().join(format!("sf-v2-{label}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&raw).unwrap();
    let canon = std::fs::canonicalize(&raw).unwrap_or(raw);
    (canon.clone(), TempDirGuard(canon))
}

struct SandboxTmux {
    socket: String,
    tmux: Tmux,
    real_tmux: String,
}

impl SandboxTmux {
    fn new(label: &str) -> Self {
        let seq = V2_COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = format!("sf-v2-{label}-{}-{seq}", std::process::id());
        let real_tmux = real_bin("tmux");
        let tmux = Tmux::new(PathBuf::from(&real_tmux), Some(socket.clone()));
        Self {
            socket,
            tmux,
            real_tmux,
        }
    }
}

impl Drop for SandboxTmux {
    fn drop(&mut self) {
        let _ = Command::new(&self.real_tmux)
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

async fn start_hub_with_web(
    state_dir: &Path,
    nodes: Vec<NodeConfig>,
    mut web_cfg: WebConfig,
) -> (Core, SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    web_cfg.bind = "127.0.0.1".into();
    web_cfg.port = addr.port();
    let mut cfg = StarfaneConfig {
        web: web_cfg,
        nodes,
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.to_path_buf(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    let app = starfane_daemon::web::router(core.clone());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (core, addr, srv)
}

async fn http_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let body_str = body.map(|b| serde_json::to_string(b).unwrap());
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
        addr.port()
    );
    if let Some(tok) = token {
        req.push_str(&format!("X-Starfane-Token: {tok}\r\n"));
    }
    if method != "GET" && method != "HEAD" {
        req.push_str(&format!("Origin: http://127.0.0.1:{}\r\n", addr.port()));
    }
    if let Some(ref b) = body_str {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(ref b) = body_str {
        req.push_str(b);
    }

    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let full = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = full.split_once("\r\n\r\n").unwrap_or((&full, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let is_chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    let body_text = if is_chunked {
        decode_chunked_text(rest)
    } else {
        rest.to_string()
    };
    let val = serde_json::from_str(&body_text).unwrap_or_else(|_| json!({ "raw": body_text }));
    (status, val)
}

fn decode_chunked_text(src: &str) -> String {
    let mut out = String::new();
    let mut rem = src;
    while let Some((sz_line, after)) = rem.split_once("\r\n") {
        let hex = sz_line.split(';').next().unwrap_or("").trim();
        let Ok(n) = usize::from_str_radix(hex, 16) else {
            break;
        };
        if n == 0 {
            break;
        }
        if after.len() < n {
            out.push_str(after);
            break;
        }
        out.push_str(&after[..n]);
        rem = after[n..].strip_prefix("\r\n").unwrap_or(&after[n..]);
    }
    out
}

/// Live HTTP/1.1 `/api/events` SSE reader decoding chunked or identity SSE frames.
struct SseClient {
    reader: BufReader<TcpStream>,
    chunked: bool,
    chunk_rem: usize,
    line_buf: Vec<u8>,
}

#[derive(Debug, Clone)]
struct SseFrame {
    id: Option<String>,
    event: String,
    data: Value,
}

impl SseClient {
    async fn connect(addr: SocketAddr, token: Option<&str>) -> Self {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let mut req = format!(
            "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: text/event-stream\r\n",
            addr.port()
        );
        if let Some(tok) = token {
            req.push_str(&format!("X-Starfane-Token: {tok}\r\n"));
        }
        req.push_str("\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(sock);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.unwrap();
        assert!(
            status_line.contains(" 200 "),
            "expected HTTP 200 on /api/events, got {status_line:?}"
        );
        let mut chunked = false;
        loop {
            let mut hdr = String::new();
            reader.read_line(&mut hdr).await.unwrap();
            if hdr == "\r\n" || hdr == "\n" || hdr.is_empty() {
                break;
            }
            if hdr.to_ascii_lowercase().starts_with("transfer-encoding:")
                && hdr.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
        Self {
            reader,
            chunked,
            chunk_rem: 0,
            line_buf: Vec::new(),
        }
    }

    async fn read_body_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
                let mut line = self.line_buf.drain(..=pos).collect::<Vec<u8>>();
                if line.ends_with(b"\n") {
                    line.pop();
                }
                if line.ends_with(b"\r") {
                    line.pop();
                }
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            if self.chunked {
                if self.chunk_rem == 0 {
                    let mut sz = String::new();
                    let n = self.reader.read_line(&mut sz).await.ok()?;
                    if n == 0 {
                        return None;
                    }
                    let hex = sz.split(';').next().unwrap_or("").trim();
                    let parsed = usize::from_str_radix(hex, 16).ok()?;
                    if parsed == 0 {
                        return None;
                    }
                    self.chunk_rem = parsed;
                }
                let mut tmp = vec![0u8; self.chunk_rem.min(4096)];
                let n = self.reader.read(&mut tmp).await.ok()?;
                if n == 0 {
                    return None;
                }
                self.chunk_rem -= n;
                self.line_buf.extend_from_slice(&tmp[..n]);
                if self.chunk_rem == 0 {
                    let mut crlf = [0u8; 2];
                    let _ = self.reader.read_exact(&mut crlf).await;
                }
            } else {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await.ok()?;
                if n == 0 {
                    return None;
                }
                return Some(line.trim_end_matches(['\r', '\n']).to_string());
            }
        }
    }

    async fn next_event(&mut self, timeout: Duration) -> Option<SseFrame> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut id = None;
        let mut event = String::new();
        let mut data_lines = Vec::new();
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return None;
            }
            let line = tokio::time::timeout(rem, self.read_body_line())
                .await
                .ok()??;
            if line.is_empty() {
                if !data_lines.is_empty() {
                    let raw = data_lines.join("\n");
                    let data = serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "raw": raw }));
                    let ev_name = if event.is_empty() {
                        "message".to_string()
                    } else {
                        std::mem::take(&mut event)
                    };
                    return Some(SseFrame {
                        id: id.take(),
                        event: ev_name,
                        data,
                    });
                }
                id = None;
                event.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data_lines.push(v.trim_start().to_string());
            }
        }
    }

    async fn wait_for<F>(&mut self, timeout: Duration, mut pred: F) -> Option<SseFrame>
    where
        F: FnMut(&SseFrame) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return None;
            }
            let frame = self.next_event(rem).await?;
            if pred(&frame) {
                return Some(frame);
            }
        }
    }
}

struct ClaudePaneSpec<'a> {
    session_name: &'a str,
    create_session: bool,
    window_name: &'a str,
    cwd: &'a Path,
    cl_sid: &'a str,
    initial_user: &'a str,
    initial_reply: &'a str,
    recv_log: Option<&'a Path>,
}

fn spawn_claude_pane_with_transcript(
    sb: &SandboxTmux,
    remote_home: &Path,
    spec: &ClaudePaneSpec<'_>,
) -> (PaneId, PathBuf) {
    let cwd = spec.cwd;
    let cl_sid = spec.cl_sid;
    let initial_user = spec.initial_user;
    let initial_reply = spec.initial_reply;
    std::fs::create_dir_all(cwd).unwrap();
    let enc = encode_claude_project_dir(&cwd.to_string_lossy());
    let tr_path = remote_home
        .join(".claude/projects")
        .join(&enc)
        .join(format!("{cl_sid}.jsonl"));
    std::fs::create_dir_all(tr_path.parent().unwrap()).unwrap();
    std::fs::write(
        &tr_path,
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-11-26T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{initial_user}\"}}}}\n\
             {{\"type\":\"assistant\",\"timestamp\":\"2026-11-26T00:00:02.000Z\",\"message\":{{\"id\":\"m0\",\"model\":\"claude-sonnet-4-6\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"{initial_reply}\"}}],\"usage\":{{\"input_tokens\":1000,\"cache_read_input_tokens\":500,\"cache_creation_input_tokens\":0,\"output_tokens\":80}}}}}}\n"
        ),
    )
    .unwrap();

    let sess_dir = remote_home.join(".claude/sessions");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let body = if let Some(rlog) = spec.recv_log {
        format!(
            r#"exec -a claude /bin/bash -c 'NOW_MS=$(($(date +%s) * 1000)); printf "{{\"pid\":%s,\"sessionId\":\"{cl_sid}\",\"cwd\":\"{cwd}\",\"startedAt\":%s}}\n" "$$" "$NOW_MS" > "{sdir}/$$.json"; printf "claude ready\n❯ "; while IFS= read -r line; do printf "RECV:%s\n" "$line" >> "{rlog}"; printf "\nSubmitted: %s\n❯ " "$line"; done; sleep 120; true' claude"#,
            cl_sid = cl_sid,
            cwd = cwd.display(),
            sdir = sess_dir.display(),
            rlog = rlog.display(),
        )
    } else {
        format!(
            r#"exec -a claude /bin/bash -c 'NOW_MS=$(($(date +%s) * 1000)); printf "{{\"pid\":%s,\"sessionId\":\"{cl_sid}\",\"cwd\":\"{cwd}\",\"startedAt\":%s}}\n" "$$" "$NOW_MS" > "{sdir}/$$.json"; printf "claude ready\n❯ "; sleep 120; true' claude"#,
            cl_sid = cl_sid,
            cwd = cwd.display(),
            sdir = sess_dir.display(),
        )
    };
    let pane = sb
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some(spec.session_name.into()),
            create_session: spec.create_session,
            window_name: spec.window_name.into(),
            cwd: cwd.to_path_buf(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), body],
        })
        .unwrap();
    (pane, tr_path)
}

fn append_claude_turn(tr_path: &Path, idx: usize, user_text: &str, assistant_text: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(tr_path)
        .unwrap();
    writeln!(
        f,
        "{{\"type\":\"user\",\"timestamp\":\"2026-11-26T00:01:{idx:02}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{user_text}\"}}}}"
    )
    .unwrap();
    writeln!(
        f,
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-11-26T00:01:{idx:02}.500Z\",\"message\":{{\"id\":\"m{idx}\",\"model\":\"claude-sonnet-4-6\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"{assistant_text}\"}}],\"usage\":{{\"input_tokens\":1400,\"cache_read_input_tokens\":600,\"cache_creation_input_tokens\":0,\"output_tokens\":95}}}}}}"
    )
    .unwrap();
}

// ===========================================================================
// Test 1: Persistent Conduit Reuse, True `rtt_ms` vs `sync_ms` Separation &
//         FD 3/4 Isolation
// ===========================================================================
#[tokio::test]
async fn v2_persistent_conduit_reuse_rtt_vs_sync_separation_and_fd_isolation() {
    if !has_tmux() {
        return;
    }
    let (root, _guard) = mk_temp_root("conduit-rtt");
    let sb = SandboxTmux::new("conduit-rtt");
    let remote_home = root.join("remote_home");
    let proj_cwd = remote_home.join("repo-rtt");
    let recv_log = root.join("pane_recv.log");
    let ssh_log = root.join("ssh_events.log");
    let exec_delay_file = root.join("exec_delay_sec");
    std::fs::write(&exec_delay_file, "0\n").unwrap();

    let (pane, _tr) = spawn_claude_pane_with_transcript(
        &sb,
        &remote_home,
        &ClaudePaneSpec {
            session_name: "work",
            create_session: true,
            window_name: "claude-1",
            cwd: &proj_cwd,
            cl_sid: "cl-v2-rtt-1111-2222-333344445555",
            initial_user: "hello rtt",
            initial_reply: "world rtt",
            recv_log: Some(&recv_log),
        },
    );
    tokio::time::sleep(Duration::from_millis(250)).await;

    // `fake-ssh` logging SPAWN / PING / EXEC separately and applying `exec_delay_file` ONLY to EXEC
    // (simulating real remote discovery / sleep-padded dispatch work while transport PING stays fast!).
    let fake_ssh = root.join("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            r#"#!/bin/sh
LOG="{log}"
DELAY_FILE="{delay}"
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
    if [ "$_op" = "PING" ]; then
      printf 'PING\n' >> "$LOG"
    elif [ "$_op" = "EXEC" ]; then
      printf 'EXEC\n' >> "$LOG"
      _d=$(head -n 1 "$DELAY_FILE" 2>/dev/null | tr -d '\r\n')
      if [ -n "$_d" ] && [ "$_d" != "0" ]; then
        sleep "$_d"
      fi
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
            delay = exec_delay_file.display(),
            rhome = remote_home.display(),
            root = root.display(),
        ),
    );

    // 1. Direct FD 3/4 isolation check on `SshConduitPool`:
    //    inner script attempts to read stdin (`cat`) and prints spoofed `\x1eSF_RES` framing noise.
    let direct_ssh = SshExec::new("dev@fd-iso.example", root.join("cm-fd.sock"))
        .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
    let direct_pool = SshConduitPool::new(direct_ssh);
    let noisy = direct_pool
        .exec_text(
            ConduitLane::Interactive,
            "cat >/dev/null; printf '\\036SF_RES 999 0 0 0\\nnoisy-ok\\n'; printf 'err-noise\\n' >&2",
            Some(b"user-stdin-only"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(noisy.success);
    assert!(noisy.stdout.contains("noisy-ok"));
    assert!(noisy.stderr.contains("err-noise"));
    let ping_after = direct_pool.ping(Duration::from_secs(5)).await.unwrap();
    assert!(
        ping_after < 100,
        "PING after FD-noise script must remain fast and uncorrupted, got {ping_after}ms"
    );
    direct_pool.reset().await;
    // Clear log before Hub E2E measurement.
    let _ = std::fs::write(&ssh_log, "");

    // 2. Full Hub `SshMuxNodeProvider` lifecycle: discovery + capture + transcript + dispatch + probe.
    let node_cfg = NodeConfig {
        id: "mac-v2".into(),
        display_name: Some("Mac V2".into()),
        kind: NodeKindConfig::Ssh,
        enabled: true,
        push_watch: false,
        ssh_target: "dev@mac-v2.example".into(),
        ssh_bin: fake_ssh.to_string_lossy().into_owned(),
        tmux_bin: sb.real_tmux.clone(),
        tmux_socket: sb.socket.clone(),
        connect_timeout_secs: 5,
        compression: true,
        ..NodeConfig::default()
    };
    let (core, addr, srv) = start_hub_with_web(
        &root.join("hub-state"),
        vec![node_cfg],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    // Initial sync warms persistent conduits (`lane_interactive` for PING + `lane_background` for v3 discover).
    core.sync_remote_host(&HostId::new("mac-v2")).await;
    let sid = SessionId::new(format!("mac-v2:tmux-{}", pane.as_str()));
    let enc_sid = sid.as_str().replace('%', "%25").replace(':', "%3A");
    let enc_pane = format!("mac-v2%3A%25{}", &pane.as_str()[1..]);

    // Inject 220ms pure-EXEC remote work delay (simulating heavy discovery + sleep-padded dispatch),
    // while keeping transport PING at 0ms!
    std::fs::write(&exec_delay_file, "0.22\n").unwrap();

    // Run 2 discovery syncs + 2 pane captures + transcript read + multiline sleep-padded dispatch + probe!
    core.sync_remote_host(&HostId::new("mac-v2")).await;
    core.sync_remote_host(&HostId::new("mac-v2")).await;
    let (st_pv1, pv1) = http_json(
        addr,
        "GET",
        &format!("/api/panes/{enc_pane}/preview?lines=20"),
        None,
        None,
    )
    .await;
    assert_eq!(st_pv1, 200, "{pv1}");
    let (st_pv2, pv2) = http_json(
        addr,
        "GET",
        &format!("/api/panes/{enc_pane}/preview?lines=40"),
        None,
        None,
    )
    .await;
    assert_eq!(st_pv2, 200, "{pv2}");
    let (st_tr, tr_res) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?limit=20"),
        None,
        None,
    )
    .await;
    assert_eq!(st_tr, 200, "{tr_res}");
    assert_eq!(tr_res["data"]["source"], "transcript");

    let disp = core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session { id: sid.clone() },
            text: "line one\nline two multiline dispatch".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .unwrap();
    assert_eq!(disp["status"], "delivered", "{disp}");

    let probe = core.probe_host("mac-v2").await.unwrap();
    assert_eq!(probe.status, HostStatus::Online);

    // Inspect HostRecord from HTTP `/api/hosts`: `rtt_ms` stays low pure-transport (<60ms),
    // while `sync_ms` accurately captures the >=200ms remote discovery duration!
    let (st_h, hosts_res) = http_json(addr, "GET", "/api/hosts", None, None).await;
    assert_eq!(st_h, 200, "{hosts_res}");
    let mac_host = hosts_res["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == "mac-v2")
        .expect("mac-v2 in /api/hosts");
    let rtt_ms = mac_host["rtt_ms"]
        .as_u64()
        .expect("rtt_ms must be populated");
    let sync_ms = mac_host["sync_ms"]
        .as_u64()
        .expect("sync_ms must be populated separately");
    assert!(
        rtt_ms < 60,
        "pure transport rtt_ms must stay unpolluted (<60ms) despite 220ms+ discovery & dispatch, got rtt_ms={rtt_ms} sync_ms={sync_ms}"
    );
    assert!(
        sync_ms >= 180,
        "HostRecord.sync_ms must reflect >=180ms discovery execution time separately from rtt_ms, got sync_ms={sync_ms}"
    );

    // Inspect SSH spawn vs logical call ratio: <= 2 persistent spawns across >= 8 logical calls!
    let log_text = std::fs::read_to_string(&ssh_log).unwrap_or_default();
    let spawns = log_text.lines().filter(|l| l.starts_with("SPAWN|")).count();
    let pings = log_text.lines().filter(|l| *l == "PING").count();
    let execs = log_text.lines().filter(|l| *l == "EXEC").count();
    assert!(
        spawns <= 2,
        "dual-lane persistent conduit must spawn at most 2 ssh children (got {spawns}; pings={pings}, execs={execs})"
    );
    assert!(
        pings + execs >= 8,
        "expected >= 8 logical conduit calls reused over {spawns} spawns (got pings={pings}, execs={execs})"
    );

    core.request_shutdown();
    srv.abort();
}

// ===========================================================================
// Test 2: Discovery v3 Optimization (Wrap-Safe `awk` PID Tree, Batched `stat`,
//         Shell-Pane Capture Skip) + Inline `<= 64 KiB` Hot Delta -> `/api/events` SSE
// ===========================================================================
#[tokio::test]
async fn v2_discovery_v3_awk_pid_wrap_batched_stat_shell_skip_and_inline_hot_delta_sse() {
    if !has_tmux() {
        return;
    }
    let (root, _guard) = mk_temp_root("v3-opt");
    let sb = SandboxTmux::new("v3-opt");
    let remote_home = root.join("remote_home");
    let cwd_a = remote_home.join("proj-a");
    let cwd_b = remote_home.join("proj-b");
    let sh_cwd = remote_home.join("plain-sh");
    std::fs::create_dir_all(&sh_cwd).unwrap();

    // 1. Spawn 2 real Claude panes + 1 plain shell pane (skipped) + 1 shell pane anchored to a
    //    synthetic PID-wrapped `claude` descendant (`child_pid 42 < wrapper_pid 120 < pane_pid`).
    let (p_cl1, tr_cl1) = spawn_claude_pane_with_transcript(
        &sb,
        &remote_home,
        &ClaudePaneSpec {
            session_name: "v3",
            create_session: true,
            window_name: "cl-main",
            cwd: &cwd_a,
            cl_sid: "cl-v3-main-0001-1111-222233334444",
            initial_user: "initial q1",
            initial_reply: "initial a1",
            recv_log: None,
        },
    );
    let (_p_cl2, _tr_cl2) = spawn_claude_pane_with_transcript(
        &sb,
        &remote_home,
        &ClaudePaneSpec {
            session_name: "v3",
            create_session: false,
            window_name: "cl-second",
            cwd: &cwd_b,
            cl_sid: "cl-v3-second-0001-1111-222233334444",
            initial_user: "second q1",
            initial_reply: "second a1",
            recv_log: None,
        },
    );
    let p_plain_shell = sb
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("v3".into()),
            create_session: false,
            window_name: "zsh-idle".into(),
            cwd: sh_cwd.clone(),
            env: Vec::new(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 120".into()],
        })
        .unwrap();
    let p_wrap_shell = sb
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("v3".into()),
            create_session: false,
            window_name: "wrap-pane".into(),
            cwd: sh_cwd.clone(),
            env: Vec::new(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 120".into()],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    let panes_now = sb.tmux.list_panes().unwrap();
    let wrap_pane_pid = panes_now
        .iter()
        .find(|p| p.pane_id == p_wrap_shell)
        .expect("wrap pane exists")
        .pane_pid;
    assert!(
        wrap_pane_pid > 120,
        "expected wrap pane PID > 120 to exercise child_pid < parent_pid wrap, got {wrap_pane_pid}"
    );

    // Wrapper shims observing exact `capture-pane` targets, batched `stat` calls, and injecting
    // wrap-around PID tree + 60 decoy OS processes into `ps`.
    let shim_bin = root.join("shim_bin");
    std::fs::create_dir_all(&shim_bin).unwrap();
    let capture_log = root.join("capture_targets.log");
    let stat_log = root.join("stat_calls.log");
    let ssh_log = root.join("ssh_v3.log");
    let real_stat = real_bin("stat");
    let real_ps = real_bin("ps");

    let tmux_shim = shim_bin.join("tmux");
    write_executable(
        &tmux_shim,
        &format!(
            r#"#!/bin/sh
PREV=""
IS_CAP=0
TGT=""
for a in "$@"; do
  if [ "$a" = "capture-pane" ]; then IS_CAP=1; fi
  if [ "$PREV" = "-t" ] && [ "$IS_CAP" -eq 1 ]; then TGT="$a"; fi
  PREV="$a"
done
if [ "$IS_CAP" -eq 1 ] && [ -n "$TGT" ]; then
  printf '%s\n' "$TGT" >> "{cap_log}"
fi
exec "{real_tmux}" "$@"
"#,
            cap_log = capture_log.display(),
            real_tmux = sb.real_tmux,
        ),
    );

    let stat_shim = shim_bin.join("stat");
    write_executable(
        &stat_shim,
        &format!(
            r#"#!/bin/sh
printf 'STAT|%s|%s\n' "$#" "$*" >> "{stat_log}"
exec "{real_stat}" "$@"
"#,
            stat_log = stat_log.display(),
            real_stat = real_stat,
        ),
    );

    let ps_shim = shim_bin.join("ps");
    write_executable(
        &ps_shim,
        &format!(
            r#"#!/bin/sh
"{real_ps}" "$@"
# Emit wrap-around descendant chain (`42 < 120 < {wrap_pid}`) BEFORE any higher PIDs,
# plus 60 unrelated decoy OS processes that `awk` must filter out completely!
printf '   42   120 00:12 /usr/local/bin/claude --dangerously-skip-permissions\n'
printf '  120 {wrap_pid} 00:15 /bin/sh -c wrapper\n'
_i=1
while [ "$_i" -le 60 ]; do
  printf '%5d     1 01:00 /usr/libexec/decoy-daemon-%d\n' "$((50000 + _i))" "$_i"
  _i=$((_i + 1))
done
"#,
            real_ps = real_ps,
            wrap_pid = wrap_pane_pid,
        ),
    );

    let fake_ssh = root.join("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            r#"#!/bin/sh
LOG="{ssh_log}"
export HOME="{rhome}"
export PATH="{shim_bin}:$PATH"
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then exit 0; fi
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
    if [ "$_op" = "PING" ] || [ "$_op" = "EXEC" ]; then
      printf '%s\n' "$_op" >> "$LOG"
    fi
    printf '%s\n' "$_line" >&8 || break
  done
  exec 8>&- 2>/dev/null || true
  wait "$SH_PID"
  exit $?
fi
exec /bin/sh -c "$1"
"#,
            ssh_log = ssh_log.display(),
            rhome = remote_home.display(),
            shim_bin = shim_bin.display(),
            root = root.display(),
        ),
    );

    let node_cfg = NodeConfig {
        id: "host-v3".into(),
        display_name: Some("Host V3".into()),
        kind: NodeKindConfig::Ssh,
        enabled: true,
        include_shell_panes: false,
        push_watch: false,
        ssh_target: "dev@host-v3.example".into(),
        ssh_bin: fake_ssh.to_string_lossy().into_owned(),
        tmux_bin: tmux_shim.to_string_lossy().into_owned(),
        tmux_socket: sb.socket.clone(),
        connect_timeout_secs: 5,
        compression: true,
        ..NodeConfig::default()
    };
    let (core, addr, srv) = start_hub_with_web(
        &root.join("hub-state"),
        vec![node_cfg],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    // Tick 1: full v3 discovery.
    core.sync_remote_host(&HostId::new("host-v3")).await;
    let snap = core.snapshot();

    // (a) Wrap-safe `awk` PID tree: `p_wrap_shell` (`42 -> 120 -> wrap_pane_pid`) MUST be detected as Claude!
    let wrap_sid = format!("host-v3:tmux-{}", p_wrap_shell.as_str());
    let wrap_sess = snap
        .sessions
        .iter()
        .find(|s| s.session_id.as_str() == wrap_sid)
        .expect("PID-wrap child claude (42 < 120 < pane_pid) must be discovered by awk fixed-point closure");
    assert_eq!(wrap_sess.agent, AgentKind::Claude);

    // (b) Shell-pane skip: plain shell pane `p_plain_shell` MUST NOT be discovered OR captured!
    let plain_sid = format!("host-v3:tmux-{}", p_plain_shell.as_str());
    assert!(
        snap.sessions
            .iter()
            .all(|s| s.session_id.as_str() != plain_sid),
        "plain shell pane must be excluded when include_shell_panes=false"
    );
    let captured_targets = std::fs::read_to_string(&capture_log).unwrap_or_default();
    assert!(
        captured_targets.lines().any(|l| l.trim() == p_cl1.as_str()),
        "agent pane {p_cl1} must be captured: {captured_targets:?}"
    );
    assert!(
        captured_targets
            .lines()
            .all(|l| l.trim() != p_plain_shell.as_str()),
        "plain shell pane {p_plain_shell} must NEVER be captured when include_shell_panes=false: {captured_targets:?}"
    );

    // (c) Batched OS-native `stat`: `sf_batch_stat` must batch multiple transcript files into 1 call!
    let stat_lines = std::fs::read_to_string(&stat_log).unwrap_or_default();
    let has_batched_tr_stat = stat_lines.lines().any(|l| {
        // `STAT|<argc>|-f %i %z %m %N <path1> <path2> ...` (argc >= 4)
        let mut parts = l.splitn(3, '|');
        let _ = parts.next();
        let argc = parts
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let args = parts.next().unwrap_or("");
        argc >= 4 && args.contains(".claude/projects/")
    });
    assert!(
        has_batched_tr_stat,
        "expected a single batched OS-native `stat` call covering multiple transcript files:\n{stat_lines}"
    );

    // (d) Inline `<= 64 KiB` hot delta -> Hub `/api/events` SSE `event: output` with 0 extra SSH RTTs!
    let mut sse = SseClient::connect(addr, None).await;
    // Consume initial snapshot frame if any, or just wait specifically for `event: output`.
    append_claude_turn(
        &tr_cl1,
        2,
        "hot delta question",
        "v3-inline-hot-delta-reply",
    );

    let log_before = std::fs::read_to_string(&ssh_log).unwrap_or_default();
    let spawns_before = log_before
        .lines()
        .filter(|l| l.starts_with("SPAWN|"))
        .count();
    let execs_before = log_before.lines().filter(|l| *l == "EXEC").count();

    core.sync_remote_host(&HostId::new("host-v3")).await;

    let log_after = std::fs::read_to_string(&ssh_log).unwrap_or_default();
    let spawns_after = log_after
        .lines()
        .filter(|l| l.starts_with("SPAWN|"))
        .count();
    let execs_after = log_after.lines().filter(|l| *l == "EXEC").count();

    assert_eq!(
        spawns_after - spawns_before,
        0,
        "inline hot delta sync must reuse persistent conduit (0 new ssh spawns)"
    );
    assert_eq!(
        execs_after - execs_before,
        1,
        "<=64KiB hot delta must ride inline inside the 1 single discovery EXEC (0 extra SSH round-trips!)"
    );

    let target_sid = format!("host-v3:tmux-{}", p_cl1.as_str());
    let output_ev = sse
        .wait_for(Duration::from_secs(3), |f| {
            f.event == "output" && f.data["session_id"].as_str() == Some(target_sid.as_str())
        })
        .await
        .expect("expected Hub /api/events SSE `event: output` from inline hot delta");
    assert!(output_ev.id.is_some(), "SSE output frame must carry `id:`");
    assert_eq!(output_ev.data["dirty"], json!(false), "{output_ev:?}");
    assert_eq!(output_ev.data["start_seq"], json!(2), "{output_ev:?}");
    assert_eq!(output_ev.data["total_messages"], json!(4), "{output_ev:?}");
    let appended = output_ev.data["appended_messages"].as_array().unwrap();
    assert_eq!(appended.len(), 2, "{output_ev:?}");
    assert_eq!(appended[1]["text"], json!("v3-inline-hot-delta-reply"));

    core.request_shutdown();
    srv.abort();
}

// ===========================================================================
// Test 3: `tmux -C` Push Watcher (`SshTmuxControlWatcher`) + `Core` Supervisor
//         Targeted `sync_dirty_panes` (Sibling Preservation) + `/api/events` SSE
// ===========================================================================
#[tokio::test]
async fn v2_tmux_control_push_watcher_dirty_pane_sync_and_sse_output() {
    if !has_tmux() {
        return;
    }
    let (root, _guard) = mk_temp_root("tmux-push");
    let sb = SandboxTmux::new("tmux-push");
    let remote_home = root.join("remote_home");
    let cwd_a = remote_home.join("proj-a");
    let cwd_b = remote_home.join("proj-b");

    let (pane_a, tr_a) = spawn_claude_pane_with_transcript(
        &sb,
        &remote_home,
        &ClaudePaneSpec {
            session_name: "push-s",
            create_session: true,
            window_name: "cl-a",
            cwd: &cwd_a,
            cl_sid: "cl-push-a-0001-1111-222233334444",
            initial_user: "pane a q0",
            initial_reply: "pane a a0",
            recv_log: None,
        },
    );
    let (pane_b, tr_b) = spawn_claude_pane_with_transcript(
        &sb,
        &remote_home,
        &ClaudePaneSpec {
            session_name: "push-s",
            create_session: false,
            window_name: "cl-b",
            cwd: &cwd_b,
            cl_sid: "cl-push-b-0001-1111-222233334444",
            initial_user: "pane b q0",
            initial_reply: "pane b a0",
            recv_log: None,
        },
    );
    tokio::time::sleep(Duration::from_millis(250)).await;

    let watch_in_log = root.join("watch_in.log");
    let watch_emit_file = root.join("watch_emit.txt");
    let digest_file = root.join("digest_state.txt");
    let exec_kind_log = root.join("exec_kinds.log");

    // Initial multi-session digest baseline (same history_size=10 for both panes).
    std::fs::write(
        &digest_file,
        format!(
            "{}|10|2|5|0|0|claude\n{}|10|2|5|0|0|claude\n",
            pane_a.as_str(),
            pane_b.as_str()
        ),
    )
    .unwrap();
    std::fs::write(&watch_emit_file, "").unwrap();

    // Fake `tmux -C` script invoked whenever `SshTmuxControlWatcher` runs `tmux ... -C attach-session`.
    let fake_watch = root.join("fake-tmux-watch.sh");
    write_executable(
        &fake_watch,
        &format!(
            r#"#!/bin/sh
IN_LOG="{in_log}"
EMIT_FILE="{emit_file}"
DIGEST_FILE="{digest_file}"

# 1. Control-mode attach handshake (`%begin/%end`).
printf '%%begin 1000 1 0\n%%end 1000 1 0\n'

# Background trigger forwarder emitting async `%output` / `%pause` lines on demand.
(
  while true; do
    if [ -s "$EMIT_FILE" ]; then
      mv "$EMIT_FILE" "$EMIT_FILE.sending" 2>/dev/null || {{ sleep 0.05; continue; }}
      cat "$EMIT_FILE.sending"
      rm -f "$EMIT_FILE.sending"
    fi
    sleep 0.05
  done
) &
EMIT_PID=$!
trap 'kill -TERM "$EMIT_PID" 2>/dev/null || true; exit 0' INT TERM HUP EXIT

# 2. Read control commands from `SshTmuxControlWatcher` stdin.
CMD_N=2
while IFS= read -r _cmd; do
  printf '%s\n' "$_cmd" >> "$IN_LOG"
  case "$_cmd" in
    refresh-client\ -f*)
      printf '%%begin 1001 %s 0\n%%end 1001 %s 0\n' "$CMD_N" "$CMD_N"
      CMD_N=$((CMD_N + 1))
      ;;
    list-panes\ -a*)
      printf '%%begin 1002 %s 0\n' "$CMD_N"
      cat "$DIGEST_FILE" 2>/dev/null || true
      printf '%%end 1002 %s 0\n' "$CMD_N"
      CMD_N=$((CMD_N + 1))
      ;;
    refresh-client\ -A*)
      printf '%%begin 1003 %s 0\n%%end 1003 %s 0\n' "$CMD_N" "$CMD_N"
      CMD_N=$((CMD_N + 1))
      ;;
  esac
done
"#,
            in_log = watch_in_log.display(),
            emit_file = watch_emit_file.display(),
            digest_file = digest_file.display(),
        ),
    );

    // `fake-ssh` routing `-C attach-session` to `fake_watch` and `/bin/sh -s` to real conduit while
    // classifying every `EXEC` payload (`FULL_V3` vs `DIRTY_SYNC`) via decoded base64!
    let fake_ssh = root.join("fake-ssh");
    write_executable(
        &fake_ssh,
        &format!(
            r#"#!/bin/sh
KIND_LOG="{kind_log}"
WATCH_SH="{watch_sh}"
export HOME="{rhome}"
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then exit 0; fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
case "$1" in
  *"-C attach-session"*)
    exec "$WATCH_SH"
    ;;
esac
if [ "$1" = "/bin/sh -s" ]; then
  FIFO="{root}/c.$$.fifo"
  rm -f "$FIFO"
  mkfifo "$FIFO"
  /bin/sh -s <"$FIFO" &
  SH_PID=$!
  exec 8>"$FIFO"
  rm -f "$FIFO"
  trap 'exec 8>&- 2>/dev/null || true; kill -TERM "$SH_PID" 2>/dev/null || true; exit 0' INT TERM HUP
  B64_DEC="base64 -d"
  if [ "$(printf 'b2s=' | base64 -d 2>/dev/null)" != "ok" ]; then B64_DEC="base64 -D"; fi
  FIRST=1
  while IFS= read -r _line; do
    if [ "$FIRST" -eq 1 ]; then
      FIRST=0
      printf '%s\n' "$_line" >&8 || break
      continue
    fi
    _op=${{_line%% *}}
    if [ "$_op" = "EXEC" ]; then
      _rest=${{_line#* }}
      _rest=${{_rest#* }}
      _sb64=${{_rest%% *}}
      _dec=$(printf '%s\n' "$_sb64" | $B64_DEC 2>/dev/null || true)
      case "$_dec" in
        *__SF_DIRTY_META__*) printf 'DIRTY_SYNC\n' >> "$KIND_LOG" ;;
        *__SF_SEC_PANES__*) printf 'FULL_V3\n' >> "$KIND_LOG" ;;
        *) printf 'OTHER_EXEC\n' >> "$KIND_LOG" ;;
      esac
    fi
    printf '%s\n' "$_line" >&8 || break
  done
  exec 8>&- 2>/dev/null || true
  wait "$SH_PID"
  exit $?
fi
exec /bin/sh -c "$1"
"#,
            kind_log = exec_kind_log.display(),
            watch_sh = fake_watch.display(),
            rhome = remote_home.display(),
            root = root.display(),
        ),
    );

    let node_cfg = NodeConfig {
        id: "host-push".into(),
        display_name: Some("Host Push".into()),
        kind: NodeKindConfig::Ssh,
        enabled: true,
        include_shell_panes: false,
        push_watch: true,
        ssh_target: "dev@host-push.example".into(),
        ssh_bin: fake_ssh.to_string_lossy().into_owned(),
        tmux_bin: sb.real_tmux.clone(),
        tmux_socket: sb.socket.clone(),
        connect_timeout_secs: 5,
        compression: true,
        ..NodeConfig::default()
    };
    let (core, addr, srv) = start_hub_with_web(
        &root.join("hub-state"),
        vec![node_cfg],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    let mut sse = SseClient::connect(addr, None).await;
    // Start real Core per-host remote supervisor (performs initial sync + spawns `SshTmuxControlWatcher`!).
    core.spawn_remote_host_supervisors();

    let sid_a = format!("host-push:tmux-{}", pane_a.as_str());
    let sid_b = format!("host-push:tmux-{}", pane_b.as_str());

    // Wait for initial supervisor sync + control watcher handshake (`refresh-client -f pause-after=2`).
    let wait_ready_dl = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = core.snapshot();
        let has_both = snap.sessions.iter().any(|s| s.session_id.as_str() == sid_a)
            && snap.sessions.iter().any(|s| s.session_id.as_str() == sid_b);
        let in_log = std::fs::read_to_string(&watch_in_log).unwrap_or_default();
        if has_both && in_log.contains("refresh-client -f pause-after=2") {
            break;
        }
        assert!(
            Instant::now() < wait_ready_dl,
            "supervisor/watcher did not initialize in time; has_both={has_both}, in_log={in_log:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Phase 1: Append new turn to `pane_a`'s remote transcript and emit `%output` + `%pause`
    // from `fake-tmux-C` -> verify targeted `sync_dirty_panes`, flow-control `continue`, SSE `output`,
    // and sibling `pane_b` preservation!
    let kinds_before_p1 = std::fs::read_to_string(&exec_kind_log).unwrap_or_default();
    let full_before_p1 = kinds_before_p1.lines().filter(|l| *l == "FULL_V3").count();
    let dirty_before_p1 = kinds_before_p1
        .lines()
        .filter(|l| *l == "DIRTY_SYNC")
        .count();

    append_claude_turn(&tr_a, 2, "push watcher q1", "push-watcher-pane-a-delta");
    std::fs::write(
        &watch_emit_file,
        format!(
            "%output {} streaming-token\\015\\012\n%pause {}\n",
            pane_a.as_str(),
            pane_a.as_str()
        ),
    )
    .unwrap();

    let ev_a = sse
        .wait_for(Duration::from_secs(2), |f| {
            f.event == "output"
                && f.data["session_id"].as_str() == Some(sid_a.as_str())
                && f.data["appended_messages"]
                    .as_array()
                    .is_some_and(|arr| !arr.is_empty())
        })
        .await
        .expect("expected Hub SSE `event: output` for pane_a triggered by `%output` + `%pause`");
    assert_eq!(
        ev_a.data["appended_messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["text"],
        json!("push-watcher-pane-a-delta")
    );

    let kinds_after_p1 = std::fs::read_to_string(&exec_kind_log).unwrap_or_default();
    let full_after_p1 = kinds_after_p1.lines().filter(|l| *l == "FULL_V3").count();
    let dirty_after_p1 = kinds_after_p1
        .lines()
        .filter(|l| *l == "DIRTY_SYNC")
        .count();
    assert!(
        dirty_after_p1 > dirty_before_p1,
        "expected targeted DIRTY_SYNC on `%output` push (kinds:\n{kinds_after_p1})"
    );
    assert_eq!(
        full_after_p1, full_before_p1,
        "targeted `%output` push must NOT trigger full `FULL_V3` discovery!"
    );

    // Verify watcher issued `refresh-client -A <pane_a>:continue` after `%pause`.
    let cont_dl = Instant::now() + Duration::from_secs(1);
    let expected_cont = format!("refresh-client -A {}:continue", pane_a.as_str());
    loop {
        let in_log = std::fs::read_to_string(&watch_in_log).unwrap_or_default();
        if in_log.contains(&expected_cont) {
            break;
        }
        assert!(
            Instant::now() < cont_dl,
            "expected watcher to resume paused pane with `{expected_cont}`, got:\n{in_log}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Verify non-dirty sibling `pane_b` was NOT reaped by partial dirty sync!
    let snap_after_p1 = core.snapshot();
    let sibling_b = snap_after_p1
        .sessions
        .iter()
        .find(|s| s.session_id.as_str() == sid_b)
        .expect("sibling pane_b must survive targeted partial dirty sync");
    assert_ne!(sibling_b.state, SessionState::Ended);

    // Phase 2: Multi-session `list-panes -a` digest diff on unattached sibling `pane_b`!
    // Wait until watcher completes its baseline `list-panes -a` digest poll (<= 3.5s idle cadence),
    // then bump `pane_b`'s history_size in `digest_state.txt` + append a new turn to `tr_b`.
    let base_digest_dl = Instant::now() + Duration::from_secs(4);
    loop {
        let in_log = std::fs::read_to_string(&watch_in_log).unwrap_or_default();
        if in_log.contains("list-panes -a") {
            break;
        }
        assert!(
            Instant::now() < base_digest_dl,
            "expected watcher to send baseline `list-panes -a` digest, got:\n{in_log}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    append_claude_turn(&tr_b, 2, "digest diff q1", "push-digest-pane-b-delta");
    std::fs::write(
        &digest_file,
        format!(
            "{}|10|2|5|0|0|claude\n{}|14|2|6|0|0|claude\n",
            pane_a.as_str(),
            pane_b.as_str()
        ),
    )
    .unwrap();

    let ev_b = sse
        .wait_for(Duration::from_secs(5), |f| {
            f.event == "output"
                && f.data["session_id"].as_str() == Some(sid_b.as_str())
                && f.data["appended_messages"]
                    .as_array()
                    .is_some_and(|arr| !arr.is_empty())
        })
        .await
        .expect(
            "expected Hub SSE `event: output` for pane_b triggered by multi-session digest diff",
        );
    assert_eq!(
        ev_b.data["appended_messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["text"],
        json!("push-digest-pane-b-delta")
    );
    // Both siblings remain intact!
    let snap_final = core.snapshot();
    assert!(snap_final
        .sessions
        .iter()
        .any(|s| s.session_id.as_str() == sid_a && s.state != SessionState::Ended));
    assert!(snap_final
        .sessions
        .iter()
        .any(|s| s.session_id.as_str() == sid_b && s.state != SessionState::Ended));

    core.request_shutdown();
    srv.abort();
}

// ===========================================================================
// Test 4: Peer v2 SSE Push + `Last-Event-Id` Reconnect + HTTP/1.1 Keep-Alive
//         Pool + `SshLoopbackTunnel` Fake-SSH Bootstrap/Forward/Cancel
// ===========================================================================
#[tokio::test]
async fn v2_peer_sse_push_last_event_id_keepalive_and_ssh_loopback_tunnel() {
    let (root, _guard) = mk_temp_root("peer-v2");
    let peer_dir = root.join("peer");
    let hub_dir = root.join("hub");
    std::fs::create_dir_all(&peer_dir).unwrap();
    std::fs::create_dir_all(&hub_dir).unwrap();

    // 1. Start real in-process Peer `Core` + Axum HTTP server.
    let peer_tr = peer_dir.join("peer-s1.jsonl");
    std::fs::write(
        &peer_tr,
        "{\"type\":\"user\",\"timestamp\":\"2026-11-26T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"peer init q\"}}\n\
         {\"type\":\"assistant\",\"timestamp\":\"2026-11-26T00:00:02.000Z\",\"message\":{\"id\":\"pm0\",\"model\":\"claude-sonnet-4-6\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"peer init a\"}]}}\n",
    )
    .unwrap();

    let (peer_core, peer_addr, peer_srv) = start_hub_with_web(
        &peer_dir.join("state"),
        Vec::new(),
        WebConfig {
            auth_enabled: true,
            scoped_tokens: vec![ScopedTokenConfig {
                label: "hub".into(),
                token: "peer-v2-secret".into(),
                token_env: String::new(),
                read_only: false,
            }],
            ..WebConfig::default()
        },
    )
    .await;

    peer_core
        .ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Claude,
            hook_event: Some("Stop".into()),
            payload: json!({
                "session_id": "peer-s1",
                "hook_event_name": "Stop",
                "cwd": peer_dir.to_string_lossy(),
                "transcript_path": peer_tr.to_string_lossy(),
                "last_assistant_message": "peer init a"
            }),
            env: EnvCtx {
                tmux_pane: Some("%7".into()),
                tmux: Some("/tmp/tmux,1,0".into()),
                ..EnvCtx::default()
            },
            recv_ts_ms: id::now_ms(),
        })
        .await;

    // 2. Start Hub `Core` connected to Peer with `push_watch = true` + Hub HTTP `/api/events` SSE.
    let peer_url = format!("http://{peer_addr}");
    let (hub_core, hub_addr, hub_srv) = start_hub_with_web(
        &hub_dir.join("state"),
        vec![NodeConfig {
            id: "peer-v2".into(),
            display_name: Some("Peer V2".into()),
            kind: NodeKindConfig::Peer,
            enabled: true,
            push_watch: true,
            endpoints: vec![peer_url.clone()],
            token: "peer-v2-secret".into(),
            ..NodeConfig::default()
        }],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    let mut hub_sse = SseClient::connect(hub_addr, None).await;
    hub_core.spawn_remote_host_supervisors();

    // Wait for Hub initial sync + Peer SSE subscriber connection.
    let ready_dl = Instant::now() + Duration::from_secs(4);
    loop {
        if hub_core
            .snapshot()
            .sessions
            .iter()
            .any(|s| s.session_id.as_str() == "peer-v2:peer-s1")
        {
            break;
        }
        assert!(
            Instant::now() < ready_dl,
            "hub failed to sync initial peer session"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    // Give `PeerSseSubscriber` a brief beat to open `/api/events` on Peer.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Append new turn on Peer and ingest a hook event -> Peer emits `DaemonEvent::Session` +
    // `DaemonEvent::Output` -> streams in real time (<2s, vs 30s heartbeat) to Hub `/api/events` SSE!
    append_claude_turn(&peer_tr, 2, "peer push q2", "peer-sse-pushed-answer");
    peer_core
        .ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Claude,
            hook_event: Some("Stop".into()),
            payload: json!({
                "session_id": "peer-s1",
                "hook_event_name": "Stop",
                "cwd": peer_dir.to_string_lossy(),
                "transcript_path": peer_tr.to_string_lossy(),
                "last_assistant_message": "peer-sse-pushed-answer"
            }),
            env: EnvCtx {
                tmux_pane: Some("%7".into()),
                tmux: Some("/tmp/tmux,1,0".into()),
                ..EnvCtx::default()
            },
            recv_ts_ms: id::now_ms(),
        })
        .await;

    let mut saw_peer_session = false;
    let mut saw_peer_output = false;
    let sse_dl = Instant::now() + Duration::from_secs(3);
    while !(saw_peer_session && saw_peer_output) && Instant::now() < sse_dl {
        let rem = sse_dl.saturating_duration_since(Instant::now());
        let Some(frame) = hub_sse.next_event(rem).await else {
            break;
        };
        if frame.event == "session"
            && frame.data["session"]["session_id"].as_str() == Some("peer-v2:peer-s1")
            && frame.data["session"]["last_message"].as_str() == Some("peer-sse-pushed-answer")
        {
            saw_peer_session = true;
        }
        if frame.event == "output" && frame.data["session_id"].as_str() == Some("peer-v2:peer-s1") {
            let texts: Vec<&str> = frame.data["appended_messages"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["text"].as_str()).collect())
                .unwrap_or_default();
            if texts.contains(&"peer-sse-pushed-answer") || frame.data["dirty"] == json!(true) {
                saw_peer_output = true;
            }
        }
    }
    assert!(
        saw_peer_session && saw_peer_output,
        "expected real-time Peer SSE push for both session ({saw_peer_session}) and output ({saw_peer_output})"
    );

    let peer_host_rec = hub_core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "peer-v2")
        .unwrap();
    assert_eq!(peer_host_rec.status, HostStatus::Online);
    assert!(peer_host_rec.rtt_ms.is_some());
    assert!(peer_host_rec.sync_ms.is_some());

    // 3. Verify `PeerKeepAlivePool` reuses warm Keep-Alive TCP socket & records pure RTT.
    let ka_pool = PeerKeepAlivePool::new();
    let rtt1 = ka_pool
        .probe_rtt(&peer_url, Some("peer-v2-secret"))
        .await
        .unwrap();
    assert!(rtt1 < 500);
    assert!(
        ka_pool.idle_count() >= 1,
        "PeerKeepAlivePool must keep idle HTTP/1.1 Keep-Alive socket warm"
    );
    let state_v = ka_pool
        .request_json(
            "GET",
            &format!("{peer_url}/api/state"),
            Some("peer-v2-secret"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(state_v["ok"], json!(true));
    assert!(ka_pool.rtt_ms().is_some());

    // 4. Verify `PeerSseSubscriber` `Last-Event-Id` reconnect over real TCP drop!
    let mock_sse_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_sse_addr = mock_sse_listener.local_addr().unwrap();
    let seen_last_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen_clone = seen_last_id.clone();
    let mock_sse_srv = tokio::spawn(async move {
        for attempt in 0..2u8 {
            let Ok((mut sock, _)) = mock_sse_listener.accept().await else {
                break;
            };
            let mut req_buf = vec![0u8; 2048];
            let n = sock.read(&mut req_buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&req_buf[..n]);
            if attempt == 0 {
                let ev1 = serde_json::to_string(&DaemonEvent::Output {
                    seq: 41,
                    session_id: SessionId::new("peer-s1"),
                    start_seq: Some(0),
                    total_messages: Some(1),
                    appended_messages: vec![json!({"role":"assistant","text":"before-drop"})],
                    live_tail: None,
                    metrics: None,
                    dirty: false,
                })
                .unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nid: 41\nevent: output\ndata: {ev1}\n\n"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            } else {
                let lid = req_str.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("last-event-id")
                        .then(|| v.trim().to_string())
                });
                *seen_clone.lock().unwrap() = lid;
                let ev2 = serde_json::to_string(&DaemonEvent::Output {
                    seq: 42,
                    session_id: SessionId::new("peer-s1"),
                    start_seq: Some(1),
                    total_messages: Some(2),
                    appended_messages: vec![json!({"role":"assistant","text":"after-reconnect"})],
                    live_tail: None,
                    metrics: None,
                    dirty: false,
                })
                .unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\nid: 42\nevent: output\ndata: {ev2}\n\n"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });

    let (sig_tx, mut sig_rx) = tokio::sync::mpsc::unbounded_channel();
    let sub = PeerSseSubscriber::with_endpoints(vec![PeerSseEndpoint::new(
        format!("http://{mock_sse_addr}"),
        None,
    )])
    .with_backoff_schedule(vec![Duration::from_millis(50)]);
    let sub_handle = tokio::spawn(async move {
        sub.run_signals(sig_tx).await;
    });

    let mut received_seqs = Vec::new();
    let mut saw_reconnect = false;
    let sub_dl = Instant::now() + Duration::from_secs(3);
    while received_seqs.len() < 2 && Instant::now() < sub_dl {
        let rem = sub_dl.saturating_duration_since(Instant::now());
        if let Ok(Some(sig)) = tokio::time::timeout(rem, sig_rx.recv()).await {
            match sig {
                PeerSseSignal::Connected {
                    reconnected: true, ..
                } => {
                    saw_reconnect = true;
                }
                PeerSseSignal::Frame(frame) => {
                    if let Ok(DaemonEvent::Output { seq, .. }) = frame.parse_daemon_event() {
                        received_seqs.push(seq);
                    }
                }
                _ => {}
            }
        }
    }
    sub_handle.abort();
    mock_sse_srv.abort();
    assert_eq!(received_seqs, vec![41, 42]);
    assert!(
        saw_reconnect,
        "expected Connected {{ reconnected: true }} signal"
    );
    assert_eq!(
        seen_last_id.lock().unwrap().as_deref(),
        Some("41"),
        "PeerSseSubscriber must send `Last-Event-Id: 41` on reconnect"
    );

    // 5. Verify `SshLoopbackTunnel` fake-ssh bootstrap + dynamic `-O forward` + `-O cancel`
    //    AND end-to-end `PeerDaemonNodeProvider` (`endpoints = []`, `ssh_target = ...`) sync!
    let tun_home = root.join("tun_remote_home");
    let tun_cfg_dir = tun_home.join(".config/starfane");
    let tun_state_dir = tun_home.join(".local/state/starfane");
    let tun_bin_dir = tun_home.join(".local/bin");
    std::fs::create_dir_all(&tun_cfg_dir).unwrap();
    std::fs::create_dir_all(&tun_state_dir).unwrap();
    std::fs::create_dir_all(&tun_bin_dir).unwrap();

    // Seed remote config pointing to our live in-process Peer port & auth_enabled = true.
    std::fs::write(
        tun_cfg_dir.join("config.toml"),
        format!("[web]\nport = {}\nauth_enabled = true\n", peer_addr.port()),
    )
    .unwrap();
    // Seed remote bearer token + fake `starfane` binary that creates pid/sock when bootstrapped.
    std::fs::write(tun_state_dir.join("web.token"), "peer-v2-secret\n").unwrap();
    let fake_remote_sf = tun_bin_dir.join("starfane");
    write_executable(
        &fake_remote_sf,
        &format!(
            r#"#!/bin/sh
# Simulate `starfane daemon --detach` marking remote daemon running.
printf '%s\n' "$$" > "{sdir}/starfaned.pid"
exit 0
"#,
            sdir = tun_state_dir.display(),
        ),
    );

    let tun_ssh_log = root.join("tun_ssh.log");
    let tun_fake_ssh = root.join("tun-fake-ssh");
    write_executable(
        &tun_fake_ssh,
        &format!(
            r#"#!/bin/sh
LOG="{log}"
export HOME="{rhome}"
export XDG_CONFIG_HOME="{rhome}/.config"
export STARFANE_STATE_DIR="{sdir}"
if [ "$1" = "-O" ] && [ "$2" = "forward" ]; then
  printf 'FORWARD|%s\n' "$*" >> "$LOG"
  # Print the live in-process Peer loopback port as the dynamically allocated local port!
  printf '{peer_port}\n'
  exit 0
fi
if [ "$1" = "-O" ] && [ "$2" = "cancel" ]; then
  printf 'CANCEL|%s\n' "$*" >> "$LOG"
  exit 0
fi
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  printf 'EXIT|%s\n' "$*" >> "$LOG"
  exit 0
fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
printf 'BOOT|%s\n' "$*" >> "$LOG"
exec /bin/sh -c "$1"
"#,
            log = tun_ssh_log.display(),
            rhome = tun_home.display(),
            sdir = tun_state_dir.display(),
            peer_port = peer_addr.port(),
        ),
    );

    // 5a. Direct `SshLoopbackTunnel` bootstrap -> forward -> teardown check.
    let tun_ssh = SshExec::new("dev@tun-peer.example", root.join("cm-tun.sock"))
        .with_ssh_bin(Some(tun_fake_ssh.to_string_lossy().into_owned()));
    let tunnel = SshLoopbackTunnel::new(tun_ssh);
    let ep = tunnel.ensure_endpoint().await.unwrap();
    assert_eq!(ep.remote_port, peer_addr.port());
    assert_eq!(ep.local_port, peer_addr.port());
    assert_eq!(
        ep.local_endpoint,
        format!("http://127.0.0.1:{}", peer_addr.port())
    );
    assert_eq!(ep.host_header, format!("127.0.0.1:{}", peer_addr.port()));
    assert_eq!(ep.token.as_deref(), Some("peer-v2-secret"));
    assert_eq!(ep.remote_bin, fake_remote_sf.to_string_lossy());
    tunnel.teardown().await;

    let tun_log_text = std::fs::read_to_string(&tun_ssh_log).unwrap_or_default();
    assert!(tun_log_text.contains("BOOT|"), "{tun_log_text}");
    assert!(tun_log_text.contains("FORWARD|"), "{tun_log_text}");
    assert!(tun_log_text.contains("CANCEL|"), "{tun_log_text}");

    // 5b. Full `PeerDaemonNodeProvider` with `endpoints = []` & `ssh_target` auto-tunnel!
    let ssh_peer_cfg = NodeConfig {
        id: "tun-peer".into(),
        display_name: Some("Tunneled Peer".into()),
        kind: NodeKindConfig::Peer,
        enabled: true,
        push_watch: true,
        endpoints: Vec::new(),
        ssh_target: "dev@tun-peer.example".into(),
        ssh_bin: tun_fake_ssh.to_string_lossy().into_owned(),
        ..NodeConfig::default()
    };
    ssh_peer_cfg
        .validate()
        .expect("Peer node with ssh_target and empty endpoints must validate cleanly");
    let tun_provider = PeerDaemonNodeProvider::with_state_dir(&ssh_peer_cfg, &root.join("tun-st"));
    let tun_snap = tun_provider
        .sync_with_claimed("Tunneled Peer", &HashSet::new())
        .await
        .expect("PeerDaemonNodeProvider must sync live Peer via SshLoopbackTunnel");
    assert_eq!(tun_snap.host.status, HostStatus::Online);
    assert!(tun_snap.host.rtt_ms.is_some());
    assert!(tun_snap.host.sync_ms.is_some());
    assert!(
        tun_snap
            .sessions
            .iter()
            .any(|s| s.session_id.as_str() == "tun-peer:peer-s1"),
        "expected tun-peer:peer-s1 via SSH loopback tunnel: {:?}",
        tun_snap.sessions
    );
    drop(tun_provider);

    hub_core.request_shutdown();
    peer_core.request_shutdown();
    hub_srv.abort();
    peer_srv.abort();
}
