//! End-to-end `fake-ssh` verification suite for zero-install SSH remote host (`kind = "ssh"`)
//! feature parity & WAN latency resilience (Blueprint §6 — all 10 scenarios).

use serde_json::{json, Value};
use starfane_daemon::config::{NodeConfig, NodeKindConfig, ScopedTokenConfig, WebConfig};
use starfane_daemon::core::TranscriptQuery;
use starfane_daemon::mcp::{CallError, CallerEnv, McpServer, Transport};
use starfane_daemon::remote_mirror::RemoteTranscriptMirror;
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use starfane_tmux::{SshExec, Tmux};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

const SAMPLE_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn has_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn has_sqlite3() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn real_tmux_bin() -> String {
    let out = Command::new("/bin/sh")
        .args(["-c", "command -v tmux"])
        .output()
        .expect("locate tmux");
    assert!(out.status.success(), "tmux must be installed on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn find_system_tool(name: &str) -> Option<PathBuf> {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
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

fn run_sqlite(db: &Path, sql: &str) {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let out = Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .expect("run sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// Process-wide shared executable fixtures (`fake-ssh`, default mock agent binaries, and
/// POSIX-only `no_sqlite_bin`), created & warmed ONCE per test binary so parallel tests never
/// queue dozens of freshly-written scripts through macOS Gatekeeper / `syspolicyd`.
struct SharedTestEnv {
    reg_dir: PathBuf,
    shared_bin: PathBuf,
    no_sqlite_bin: PathBuf,
    fake_ssh: PathBuf,
    real_tmux: String,
}

fn shared_env() -> &'static SharedTestEnv {
    static ENV: OnceLock<SharedTestEnv> = OnceLock::new();
    ENV.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("sf-ssh-shared-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let reg_dir = base.join("reg");
        let shared_bin = base.join("bin");
        let no_sqlite_bin = base.join("no_sqlite_bin");
        std::fs::create_dir_all(&reg_dir).unwrap();
        std::fs::create_dir_all(&shared_bin).unwrap();
        std::fs::create_dir_all(&no_sqlite_bin).unwrap();

        let real_tmux = real_tmux_bin();

        // Populate `no_sqlite_bin` with symlinks to POSIX utilities (strictly excluding `sqlite3`).
        #[cfg(unix)]
        for tool in [
            "sh", "bash", "ps", "stat", "date", "head", "tail", "cut", "tr", "sed", "awk",
            "sort", "ls", "wc", "mkdir", "chmod", "mv", "rm", "sleep", "cat", "gzip", "lsof",
            "readlink", "env", "printf", "touch", "grep",
        ] {
            if let Some(src) = find_system_tool(tool) {
                let dst = no_sqlite_bin.join(tool);
                let _ = std::os::unix::fs::symlink(&src, &dst);
            }
        }

        // Shared fake interactive agent binaries whose argv[0] is the real agent name
        // (`claude`, `codex`, `agy`). Note: trailing `; true` prevents Bash's tail-call
        // `exec sleep` optimization from clobbering argv[0] in `ps`.
        for agent in ["claude", "codex", "agy"] {
            let bin = shared_bin.join(agent);
            write_executable(
                &bin,
                &format!(
                    "#!/bin/bash\n[ \"$1\" = \"--sf-warmup\" ] && exit 0\nexec -a {agent} /bin/bash -c 'printf \"{agent} tui ready\\n\"; sleep 120; true' {agent} \"$@\"\n"
                ),
            );
            let _ = Command::new(&bin).arg("--sf-warmup").output();
        }

        // Single shared `fake-ssh` dispatcher parameterized per-target via `$REG_DIR/<target>.env`.
        let fake_ssh = base.join("fake-ssh");
        let script = format!(
            r#"#!/bin/sh
REG_DIR="{reg_dir}"

if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  eval "TARGET=\${{$#}}"
  CFG="$REG_DIR/$TARGET.env"
  if [ -f "$CFG" ]; then
    . "$CFG"
    printf 'EXIT|%s\n' "$*" >> "$LOG"
  fi
  exit 0
fi

TARGET=""
PREV=""
ORIG_ARGS="$*"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then
    TARGET="$PREV"
    shift
    break
  fi
  PREV="$1"
  shift
done

CFG="$REG_DIR/$TARGET.env"
if [ ! -f "$CFG" ]; then
  printf 'fake-ssh: unknown target %s\n' "$TARGET" >&2
  exit 255
fi
. "$CFG"

printf 'CMD|%s\n' "$ORIG_ARGS" >> "$LOG"

MODE="normal"
if [ -f "$MODE_FILE" ]; then
  MODE=$(head -n 1 "$MODE_FILE" | tr -d '\r\n')
fi

case "$MODE" in
  exit255_once)
    printf 'normal\n' > "$MODE_FILE"
    printf 'ControlSocket /tmp/stale.sock already exists, disabling multiplexing\nmux_client_request_session: session request failed: Broken pipe\n' >&2
    exit 255
    ;;
  exit255_always)
    printf 'ssh: connect to host dead.example: Connection refused\n' >&2
    exit 255
    ;;
  hang)
    sleep 30
    exit 255
    ;;
  delay_150ms)
    sleep 0.15
    ;;
esac

export HOME="$RHOME"
if [ "$MODE" = "strip_sqlite3" ]; then
  export PATH="$RBIN:$NOSQL_BIN"
else
  export PATH="$RBIN:$PATH"
fi

exec /bin/sh -c "$1"
"#,
            reg_dir = reg_dir.display(),
        );
        write_executable(&fake_ssh, &script);

        // Warm `fake-ssh` once so OS script verification happens upfront, once.
        let _ = Command::new(&fake_ssh)
            .args(["-O", "exit", "__warmup__"])
            .output();

        SharedTestEnv {
            reg_dir,
            shared_bin,
            no_sqlite_bin,
            fake_ssh,
            real_tmux,
        }
    })
}

/// Isolated `fake-ssh` remote host backed by a real sandbox `tmux -L <unique_sock>` server,
/// isolated `$REMOTE_HOME`, call log for strict RTT assertions, and fault-injection mode file.
struct FakeSshHost {
    root: PathBuf,
    remote_home: PathBuf,
    fake_ssh: PathBuf,
    ssh_target: String,
    env_cfg: PathBuf,
    calls_log: PathBuf,
    mode_file: PathBuf,
    tmux_socket: String,
    tmux: Tmux,
}

impl FakeSshHost {
    fn new(label: &str) -> Self {
        let env = shared_env();
        let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("sf-ssh-{label}-{}-{seq}", std::process::id()));
        let remote_home = root.join("remote_home");
        std::fs::create_dir_all(&remote_home).unwrap();
        // Canonicalize `root` & `remote_home` so macOS `/var -> /private/var` symlinks
        // never diverge from `tmux list-panes -F '#{pane_current_path}'`.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let remote_home = root.join("remote_home");

        let tmux_socket = format!("sf-ssh-{label}-{}-{seq}", std::process::id());
        let ssh_target = format!("ubuntu@{label}-{seq}.example");

        let calls_log = root.join("ssh_calls.log");
        let mode_file = root.join("fake_ssh_mode");
        std::fs::write(&mode_file, "normal\n").unwrap();

        let env_cfg = env.reg_dir.join(format!("{ssh_target}.env"));
        std::fs::write(
            &env_cfg,
            format!(
                "LOG='{}'\nMODE_FILE='{}'\nRHOME='{}'\nRBIN='{}'\nNOSQL_BIN='{}'\n",
                calls_log.display(),
                mode_file.display(),
                remote_home.display(),
                env.shared_bin.display(),
                env.no_sqlite_bin.display(),
            ),
        )
        .unwrap();

        let tmux = Tmux::new(PathBuf::from(&env.real_tmux), Some(tmux_socket.clone()));
        Self {
            root,
            remote_home,
            fake_ssh: env.fake_ssh.clone(),
            ssh_target,
            env_cfg,
            calls_log,
            mode_file,
            tmux_socket,
            tmux,
        }
    }

    fn agent_bin(&self, agent: &str) -> String {
        shared_env()
            .shared_bin
            .join(agent)
            .to_string_lossy()
            .into_owned()
    }

    fn set_mode(&self, mode: &str) {
        std::fs::write(&self.mode_file, format!("{mode}\n")).unwrap();
    }

    fn call_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.calls_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn cmd_call_count(&self) -> usize {
        self.call_lines()
            .iter()
            .filter(|l| l.starts_with("CMD|"))
            .count()
    }

    fn exit_call_count(&self) -> usize {
        self.call_lines()
            .iter()
            .filter(|l| l.starts_with("EXIT|"))
            .count()
    }

    fn node_config(&self, id: &str, display_name: &str) -> NodeConfig {
        NodeConfig {
            id: id.into(),
            display_name: Some(display_name.into()),
            kind: NodeKindConfig::Ssh,
            enabled: true,
            ssh_target: self.ssh_target.clone(),
            ssh_bin: self.fake_ssh.to_string_lossy().into_owned(),
            tmux_bin: shared_env().real_tmux.clone(),
            tmux_socket: self.tmux_socket.clone(),
            connect_timeout_secs: 5,
            compression: true,
            ..NodeConfig::default()
        }
    }
}

impl Drop for FakeSshHost {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.tmux_socket, "kill-server"])
            .output();
        let _ = std::fs::remove_file(&self.env_cfg);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mk_hub_dir(label: &str) -> (PathBuf, TempDirGuard) {
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sf-hub-{label}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.clone(), TempDirGuard(dir))
}

fn build_hub_core(state_dir: &Path, nodes: Vec<NodeConfig>, web: WebConfig) -> Core {
    let mut cfg = StarfaneConfig {
        web,
        nodes,
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;
    Core::new(&DaemonConfig {
        state_dir: state_dir.to_path_buf(),
        socket: state_dir.join("starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap()
}

async fn start_hub_with_web(
    state_dir: &Path,
    nodes: Vec<NodeConfig>,
    mut web: WebConfig,
) -> (Core, SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    web.port = addr.port();
    let core = build_hub_core(state_dir, nodes, web);
    let app = starfane_daemon::web::router(core.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (core, addr, handle)
}

async fn http_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body_str = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port(),
        addr.port(),
    );
    if let Some(tok) = token {
        req.push_str(&format!("X-Starfane-Token: {tok}\r\n"));
    }
    if body.is_some() {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body_str.len()
        ));
    }
    req.push_str("\r\n");
    if body.is_some() {
        req.push_str(&body_str);
    }
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    let raw = String::from_utf8_lossy(&resp);
    let (head, body_raw) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let parsed: Value =
        serde_json::from_str(body_raw.trim()).unwrap_or_else(|_| json!({ "raw": body_raw.trim() }));
    (status, parsed)
}

fn extract_control_path_arg(cmd_line: &str) -> Option<String> {
    cmd_line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("ControlPath=").map(str::to_string))
}

// ===========================================================================
// Test 1: ControlMaster args (`sun_path <= 92`, `Compression=yes`), zombie
// `ssh -O exit` cleanup, 1-shot `exit255_once` recovery on `sync` vs ZERO
// automatic retry on `dispatch_interactive`.
// ===========================================================================
#[tokio::test]
async fn test_01_controlmaster_args_stale_socket_recovery_and_no_dispatch_retry() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t1");
    let (hub_dir, _hub_guard) = mk_hub_dir("t1");
    // Deliberately deep state dir (> 110 bytes) to prove `ControlPath` auto-hashes to <= 92 bytes!
    let deep_state = hub_dir.join(
        "very-long-nested-directory-segment-one/very-long-nested-directory-segment-two/state-dir",
    );
    std::fs::create_dir_all(&deep_state).unwrap();

    // Seed one live remote pane in the sandbox tmux server.
    let _pane = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("work".into()),
            create_session: true,
            window_name: "claude-1".into(),
            cwd: remote.remote_home.clone(),
            env: Vec::new(),
            argv: vec![remote.agent_bin("claude")],
        })
        .unwrap();

    let core = build_hub_core(
        &deep_state,
        vec![remote.node_config("gpu-cm", "GPU CM")],
        WebConfig::default(),
    );

    // 1. Normal sync: inspect exact OpenSSH ControlMaster flags & `sun_path <= 92`.
    core.sync_remote_host(&HostId::new("gpu-cm")).await;
    let lines = remote.call_lines();
    let first_cmd = lines
        .iter()
        .find(|l| l.starts_with("CMD|"))
        .expect("expected SSH CMD invocation");
    assert!(first_cmd.contains("ControlMaster=auto"), "{first_cmd}");
    assert!(first_cmd.contains("ControlPersist=600"), "{first_cmd}");
    assert!(first_cmd.contains("Compression=yes"), "{first_cmd}");
    assert!(first_cmd.contains("BatchMode=yes"), "{first_cmd}");
    assert!(first_cmd.contains("ConnectTimeout=5"), "{first_cmd}");
    let cp = extract_control_path_arg(first_cmd).expect("ControlPath arg");
    assert!(
        cp.len() <= 92,
        "ControlPath must stay <= 92 bytes for macOS sun_path, got {} bytes: {cp}",
        cp.len()
    );

    let host_rec = core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "gpu-cm")
        .unwrap();
    assert_eq!(host_rec.status, HostStatus::Online);
    assert!(host_rec.rtt_ms.is_some());

    // 2. Stale ControlMaster socket (`exit255_once`) on idempotent `sync`:
    //    MUST clean up via `ssh -O exit` and transparently retry once -> succeeds!
    let cmds_before = remote.cmd_call_count();
    let exits_before = remote.exit_call_count();
    remote.set_mode("exit255_once");
    core.sync_remote_host(&HostId::new("gpu-cm")).await;
    assert_eq!(
        remote.cmd_call_count() - cmds_before,
        2,
        "idempotent sync must perform exactly 1 retry after stale-mux exit 255"
    );
    assert_eq!(
        remote.exit_call_count() - exits_before,
        1,
        "exit 255 must trigger `ssh -O exit` zombie ControlMaster cleanup"
    );
    let host_after = core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "gpu-cm")
        .unwrap();
    assert_eq!(host_after.status, HostStatus::Online);

    // 3. Stale socket (`exit255_once`) on mutating `dispatch_interactive`:
    //    MUST trigger `ssh -O exit` cleanup and fail with ZERO automatic retry (no duplicate paste!).
    let remote_sess = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.effective_host_id() == "gpu-cm")
        .expect("discovered remote claude session");
    let cmds_before_disp = remote.cmd_call_count();
    let exits_before_disp = remote.exit_call_count();
    remote.set_mode("exit255_once");
    let disp_res = core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: remote_sess.session_id.clone(),
            },
            text: "never duplicate this paste".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await;
    assert!(
        disp_res.is_err(),
        "mutating dispatch_interactive must return error on exit 255 without retrying"
    );
    assert_eq!(
        remote.cmd_call_count() - cmds_before_disp,
        1,
        "dispatch_interactive MUST NOT retry on exit 255 (1 CMD call only!)"
    );
    assert_eq!(
        remote.exit_call_count() - exits_before_disp,
        1,
        "dispatch_interactive exit 255 must still run `ssh -O exit` cleanup"
    );
}

// ===========================================================================
// Test 2: Compound discovery + remote spawn (`$REMOTE_HOME/.agent_session`,
// sequential `codex-1`/`codex-2`, zero transcript/metrics hijacking across
// sibling panes).
// ===========================================================================
#[tokio::test]
async fn test_02_compound_discovery_remote_spawn_and_fresh_spawn_isolation() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t2");
    let (hub_dir, _hub_guard) = mk_hub_dir("t2");
    let hub_home = hub_dir.join("hub_home");
    std::fs::create_dir_all(hub_home.join(".agent_session")).unwrap();

    // Seed historical Codex + Claude + AGY transcripts in `$REMOTE_HOME/.agent_session`.
    let remote_agent_dir = remote.remote_home.join(".agent_session");
    std::fs::create_dir_all(&remote_agent_dir).unwrap();
    let old_cx_id = "01a18088-9999-7820-9614-90298fe15a06";
    let old_cx_path = remote
        .remote_home
        .join(".codex/sessions/2026/11/24")
        .join(format!("rollout-2026-11-24T01-00-00-{old_cx_id}.jsonl"));
    std::fs::create_dir_all(old_cx_path.parent().unwrap()).unwrap();
    std::fs::write(
        &old_cx_path,
        format!(
            "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-24T01:00:00.000Z\",\"payload\":{{\"id\":\"{old_cx_id}\",\"timestamp\":\"2026-11-24T01:00:00.000Z\",\"cwd\":\"{}\"}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"2026-11-24T01:00:02.000Z\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"historical remote prompt\"}}]}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"2026-11-24T01:00:05.000Z\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"historical remote answer\"}}]}}}}\n",
            remote_agent_dir.display()
        ),
    )
    .unwrap();
    if has_sqlite3() {
        run_sqlite(
            &remote.remote_home.join(".codex/state_5.sqlite"),
            &format!(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, cwd TEXT NOT NULL, name TEXT, archived INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO threads VALUES ('{old_cx_id}', '{}', 1700000000, 1700000050, '{}', 'historical remote prompt', 0);",
                old_cx_path.display(),
                remote_agent_dir.display()
            ),
        );
    }

    let (core, addr, srv) = start_hub_with_web(
        &hub_dir.join("state"),
        vec![remote.node_config("remote-gpu", "Remote GPU")],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;
    core.set_home_dir_override(Some(hub_home.clone()));

    // Spawn 1st Codex session on `remote-gpu` with `project_root` omitted -> expands remote `$HOME/.agent_session`.
    let (st1, r1) = http_json(
        addr,
        "POST",
        "/api/sessions/spawn",
        None,
        Some(&json!({
            "agent": "codex",
            "host_id": "remote-gpu",
            "model": "gpt-6.2-sol",
            "effort": "xhigh"
        })),
    )
    .await;
    assert_eq!(st1, 200, "{r1}");
    let d1 = &r1["data"];
    assert_eq!(d1["host_id"], "remote-gpu");
    assert_eq!(
        d1["cwd"].as_str().unwrap(),
        remote_agent_dir.to_string_lossy().as_ref(),
        "remote spawn must expand $REMOTE_HOME/.agent_session, NOT local hub home"
    );
    assert!(
        d1["tmux_label"].as_str().unwrap().ends_with("codex-1"),
        "{d1}"
    );
    let sid1 = SessionId::new(d1["session_id"].as_str().unwrap());

    // Spawn 2nd Codex session on `remote-gpu` -> sequential `codex-2`!
    let (st2, r2) = http_json(
        addr,
        "POST",
        "/api/sessions/spawn",
        None,
        Some(&json!({
            "agent": "codex",
            "host_id": "remote-gpu",
            "model": "gpt-6.2-astra",
            "effort": "high"
        })),
    )
    .await;
    assert_eq!(st2, 200, "{r2}");
    let d2 = &r2["data"];
    assert_eq!(d2["host_id"], "remote-gpu");
    assert!(
        d2["tmux_label"].as_str().unwrap().ends_with("codex-2"),
        "{d2}"
    );
    let sid2 = SessionId::new(d2["session_id"].as_str().unwrap());

    // Verify `GET /api/tmux/sessions?host=remote-gpu` lists the remote tmux session (`agent-session`)
    // and unknown host returns 404.
    let (st_ls, r_ls) = http_json(
        addr,
        "GET",
        "/api/tmux/sessions?host=remote-gpu",
        None,
        None,
    )
    .await;
    assert_eq!(st_ls, 200, "{r_ls}");
    let sess_list: Vec<&str> = r_ls["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        sess_list.contains(&"agent-session"),
        "expected agent-session in remote tmux sessions: {sess_list:?}"
    );
    let (st_404, _) = http_json(
        addr,
        "GET",
        "/api/tmux/sessions?host=no-such-host",
        None,
        None,
    )
    .await;
    assert_eq!(st_404, 404);

    // Run multiple discovery ticks -> BOTH freshly spawned sibling panes must have ZERO
    // transcript or metrics hijacking from historical `$REMOTE_HOME/.agent_session` data!
    core.sync_remote_host(&HostId::new("remote-gpu")).await;
    core.sync_remote_host(&HostId::new("remote-gpu")).await;

    let snap = core.snapshot();
    for (sid, expected_model) in [(&sid1, "gpt-6.2-sol"), (&sid2, "gpt-6.2-astra")] {
        let rec = snap
            .sessions
            .iter()
            .find(|s| &s.session_id == sid)
            .expect("spawned remote session present");
        assert_eq!(
            rec.transcript_path, None,
            "fresh sibling remote pane must NOT hijack old transcript: {rec:?}"
        );
        assert_eq!(
            rec.metrics, None,
            "fresh sibling remote pane must NOT inherit old metrics: {rec:?}"
        );
        assert_eq!(rec.model.as_deref(), Some(expected_model));
        let tr = core
            .session_transcript(sid, TranscriptQuery::tail(20))
            .await
            .unwrap();
        assert_eq!(tr.source, "none");
        assert!(tr.messages.is_empty());
    }

    srv.abort();
}

// ===========================================================================
// Test 3: Exact 3-agent remote transcript resolution:
// - Claude `kill -0` live-PID filter (ignoring dead & skewed recycled PIDs)
// - AGY live presence lock (`lsof`/`/proc`) + Swarm `nesting_depth = 1` children
// - Codex exact `resume <uuid>` + pure-POSIX `session_meta` fallback WITHOUT `sqlite3`!
// ===========================================================================
#[tokio::test]
async fn test_03_exact_three_agent_remote_transcript_resolution_and_zero_sqlite3_fallback() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t3");
    let (hub_dir, _hub_guard) = mk_hub_dir("t3");

    // Prepare isolated remote cwds for each agent pane.
    let cl_cwd = remote.remote_home.join("proj-claude");
    let agy_cwd = remote.remote_home.join("proj-agy");
    let cx_res_cwd = remote.remote_home.join("proj-codex-resume");
    let cx_posix_cwd = remote.remote_home.join("proj-codex-posix");
    for d in [&cl_cwd, &agy_cwd, &cx_res_cwd, &cx_posix_cwd] {
        std::fs::create_dir_all(d).unwrap();
    }

    // 1. Claude: live-PID session file written by the running fake `claude` binary (`$$`),
    //    plus a dead PID `999999.json` and a skewed recycled-PID file (must be ignored).
    let cl_sid = "cl-live-0001-aaaa-bbbb-ccccddddeeee";
    let cl_enc = encode_claude_project_dir(&cl_cwd.to_string_lossy());
    let cl_tr = remote
        .remote_home
        .join(".claude/projects")
        .join(&cl_enc)
        .join(format!("{cl_sid}.jsonl"));
    std::fs::create_dir_all(cl_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &cl_tr,
        r#"{"type":"user","timestamp":"2026-11-25T08:00:00Z","message":{"role":"user","content":"claude remote ping"}}
{"type":"assistant","timestamp":"2026-11-25T08:00:02Z","message":{"id":"m1","model":"claude-opus-4-6","stop_reason":"end_turn","content":[{"type":"text","text":"claude remote pong"}],"usage":{"input_tokens":500,"cache_read_input_tokens":200,"cache_creation_input_tokens":0,"output_tokens":120}}}
"#,
    )
    .unwrap();
    let cl_sess_dir = remote.remote_home.join(".claude/sessions");
    std::fs::create_dir_all(&cl_sess_dir).unwrap();
    // Dead PID leftover -> `kill -0 999999` filters it out!
    std::fs::write(
        cl_sess_dir.join("999999.json"),
        format!(
            r#"{{"pid":999999,"sessionId":"dead-pid-sid","cwd":"{}","startedAt":1795500000000}}"#,
            cl_cwd.display()
        ),
    )
    .unwrap();
    let cl_inline = format!(
        r#"exec -a claude /bin/bash -c 'NOW_MS=$(($(date +%s) * 1000)); printf "{{\"pid\":%s,\"sessionId\":\"{cl_sid}\",\"cwd\":\"{cwd}\",\"startedAt\":%s}}\n" "$$" "$NOW_MS" > "{sdir}/$$.json"; sleep 120; true' claude"#,
        cl_sid = cl_sid,
        cwd = cl_cwd.display(),
        sdir = cl_sess_dir.display()
    );

    // 2. AGY: live process holding real open fd on `presence/<agy_cid>.lock` + Swarm child in sqlite3!
    let agy_cid = "agy-root-1111-2222-3333-444455556666";
    let agy_child_cid = "agy-kid-7777-8888-9999-000011112222";
    let agy_lock = remote
        .remote_home
        .join(".gemini/antigravity-cli/presence")
        .join(format!("{agy_cid}.lock"));
    std::fs::create_dir_all(agy_lock.parent().unwrap()).unwrap();
    std::fs::write(&agy_lock, "lock\n").unwrap();
    let agy_tr = remote
        .remote_home
        .join(".gemini/antigravity-cli/brain")
        .join(agy_cid)
        .join(".system_generated/logs/transcript.jsonl");
    std::fs::create_dir_all(agy_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &agy_tr,
        r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-25T08:01:00Z","content":"<USER_REQUEST>\nagy remote question\n</USER_REQUEST>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-25T08:01:03Z","model":"gemini-4-neon-high","content":"agy remote answer"}
"#,
    )
    .unwrap();
    let agy_child_tr = remote
        .remote_home
        .join(".gemini/antigravity-cli/brain")
        .join(agy_child_cid)
        .join(".system_generated/logs/transcript.jsonl");
    std::fs::create_dir_all(agy_child_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &agy_child_tr,
        r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-25T08:01:05Z","content":"<USER_REQUEST>\nsubagent task\n</USER_REQUEST>"}
"#,
    )
    .unwrap();
    if has_sqlite3() {
        run_sqlite(
            &remote
                .remote_home
                .join(".gemini/antigravity-cli/conversation_summaries.db"),
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
                  ('{agy_cid}', 'Remote Root', '', 2, '2026-11-25 08:01:03+00:00', '[\"file://{}\"]', 'CASCADE_RUN_STATUS_RUNNING', '', '', 0, 0, '2026-11-25 08:01:00+00:00'),
                  ('{agy_child_cid}', 'Swarm Worker', '', 1, '2026-11-25 08:01:05+00:00', '[\"file://{}\"]', 'CASCADE_RUN_STATUS_RUNNING', 'researcher', '{agy_cid}', 1, 0, '2026-11-25 08:01:05+00:00');",
                agy_cwd.display(),
                agy_cwd.display()
            ),
        );
    }
    let agy_inline = format!(
        "exec -a agy /bin/bash -c 'exec 9< \"{lock}\"; sleep 120; true' agy",
        lock = agy_lock.display()
    );

    // 3. Codex #1: exact `codex resume <uuid>`
    let cx_res_id = "01a19000-1111-7222-8333-444455556666";
    let cx_res_tr = remote
        .remote_home
        .join(".codex/sessions/2026/11/25")
        .join(format!("rollout-2026-11-25T08-02-00-{cx_res_id}.jsonl"));
    std::fs::create_dir_all(cx_res_tr.parent().unwrap()).unwrap();
    std::fs::write(
        &cx_res_tr,
        format!(
            "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-25T08:02:00.000Z\",\"payload\":{{\"id\":\"{cx_res_id}\",\"timestamp\":\"2026-11-25T08:02:00.000Z\",\"cwd\":\"{}\"}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"2026-11-25T08:02:01.000Z\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"codex resume hello\"}}]}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"2026-11-25T08:02:03.000Z\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"codex resume reply\"}}]}}}}\n",
            cx_res_cwd.display()
        ),
    )
    .unwrap();

    // Spawn all 4 remote panes in the isolated remote tmux server.
    let p_cl = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("agents".into()),
            create_session: true,
            window_name: "cl".into(),
            cwd: cl_cwd.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), cl_inline],
        })
        .unwrap();
    let p_agy = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("agents".into()),
            create_session: false,
            window_name: "ag".into(),
            cwd: agy_cwd.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), agy_inline],
        })
        .unwrap();
    let p_cx_res = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("agents".into()),
            create_session: false,
            window_name: "cxr".into(),
            cwd: cx_res_cwd.clone(),
            env: Vec::new(),
            argv: vec![remote.agent_bin("codex"), "resume".into(), cx_res_id.into()],
        })
        .unwrap();
    let p_cx_posix = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("agents".into()),
            create_session: false,
            window_name: "cxp".into(),
            cwd: cx_posix_cwd.clone(),
            env: Vec::new(),
            argv: vec![remote.agent_bin("codex")],
        })
        .unwrap();

    // Give the spawned fake binaries a moment to write Claude's `<pid>.json` and open AGY's lock fd.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Create Codex #2's post-birth rollout JSONL (timestamp == current UTC ISO-8601) AFTER pane birth!
    let now_iso = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%S.000Z"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap();
    let cx_posix_id = "01a19000-aaaa-7bbb-8ccc-ddddeeeeffff";
    let cx_posix_tr = remote
        .remote_home
        .join(".codex/sessions/2026/11/25")
        .join(format!("rollout-2026-11-25T23-59-59-{cx_posix_id}.jsonl"));
    std::fs::write(
        &cx_posix_tr,
        format!(
            "{{\"type\":\"session_meta\",\"timestamp\":\"{now_iso}\",\"payload\":{{\"id\":\"{cx_posix_id}\",\"timestamp\":\"{now_iso}\",\"cwd\":\"{}\"}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"{now_iso}\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"pure posix codex prompt\"}}]}}}}\n\
             {{\"type\":\"response_item\",\"timestamp\":\"{now_iso}\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"pure posix codex reply\"}}]}}}}\n",
            cx_posix_cwd.display()
        ),
    )
    .unwrap();

    let core = build_hub_core(
        &hub_dir.join("state"),
        vec![remote.node_config("remote-3a", "Remote 3A")],
        WebConfig::default(),
    );

    // Tick 1 (normal mode): resolves Claude live-PID, AGY lock + Swarm child, and Codex resume!
    core.sync_remote_host(&HostId::new("remote-3a")).await;

    let cl_sid_full = SessionId::new(format!("remote-3a:tmux-{}", p_cl.as_str()));
    let agy_sid_full = SessionId::new(format!("remote-3a:tmux-{}", p_agy.as_str()));
    let cx_res_sid_full = SessionId::new(format!("remote-3a:tmux-{}", p_cx_res.as_str()));
    let cx_posix_sid_full = SessionId::new(format!("remote-3a:tmux-{}", p_cx_posix.as_str()));

    let cl_tr_resp = core
        .session_transcript(&cl_sid_full, TranscriptQuery::tail(20))
        .await
        .unwrap();
    assert_eq!(cl_tr_resp.source, "transcript");
    assert_eq!(cl_tr_resp.messages.len(), 2);
    assert_eq!(cl_tr_resp.messages[0].text, "claude remote ping");
    assert_eq!(cl_tr_resp.messages[1].text, "claude remote pong");

    let agy_tr_resp = core
        .session_transcript(&agy_sid_full, TranscriptQuery::tail(20))
        .await
        .unwrap();
    assert_eq!(agy_tr_resp.source, "transcript");
    assert_eq!(agy_tr_resp.messages.len(), 2);
    assert_eq!(agy_tr_resp.messages[1].text, "agy remote answer");
    if has_sqlite3() {
        let snap = core.snapshot();
        let agy_root = snap
            .sessions
            .iter()
            .find(|s| s.session_id == agy_sid_full)
            .unwrap();
        assert_eq!(agy_root.subagent_count, 1, "{agy_root:?}");
        assert_eq!(agy_root.active_subagents, 1, "{agy_root:?}");
        let child_rec = snap
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == format!("remote-3a:{agy_child_cid}"))
            .expect("AGY Swarm child discovered over SSH");
        assert!(child_rec.is_subagent);
        assert_eq!(child_rec.parent_session_id.as_ref(), Some(&agy_sid_full));
        assert_eq!(child_rec.tmux_label.as_deref(), Some("researcher"));
    }

    let cx_res_resp = core
        .session_transcript(&cx_res_sid_full, TranscriptQuery::tail(20))
        .await
        .unwrap();
    assert_eq!(cx_res_resp.source, "transcript");
    assert_eq!(cx_res_resp.messages[1].text, "codex resume reply");

    // Tick 2 & Tick 3 with `sqlite3` completely stripped from remote `$PATH`:
    // verifies pure-POSIX `session_meta` rollout fallback resolves Codex #2 AND stays
    // stably attached across subsequent ticks when Hub passes `claimed_external_ids`!
    remote.set_mode("strip_sqlite3");
    core.sync_remote_host(&HostId::new("remote-3a")).await;
    core.sync_remote_host(&HostId::new("remote-3a")).await;

    let cx_posix_resp = core
        .session_transcript(&cx_posix_sid_full, TranscriptQuery::tail(20))
        .await
        .unwrap();
    assert_eq!(cx_posix_resp.source, "transcript");
    assert_eq!(cx_posix_resp.messages.len(), 2);
    assert_eq!(cx_posix_resp.messages[1].text, "pure posix codex reply");
}

// ===========================================================================
// Test 4: Incremental byte-offset mirror + 2 MiB cold tail window + 0-SSH
// unchanged poll + `GZ1` compressed delta + `?before=`/`?all=1` prefix
// backfill + `SessionMetrics`.
// ===========================================================================
#[tokio::test]
async fn test_04_incremental_mirror_cold_tail_zero_rtt_gzip_delta_backfill_and_metrics() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t4");
    let (hub_dir, _hub_guard) = mk_hub_dir("t4");
    let state_dir = hub_dir.join("state");

    let cl_cwd = remote.remote_home.join("big-repo");
    std::fs::create_dir_all(&cl_cwd).unwrap();
    let cl_sid = "cl-big-mirror-0001-2222-333344445555";
    let cl_enc = encode_claude_project_dir(&cl_cwd.to_string_lossy());
    let cl_tr = remote
        .remote_home
        .join(".claude/projects")
        .join(&cl_enc)
        .join(format!("{cl_sid}.jsonl"));
    std::fs::create_dir_all(cl_tr.parent().unwrap()).unwrap();

    // Synthesize a ~3 MiB Claude JSONL (> 2 MiB INITIAL_TAIL_WINDOW) with usage metrics.
    let mut big_jsonl = String::with_capacity(3_300_000);
    let pad = "a".repeat(2_000);
    let initial_pairs = 680usize;
    for i in 0..initial_pairs {
        big_jsonl.push_str(&format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-11-25T07:00:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"turn-u-{i} {pad}\"}}}}\n\
             {{\"type\":\"assistant\",\"timestamp\":\"2026-11-25T07:00:04.000Z\",\"message\":{{\"id\":\"m-{i}\",\"model\":\"claude-opus-4-6\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"turn-a-{i} {pad}\"}}],\"usage\":{{\"input_tokens\":10000,\"cache_read_input_tokens\":5000,\"cache_creation_input_tokens\":0,\"output_tokens\":200}}}}}}\n"
        ));
    }
    assert!(
        big_jsonl.len() > 2_800_000,
        "synthetic JSONL must exceed 2 MiB cold tail window, got {} bytes",
        big_jsonl.len()
    );
    std::fs::write(&cl_tr, &big_jsonl).unwrap();

    let cl_sess_dir = remote.remote_home.join(".claude/sessions");
    std::fs::create_dir_all(&cl_sess_dir).unwrap();
    let cl_inline = format!(
        r#"exec -a claude /bin/bash -c 'NOW_MS=$(($(date +%s) * 1000)); printf "{{\"pid\":%s,\"sessionId\":\"{cl_sid}\",\"cwd\":\"{cwd}\",\"startedAt\":%s}}\n" "$$" "$NOW_MS" > "{sdir}/$$.json"; sleep 120; true' claude"#,
        cl_sid = cl_sid,
        cwd = cl_cwd.display(),
        sdir = cl_sess_dir.display()
    );
    let pane = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("big".into()),
            create_session: true,
            window_name: "cl-big".into(),
            cwd: cl_cwd.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), cl_inline],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (core, addr, srv) = start_hub_with_web(
        &state_dir,
        vec![remote.node_config("remote-big", "Remote Big")],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    // 1. Sync cold-mirrors the trailing 2 MiB window + computes `SessionMetrics`.
    core.sync_remote_host(&HostId::new("remote-big")).await;
    let sid = format!("remote-big:tmux-{}", pane.as_str());
    let enc_sid = sid.replace('%', "%25").replace(':', "%3A");

    // Check `.meta.json` proves cold-start tail windowing (`base_offset > 0`).
    let mirror = RemoteTranscriptMirror::new(state_dir.clone());
    let local_mirror_path =
        mirror.local_mirror_path("remote-big", AgentKind::Claude, &cl_tr.to_string_lossy());
    let meta_path = local_mirror_path.with_extension("meta.json");
    let meta_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    let initial_base_offset = meta_json["base_offset"].as_u64().unwrap();
    assert!(
        initial_base_offset > 0,
        "3 MiB cold seed must tail-window with base_offset > 0: {meta_json}"
    );

    // 2. First HTTP transcript read + unchanged repeat poll -> 0 new SSH calls!
    let cmds_before_poll = remote.cmd_call_count();
    let (st1, r1) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?limit=60"),
        None,
        None,
    )
    .await;
    assert_eq!(st1, 200, "{r1}");
    assert_eq!(r1["data"]["source"], "transcript");
    assert_eq!(r1["data"]["has_more"], true);
    let msgs1 = r1["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs1.len(), 60);
    assert!(msgs1.last().unwrap()["text"]
        .as_str()
        .unwrap()
        .starts_with(&format!("turn-a-{}", initial_pairs - 1)));
    let metrics1 = &r1["data"]["metrics"];
    assert!(
        metrics1["context_used_pct"].as_f64().is_some(),
        "expected context_used_pct in SessionMetrics: {metrics1}"
    );
    assert!(
        metrics1["tps"].as_f64().is_some(),
        "expected tps in SessionMetrics: {metrics1}"
    );
    let (st2, r2) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?limit=60"),
        None,
        None,
    )
    .await;
    assert_eq!(st2, 200, "{r2}");
    assert_eq!(
        remote.cmd_call_count(),
        cmds_before_poll,
        "unchanged remote transcript poll must make 0 SSH calls"
    );

    // 3. Append ~24 KiB (>= 16 KiB GZIP_COMPRESS_THRESHOLD) to remote JSONL, sync, and verify
    //    delta arrives cleanly with updated turns & `synced_offset` advancing by exact appended bytes!
    let mut delta_chunk = String::with_capacity(28_000);
    let delta_pad = "z".repeat(1_800);
    for j in 0..6 {
        let idx = initial_pairs + j;
        delta_chunk.push_str(&format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-11-25T07:05:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"delta-u-{idx} {delta_pad}\"}}}}\n\
             {{\"type\":\"assistant\",\"timestamp\":\"2026-11-25T07:05:02.000Z\",\"message\":{{\"id\":\"md-{idx}\",\"model\":\"claude-opus-4-6\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"delta-a-{idx} {delta_pad}\"}}],\"usage\":{{\"input_tokens\":22000,\"cache_read_input_tokens\":8000,\"cache_creation_input_tokens\":0,\"output_tokens\":400}}}}}}\n"
        ));
    }
    assert!(delta_chunk.len() >= 20_000);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&cl_tr)
            .unwrap();
        f.write_all(delta_chunk.as_bytes()).unwrap();
    }
    let expected_total_bytes = (big_jsonl.len() + delta_chunk.len()) as u64;
    let cmds_before_delta = remote.cmd_call_count();
    core.sync_remote_host(&HostId::new("remote-big")).await;
    // 1 compound discovery RTT + 1 GZ1 delta mirror RTT = 2 SSH calls total!
    assert_eq!(
        remote.cmd_call_count() - cmds_before_delta,
        2,
        "sync + delta append must take exactly 2 SSH RTTs (1 discover + 1 GZ1 delta)"
    );
    let meta_after_delta: Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        meta_after_delta["synced_offset"].as_u64(),
        Some(expected_total_bytes)
    );
    assert_eq!(
        meta_after_delta["base_offset"].as_u64(),
        Some(initial_base_offset),
        "incremental delta must NOT re-download prefix"
    );

    let (st3, r3) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?limit=10"),
        None,
        None,
    )
    .await;
    assert_eq!(st3, 200, "{r3}");
    assert!(
        r3["data"]["messages"].as_array().unwrap().last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with(&format!("delta-a-{}", initial_pairs + 5))
    );

    // 4. Request `?before=10&limit=60` (reaches start of tail window) -> triggers 1-RTT prefix
    //    backfill (`base_offset == 0`) AND `?all=1` exposes turn-u-0 from byte 0!
    let cmds_before_bf = remote.cmd_call_count();
    let (st_bf, r_bf) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?before=10&limit=60"),
        None,
        None,
    )
    .await;
    assert_eq!(st_bf, 200, "{r_bf}");
    assert_eq!(
        remote.cmd_call_count() - cmds_before_bf,
        1,
        "prefix backfill on upward paging must take 1 SSH RTT"
    );
    let meta_after_bf: Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta_after_bf["base_offset"].as_u64(), Some(0));

    let cmds_before_all = remote.cmd_call_count();
    let (st_all, r_all) = http_json(
        addr,
        "GET",
        &format!("/api/sessions/{enc_sid}/transcript?all=1"),
        None,
        None,
    )
    .await;
    assert_eq!(st_all, 200, "{r_all}");
    assert_eq!(
        remote.cmd_call_count(),
        cmds_before_all,
        "`?all=1` after backfill must hit 0-RTT local mirror"
    );
    let all_msgs = r_all["data"]["messages"].as_array().unwrap();
    assert!(
        all_msgs[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("turn-u-0 "),
        "backfilled history must start at turn-u-0, got {:?}",
        all_msgs[0]["text"]
    );
    assert_eq!(r_all["data"]["has_more"], false);

    srv.abort();
}

// ===========================================================================
// Test 5: Path traversal & symlink rejection.
// ===========================================================================
#[tokio::test]
async fn test_05_path_traversal_and_symlink_rejection() {
    let remote = FakeSshHost::new("t5");
    let (hub_dir, _hub_guard) = mk_hub_dir("t5");
    let rhome = remote.remote_home.to_string_lossy().into_owned();

    // 1. Rust allowlist rejects traversal, outside paths, and control characters.
    assert!(
        RemoteTranscriptMirror::validate_remote_transcript_path(&rhome, "../../../etc/passwd")
            .is_err()
    );
    assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
        &rhome,
        &format!("{rhome}/.claude/projects/../../etc/passwd")
    )
    .is_err());
    assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
        &rhome,
        &format!("{rhome}/.ssh/id_rsa")
    )
    .is_err());
    assert!(RemoteTranscriptMirror::validate_remote_transcript_path(
        &rhome,
        &format!("{rhome}/.claude/projects/x/a.jsonl\nrm -rf /")
    )
    .is_err());

    // 2. Remote shell `[ -f "$1" ] && [ ! -L "$1" ]` rejects a symlinked `.jsonl` inside
    //    an otherwise allowlisted Claude directory!
    let secret = remote.root.join("secret.txt");
    std::fs::write(&secret, "TOP_SECRET_DO_NOT_LEAK\n").unwrap();
    let cl_dir = remote.remote_home.join(".claude/projects/-tmp-demo");
    std::fs::create_dir_all(&cl_dir).unwrap();
    let sym_jsonl = cl_dir.join("symlinked-session.jsonl");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, &sym_jsonl).unwrap();

    let mirror = RemoteTranscriptMirror::new(hub_dir.join("state"));
    let ssh = SshExec::new(&remote.ssh_target, hub_dir.join("cm.sock"))
        .with_ssh_bin(Some(remote.fake_ssh.to_string_lossy().into_owned()));
    let res = mirror.ensure_fresh(
        &ssh,
        "remote-t5",
        &rhome,
        AgentKind::Claude,
        &sym_jsonl.to_string_lossy(),
        None,
        false,
    );
    assert!(
        res.is_err(),
        "symlinked remote .jsonl MUST be rejected by remote `[ ! -L ]` check"
    );
    let local_cache =
        mirror.local_mirror_path("remote-t5", AgentKind::Claude, &sym_jsonl.to_string_lossy());
    assert!(
        !local_cache.exists(),
        "rejected symlink must never create a local mirror file"
    );
}

// ===========================================================================
// Test 6: Remote image upload (`POST /api/uploads/image` with `host_id`) +
// long-paste staging (`0600` in `$REMOTE_HOME/.cache/starfane/inputs/` +
// Hub-local path rewrite in dispatched prompt AND inside staged `paste-*.md`).
// ===========================================================================
#[tokio::test]
async fn test_06_remote_image_upload_and_long_paste_staging_and_path_rewrite() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t6");
    let (hub_dir, _hub_guard) = mk_hub_dir("t6");
    let hub_state = hub_dir.join("state");

    // Remote pane running a fake `claude` that records every received stdin line to a file.
    let received_file = remote.root.join("pane_received.txt");
    let cl_rec_inline = format!(
        "exec -a claude /bin/bash -c 'printf \"claude ready\\n\"; while IFS= read -r line; do printf \"%s\\n\" \"$line\" >> \"{rec}\"; done' claude",
        rec = received_file.display()
    );
    let pane = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("imgtest".into()),
            create_session: true,
            window_name: "cl-1".into(),
            cwd: remote.remote_home.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), cl_rec_inline],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (core, addr, srv) = start_hub_with_web(
        &hub_state,
        vec![remote.node_config("remote-img", "Remote Img")],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;
    core.sync_remote_host(&HostId::new("remote-img")).await;

    // 1. Upload image with `host_id: "remote-img"` (with spaces in original filename!).
    let (st_up, r_up) = http_json(
        addr,
        "POST",
        "/api/uploads/image",
        None,
        Some(&json!({
            "filename": "screen shot 1.png",
            "mime": "image/png",
            "host_id": "remote-img",
            "data_base64": SAMPLE_PNG_B64
        })),
    )
    .await;
    assert_eq!(st_up, 200, "{r_up}");
    let hub_img_path = r_up["data"]["path"].as_str().unwrap().to_string();
    let img_fname = Path::new(&hub_img_path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected_remote_img = remote
        .remote_home
        .join(".cache/starfane/inputs")
        .join(&img_fname);

    // Wait for background pre-staging task triggered by `POST /api/uploads/image` to complete.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !expected_remote_img.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        expected_remote_img.exists(),
        "background pre-staging must place image at {}",
        expected_remote_img.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&expected_remote_img)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "remote staged image must be 0600");
    }

    // 2. Dispatch `[图片 #1]` + a 60-line long paste attachment to the remote session!
    let long_paste = (1..=60)
        .map(|i| format!("stacktrace line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let remote_sid = format!("remote-img:tmux-{}", pane.as_str());
    let (st_disp, r_disp) = http_json(
        addr,
        "POST",
        "/api/dispatch",
        None,
        Some(&json!({
            "target": { "type": "session", "id": remote_sid },
            "text": "请结合截图 [图片 #1] 分析以下日志：[粘贴 #1]",
            "interactive": true,
            "attachments": [
                { "kind": "image", "title": "screen shot 1.png", "path": hub_img_path, "token": "[图片 #1]" },
                { "kind": "text", "title": "log", "content": long_paste, "token": "[粘贴 #1]" }
            ]
        })),
    )
    .await;
    assert_eq!(st_disp, 200, "{r_disp}");
    assert_eq!(r_disp["data"]["status"], "delivered", "{r_disp}");

    // Verify staged `paste-*.md` exists in `$REMOTE_HOME/.cache/starfane/inputs/` with mode `0600`
    // AND has Hub-local image path rewritten to remote path!
    let remote_inputs_dir = remote.remote_home.join(".cache/starfane/inputs");
    let remote_paste_file = std::fs::read_dir(&remote_inputs_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("paste-") && n.ends_with(".md"))
        })
        .expect("remote staged paste-*.md must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&remote_paste_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "remote staged paste file must be 0600");
    }
    let remote_paste_content = std::fs::read_to_string(&remote_paste_file).unwrap();
    assert!(remote_paste_content.contains("stacktrace line 60"));
    assert!(
        remote_paste_content.contains(&expected_remote_img.to_string_lossy().into_owned()),
        "staged paste file must rewrite embedded image path to remote path: {remote_paste_content}"
    );
    assert!(
        !remote_paste_content.contains(&hub_state.to_string_lossy().into_owned()),
        "staged paste file must NOT leak Hub-local state_dir path: {remote_paste_content}"
    );

    // Wait briefly for remote pane to flush captured line and verify injected prompt paths!
    let dl2 = Instant::now() + Duration::from_secs(3);
    let mut captured = String::new();
    while Instant::now() < dl2 {
        captured = std::fs::read_to_string(&received_file).unwrap_or_default();
        if !captured.trim().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        captured.contains(&remote_paste_file.to_string_lossy().into_owned()),
        "remote pane prompt must reference remote staged paste path, got: {captured:?}"
    );
    assert!(
        captured.contains(&expected_remote_img.to_string_lossy().into_owned()),
        "remote pane prompt must reference remote staged image path, got: {captured:?}"
    );
    assert!(
        !captured.contains(&hub_state.to_string_lossy().into_owned()),
        "remote pane prompt must NEVER contain Hub-local path: {captured:?}"
    );

    srv.abort();
}

// ===========================================================================
// Test 7: 1-RTT compound interactive dispatch:
// - Copy-mode auto-cancel + bracketed paste + submit in 1 `fake-ssh` call
// - Stuck-in-prompt automatic `C-m` retry -> `Delivered` (`retry_confirmed`)
// - Persistent stuck prompt -> `Unconfirmed { ack: "stuck_in_prompt" }`
// ===========================================================================
#[tokio::test]
async fn test_07_single_rtt_compound_dispatch_copy_mode_and_stuck_retry() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t7");
    let (hub_dir, _hub_guard) = mk_hub_dir("t7");

    // 1. Copy-mode auto-cancel in 1 single SSH RTT!
    let rec_a = remote.root.join("rec_a.txt");
    let cx_rec_inline = format!(
        "exec -a codex /bin/bash -c 'printf \"codex ready\\n\"; while IFS= read -r l; do printf \"%s\\n\" \"$l\" >> \"{rec}\"; done' codex",
        rec = rec_a.display()
    );
    let p_copy = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("disp".into()),
            create_session: true,
            window_name: "copy".into(),
            cwd: remote.remote_home.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), cx_rec_inline],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Put remote pane into real tmux `copy-mode`!
    let status_cm = Command::new("tmux")
        .args([
            "-L",
            &remote.tmux_socket,
            "copy-mode",
            "-t",
            p_copy.as_str(),
        ])
        .status()
        .unwrap();
    assert!(status_cm.success());

    // 2. Mock Claude TUI #1: swallows first Enter (text stays in `❯ <text>`), submits on 2nd Enter!
    // 3. Mock Claude TUI #2: persistently keeps text stuck in `❯ <text>` on every Enter!
    // Uses POSIX `stty -echo raw` + CSI filter so bracketed paste & C-m are exact.
    let tui_inline = |mode: &str| {
        format!(
            r#"exec -a claude /bin/bash -c '
  MODE="{mode}"
  stty -echo raw 2>/dev/null || true
  printf "\033[2J\033[HClaude Code Ready\r\n❯ "
  ESC=$(printf "\033")
  CR=$(printf "\r")
  LF=$(printf "\n")
  BUF=""
  ENTERS=0
  while IFS= read -r -n 1 ch; do
    if [ "$ch" = "$ESC" ]; then
      IFS= read -r -n 1 _next
      if [ "$_next" = "[" ]; then
        while IFS= read -r -n 1 _seq; do
          case "$_seq" in
            [a-zA-Z~]) break ;;
          esac
        done
      fi
    elif [ -z "$ch" ] || [ "$ch" = "$CR" ] || [ "$ch" = "$LF" ]; then
      ENTERS=$((ENTERS + 1))
      if [ "$MODE" = "submit_on_second" ] && [ "$ENTERS" -ge 2 ]; then
        printf "\033[2J\033[HSubmitted: %s\r\nWorking...\r\n❯ " "$BUF"
        BUF=""
      fi
    else
      case "$ch" in
        [a-zA-Z0-9_\-\ \.\:\!\?])
          BUF="${{BUF}}${{ch}}"
          printf "%s" "$ch"
          ;;
      esac
    fi
  done
' claude"#
        )
    };

    let p_retry = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("disp".into()),
            create_session: false,
            window_name: "retry".into(),
            cwd: remote.remote_home.clone(),
            env: Vec::new(),
            argv: vec![
                "/bin/bash".into(),
                "-c".into(),
                tui_inline("submit_on_second"),
            ],
        })
        .unwrap();
    let p_stuck = remote
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("disp".into()),
            create_session: false,
            window_name: "stuck".into(),
            cwd: remote.remote_home.clone(),
            env: Vec::new(),
            argv: vec!["/bin/bash".into(), "-c".into(), tui_inline("always_stuck")],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    let core = build_hub_core(
        &hub_dir.join("state"),
        vec![remote.node_config("remote-d", "Remote D")],
        WebConfig::default(),
    );
    core.sync_remote_host(&HostId::new("remote-d")).await;

    // Part A: copy-mode auto-cancel + paste + Enter completes in EXACTLY 1 `fake-ssh` call!
    let calls_a = remote.cmd_call_count();
    let rep_a = core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new(format!("remote-d:tmux-{}", p_copy.as_str())),
            },
            text: "copy mode cancelled cleanly".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .unwrap();
    assert_eq!(rep_a["status"], "delivered", "{rep_a}");
    assert!(
        rep_a["ack"]
            .as_str()
            .unwrap_or("")
            .contains("exited_copy_mode_confirmed"),
        "{rep_a}"
    );
    assert_eq!(
        remote.cmd_call_count() - calls_a,
        1,
        "copy-mode cancel + paste + submit must execute in 1 single SSH RTT"
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    let got_a = std::fs::read_to_string(&rec_a).unwrap_or_default();
    assert!(
        got_a.contains("copy mode cancelled cleanly"),
        "remote pane did not receive text: {got_a:?}"
    );

    // Part B: First C-m swallowed -> automatic retry C-m submits cleanly (`retry_confirmed`, 2 RTTs)!
    let calls_b = remote.cmd_call_count();
    let rep_b = core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new(format!("remote-d:tmux-{}", p_retry.as_str())),
            },
            text: "retryme-alpha-token".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .unwrap();
    assert_eq!(rep_b["status"], "delivered", "{rep_b}");
    assert!(
        rep_b["ack"]
            .as_str()
            .unwrap_or("")
            .contains("retry_confirmed"),
        "expected retry_confirmed ack: {rep_b}"
    );
    assert_eq!(
        remote.cmd_call_count() - calls_b,
        2,
        "1 compound dispatch + 1 C-m retry = 2 SSH RTTs"
    );

    // Part C: Persistently stuck prompt -> 2 bounded retries -> `Unconfirmed { ack: "stuck_in_prompt" }`!
    let calls_c = remote.cmd_call_count();
    let rep_c = core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new(format!("remote-d:tmux-{}", p_stuck.as_str())),
            },
            text: "forever-stuck-token".into(),
            attachments: Vec::new(),
            allow: Vec::new(),
            interactive: true,
        })
        .await
        .unwrap();
    assert_eq!(rep_c["status"], "unconfirmed", "{rep_c}");
    assert_eq!(rep_c["ack"], "stuck_in_prompt", "{rep_c}");
    assert_eq!(
        remote.cmd_call_count() - calls_c,
        3,
        "1 compound dispatch + 2 bounded C-m retries = 3 SSH RTTs"
    );
}

// ===========================================================================
// Test 8: Independent per-host circuit breaker & hung-host fault isolation
// (`host-dead` hung while `local` & `host-fast` stay responsive,
// `Degraded -> Offline` backoff, 0ms silent fast-fail,
// `POST /api/hosts/{id}/probe` recovery + `403` read-only check).
// ===========================================================================
#[tokio::test]
async fn test_08_independent_circuit_breaker_hung_host_isolation_and_probe_auth() {
    if !has_tmux() {
        return;
    }
    let fast = FakeSshHost::new("t8-fast");
    let dead = FakeSshHost::new("t8-dead");
    let (hub_dir, _hub_guard) = mk_hub_dir("t8");

    let _p_fast = fast
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("fast-s".into()),
            create_session: true,
            window_name: "cl-1".into(),
            cwd: fast.remote_home.clone(),
            env: Vec::new(),
            argv: vec![fast.agent_bin("claude")],
        })
        .unwrap();
    let p_dead = dead
        .tmux
        .spawn_window(&starfane_tmux::SpawnWindow {
            target_session: Some("dead-s".into()),
            create_session: true,
            window_name: "cl-1".into(),
            cwd: dead.remote_home.clone(),
            env: Vec::new(),
            argv: vec![dead.agent_bin("claude")],
        })
        .unwrap();

    let web_cfg = WebConfig {
        auth_enabled: true,
        scoped_tokens: vec![
            ScopedTokenConfig {
                label: "admin".into(),
                token: "admin-secret-tok".into(),
                token_env: String::new(),
                read_only: false,
            },
            ScopedTokenConfig {
                label: "viewer".into(),
                token: "ro-viewer-tok".into(),
                token_env: String::new(),
                read_only: true,
            },
        ],
        ..WebConfig::default()
    };
    let (core, addr, srv) = start_hub_with_web(
        &hub_dir.join("state"),
        vec![
            fast.node_config("host-fast", "Host Fast"),
            dead.node_config("host-dead", "Host Dead"),
        ],
        web_cfg,
    )
    .await;

    // Initial sync with both hosts healthy so `host-dead` has a visible session.
    core.sync_remote_nodes().await;
    let host_dead_init = core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "host-dead")
        .unwrap();
    assert_eq!(
        host_dead_init.status,
        HostStatus::Online,
        "{host_dead_init:?}"
    );

    // 1. Put `host-dead` into `hang` mode and spawn its independent supervisor task so it is
    //    genuinely hung in-flight while `local` & `host-fast` run concurrently!
    dead.set_mode("hang");
    fast.set_mode("delay_150ms");
    let core_hang = core.clone();
    let hung_task = tokio::spawn(async move {
        core_hang.sync_remote_host(&HostId::new("host-dead")).await;
    });
    tokio::time::sleep(Duration::from_millis(60)).await;

    let t0 = Instant::now();
    core.sync_discovered_panes().await;
    core.sync_remote_host(&HostId::new("host-fast")).await;
    let fast_elapsed = t0.elapsed();
    assert!(
        fast_elapsed < Duration::from_millis(1500),
        "hung host-dead must NOT block local discovery or host-fast sync (took {fast_elapsed:?})"
    );
    let fast_rec = core
        .hosts()
        .into_iter()
        .find(|h| h.id.as_str() == "host-fast")
        .unwrap();
    assert_eq!(fast_rec.status, HostStatus::Online);
    assert!(
        fast_rec.rtt_ms.is_some_and(|ms| ms >= 100),
        "delay_150ms should populate EWMA rtt_ms: {fast_rec:?}"
    );
    hung_task.abort();
    let _ = hung_task.await;

    // 2. Drive `host-dead` through 3 consecutive failures (`Degraded -> Degraded -> Offline`)
    //    and confirm existing remote sessions remain visible (never marked `Ended`).
    dead.set_mode("exit255_always");
    let (st_p1, r_p1) = http_json(
        addr,
        "POST",
        "/api/hosts/host-dead/probe",
        Some("admin-secret-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_p1, 200, "{r_p1}");
    assert_eq!(r_p1["data"]["status"], "degraded", "{r_p1}");
    assert!(r_p1["data"]["next_retry_ms"].as_u64().is_some());

    let (st_p2, r_p2) = http_json(
        addr,
        "POST",
        "/api/hosts/host-dead/probe",
        Some("admin-secret-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_p2, 200, "{r_p2}");
    assert_eq!(r_p2["data"]["status"], "degraded", "{r_p2}");

    let (st_p3, r_p3) = http_json(
        addr,
        "POST",
        "/api/hosts/host-dead/probe",
        Some("admin-secret-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_p3, 200, "{r_p3}");
    assert_eq!(r_p3["data"]["status"], "offline", "{r_p3}");
    assert!(r_p3["data"]["next_retry_ms"].as_u64().is_some());

    let dead_sess = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.effective_host_id() == "host-dead")
        .expect("offline host sessions must stay visible");
    assert_ne!(dead_sess.state, SessionState::Ended);

    // 3. While `host-dead` is `Offline` (open circuit), silent background reads (`preview` &
    //    `tmux/sessions`) fast-fail in <20ms with 0 SSH calls!
    let calls_before_ff = dead.cmd_call_count();
    let enc_pane = format!("host-dead%3A%25{}", &p_dead.as_str()[1..]);
    let t_ff = Instant::now();
    let (st_pv, _) = http_json(
        addr,
        "GET",
        &format!("/api/panes/{enc_pane}/preview?lines=40"),
        Some("ro-viewer-tok"),
        None,
    )
    .await;
    let (st_ts, _) = http_json(
        addr,
        "GET",
        "/api/tmux/sessions?host=host-dead",
        Some("ro-viewer-tok"),
        None,
    )
    .await;
    let ff_elapsed = t_ff.elapsed();
    assert_eq!(st_pv, 503);
    assert_eq!(st_ts, 503);
    assert!(
        ff_elapsed < Duration::from_millis(100),
        "offline circuit must fast-fail in ~0ms, took {ff_elapsed:?}"
    );
    assert_eq!(
        dead.cmd_call_count(),
        calls_before_ff,
        "offline fast-fail must spawn 0 SSH processes"
    );

    // 4. `POST /api/hosts/{id}/probe` enforces admin-write auth (`403` for read-only token, `404` unknown host).
    let (st_ro, _) = http_json(
        addr,
        "POST",
        "/api/hosts/host-dead/probe",
        Some("ro-viewer-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(
        st_ro, 403,
        "read-only token must get 403 Forbidden on probe"
    );

    let (st_unk, _) = http_json(
        addr,
        "POST",
        "/api/hosts/does-not-exist/probe",
        Some("admin-secret-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_unk, 404, "unknown host probe must return 404");

    // 5. Bring `host-dead` back online -> `POST /api/hosts/host-dead/probe` resets circuit,
    //    recovers immediately to `online`, and broadcasts SSE `event: host`!
    let mut sse = core.subscribe();
    dead.set_mode("normal");
    let (st_rec, r_rec) = http_json(
        addr,
        "POST",
        "/api/hosts/host-dead/probe",
        Some("admin-secret-tok"),
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_rec, 200, "{r_rec}");
    assert_eq!(r_rec["data"]["status"], "online", "{r_rec}");
    assert!(r_rec["data"]["rtt_ms"].as_u64().is_some());

    let mut saw_online_sse = false;
    while let Ok(ev) = sse.try_recv() {
        if let DaemonEvent::Host { host, .. } = ev {
            if host.id.as_str() == "host-dead" && host.status == HostStatus::Online {
                saw_online_sse = true;
            }
        }
    }
    assert!(
        saw_online_sse,
        "probe recovery must broadcast SSE event: host"
    );

    srv.abort();
}

// ===========================================================================
// Test 9: MCP `spawn_session` with `host_id` (verifying `tools/list` schema
// and zero local `CallerEnv.cwd` leak onto remote SSH host).
// ===========================================================================
struct CoreBridgeTransport {
    core: Core,
    rt: tokio::runtime::Handle,
}

impl Transport for CoreBridgeTransport {
    fn call(&mut self, cmd: DaemonCommand) -> Result<Value, CallError> {
        let core = self.core.clone();
        tokio::task::block_in_place(|| self.rt.block_on(core.handle(cmd))).map_err(CallError::Api)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_09_mcp_spawn_session_with_host_id_no_local_cwd_leak() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t9");
    let (hub_dir, _hub_guard) = mk_hub_dir("t9");

    let core = build_hub_core(
        &hub_dir.join("state"),
        vec![remote.node_config("remote-mcp", "Remote MCP")],
        WebConfig::default(),
    );

    let mut server = McpServer::new(
        AgentKind::Codex,
        CallerEnv {
            cwd: Some("/local/caller/must/never/leak".into()),
            pane: Some(PaneId::new("%1")),
            bind_token: None,
            pid_chain: Vec::new(),
        },
        CoreBridgeTransport {
            core: core.clone(),
            rt: tokio::runtime::Handle::current(),
        },
    );

    // Verify `tools/list` exposes `host_id` on `spawn_session`.
    let list_resp = server
        .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .unwrap();
    let spawn_tool = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "spawn_session")
        .expect("spawn_session tool listed");
    assert!(
        spawn_tool["inputSchema"]["properties"]["host_id"].is_object(),
        "spawn_session schema must include host_id"
    );

    // Call `spawn_session(agent="claude", host_id="remote-mcp")` WITHOUT `project_root`.
    let call_resp = server
        .handle_line(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "spawn_session",
                    "arguments": {
                        "agent": "claude",
                        "host_id": "remote-mcp"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    let result = &call_resp["result"];
    assert_eq!(result["isError"], false, "{call_resp}");
    let sc = &result["structuredContent"];
    assert_eq!(sc["host_id"], "remote-mcp");
    let expected_remote_cwd = remote
        .remote_home
        .join(".agent_session")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        sc["cwd"].as_str(),
        Some(expected_remote_cwd.as_str()),
        "MCP remote spawn must expand remote ~/.agent_session and never leak local CallerEnv.cwd"
    );
    assert!(
        !sc["cwd"].as_str().unwrap().contains("/local/caller/"),
        "{sc}"
    );
    assert!(
        !remote.tmux.list_panes().unwrap().is_empty(),
        "pane must exist in remote sandbox tmux server"
    );
}

// ===========================================================================
// Test 10: Remote `close_pane` (`POST /api/sessions/<enc>/close` kills remote
// pane, evicts discovery cache, marks `Ended`, broadcasts SSE).
// ===========================================================================
#[tokio::test]
async fn test_10_remote_close_pane_kills_evicts_marks_ended_and_broadcasts_sse() {
    if !has_tmux() {
        return;
    }
    let remote = FakeSshHost::new("t10");
    let (hub_dir, _hub_guard) = mk_hub_dir("t10");

    let (core, addr, srv) = start_hub_with_web(
        &hub_dir.join("state"),
        vec![remote.node_config("remote-cls", "Remote Close")],
        WebConfig {
            auth_enabled: false,
            ..WebConfig::default()
        },
    )
    .await;

    let (st_sp, r_sp) = http_json(
        addr,
        "POST",
        "/api/sessions/spawn",
        None,
        Some(&json!({
            "agent": "codex",
            "host_id": "remote-cls"
        })),
    )
    .await;
    assert_eq!(st_sp, 200, "{r_sp}");
    let sid = r_sp["data"]["session_id"].as_str().unwrap().to_string();
    let enc_sid = sid.replace('%', "%25").replace(':', "%3A");

    core.sync_remote_host(&HostId::new("remote-cls")).await;
    assert_eq!(remote.tmux.list_panes().unwrap().len(), 1);

    let mut sse = core.subscribe();
    let (st_cl, r_cl) = http_json(
        addr,
        "POST",
        &format!("/api/sessions/{enc_sid}/close"),
        None,
        Some(&json!({})),
    )
    .await;
    assert_eq!(st_cl, 200, "{r_cl}");
    assert_eq!(r_cl["data"]["closed"], true);
    assert_eq!(r_cl["data"]["session_id"], sid);

    // 1. Remote tmux pane is actually killed.
    assert!(
        remote.tmux.list_panes().unwrap().is_empty(),
        "remote tmux pane must be killed by close_pane"
    );

    // 2. Hub `State` marks session `Ended`.
    let rec = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == sid)
        .expect("closed session present as Ended");
    assert_eq!(rec.state, SessionState::Ended);

    // 3. SSE broadcast `DaemonEvent::Session` with `Ended` state.
    let mut saw_ended_sse = false;
    while let Ok(ev) = sse.try_recv() {
        if let DaemonEvent::Session { session, .. } = ev {
            if session.session_id.as_str() == sid && session.state == SessionState::Ended {
                saw_ended_sse = true;
            }
        }
    }
    assert!(
        saw_ended_sse,
        "remote close must broadcast SSE session Ended"
    );

    // 4. Subsequent sync tick must NOT resurrect the evicted closed pane!
    core.sync_remote_host(&HostId::new("remote-cls")).await;
    let rec_after = core
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.session_id.as_str() == sid)
        .unwrap();
    assert_eq!(rec_after.state, SessionState::Ended);

    srv.abort();
}
