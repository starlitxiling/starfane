//! Ultra-thin synchronous hook emitter hot path (`std`-only + `serde_json`).
//!
//! Contracts §2:
//! - Never constructs `tokio`, never touches the network, never panics.
//! - Always exits with code `0`.
//! - Writes `"{}\n"` to stdout for `AgentKind::Agy`, empty string for Claude / Codex.

use starfane_proto::{
    encode_line,
    id::now_ms,
    paths::{self, ENV_DISABLE},
    AgentKind, ClientFrame, EmitRequest, EnvCtx, PROTO_VERSION,
};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Hard cap on hook stdin size (1 MiB, contracts §2).
const MAX_STDIN_BYTES: u64 = 1024 * 1024;
/// Socket write timeout (contracts §2).
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_millis(50);
/// Minimum interval between lazy `starfane daemon --detach` spawn attempts (contracts §2).
const SPAWN_COOLDOWN: Duration = Duration::from_secs(2);

static SPOOL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Required stdout hook reply per agent (`agy` requires valid JSON on stdout).
pub fn hook_reply(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Agy => "{}\n",
        AgentKind::Claude | AgentKind::Codex | AgentKind::Shell => "",
    }
}

/// Hook hot path entry point (contracts §2). Always returns `0`.
pub fn run_emit<R: Read, W: Write>(
    agent: AgentKind,
    event: Option<&str>,
    stdin: R,
    stdout: W,
) -> i32 {
    run_emit_with(
        agent,
        event,
        EnvCtx::capture(),
        &paths::socket_path(),
        &paths::spool_dir(),
        stdin,
        stdout,
    )
}

/// Testable entry point with injected environment, socket, and spool paths (contracts §2).
pub fn run_emit_with<R: Read, W: Write>(
    agent: AgentKind,
    event: Option<&str>,
    env: EnvCtx,
    socket: &Path,
    spool: &Path,
    stdin: R,
    mut stdout: W,
) -> i32 {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        run_emit_inner(agent, event, env, socket, spool, stdin);
    }));

    let reply = hook_reply(agent);
    if !reply.is_empty() {
        let _ = stdout.write_all(reply.as_bytes());
        let _ = stdout.flush();
    }
    0
}

/// Compatibility helper kept for M0 skeleton callers until `bin/starfane` wires `run_emit`.
#[doc(hidden)]
pub fn run_emit_placeholder() -> i32 {
    0
}

fn run_emit_inner<R: Read>(
    agent: AgentKind,
    event: Option<&str>,
    env: EnvCtx,
    socket: &Path,
    spool: &Path,
    stdin: R,
) {
    if std::env::var(ENV_DISABLE).ok().as_deref() == Some("1") {
        return;
    }

    let payload = read_payload(stdin);
    let recv_ts_ms = now_ms();
    let req = EmitRequest {
        v: PROTO_VERSION,
        agent,
        hook_event: event.map(str::to_string),
        payload,
        env,
        recv_ts_ms,
    };

    if send_over_uds(socket, &req).is_err() {
        let _ = write_spool(spool, &req);
        try_autostart_daemon(spool);
    }
}

fn read_payload<R: Read>(stdin: R) -> serde_json::Value {
    let mut buf = Vec::new();
    let _ = stdin.take(MAX_STDIN_BYTES).read_to_end(&mut buf);

    let trimmed = trim_ascii_whitespace(&buf);
    if trimmed.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }

    match serde_json::from_slice::<serde_json::Value>(trimmed) {
        Ok(val) => val,
        Err(_) => {
            let raw = String::from_utf8_lossy(trimmed).into_owned();
            let mut map = serde_json::Map::new();
            map.insert("_raw".to_string(), serde_json::Value::String(raw));
            serde_json::Value::Object(map)
        }
    }
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

fn send_over_uds(socket: &Path, req: &EmitRequest) -> std::io::Result<()> {
    let frame = ClientFrame::Emit(req.clone());
    let line = encode_line(&frame).map_err(std::io::Error::other)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn write_spool(spool_dir: &Path, req: &EmitRequest) -> std::io::Result<PathBuf> {
    fs::create_dir_all(spool_dir)?;
    let line = encode_line(req).map_err(std::io::Error::other)?;

    let pid = std::process::id();
    let ts = req.recv_ts_ms;
    let seq = SPOOL_SEQ.fetch_add(1, Ordering::Relaxed);

    let tmp_name = format!(".tmp-{pid}-{ts}-{seq}.jsonl");
    let final_name = if seq == 0 {
        format!("{pid}-{ts}.jsonl")
    } else {
        format!("{pid}-{ts}-{seq}.jsonl")
    };

    let tmp_path = spool_dir.join(tmp_name);
    let final_path = spool_dir.join(final_name);

    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
    }

    if fs::rename(&tmp_path, &final_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        let fallback = spool_dir.join(format!("{pid}-{ts}-{seq}.jsonl"));
        fs::write(&fallback, line.as_bytes())?;
        return Ok(fallback);
    }
    Ok(final_path)
}

fn try_autostart_daemon(spool_dir: &Path) {
    let lock_path = spool_dir.parent().unwrap_or(spool_dir).join("spawn.lock");

    if let Ok(meta) = fs::metadata(&lock_path) {
        if let Ok(modified) = meta.modified() {
            if let Ok(elapsed) = SystemTime::now().duration_since(modified) {
                if elapsed < SPAWN_COOLDOWN {
                    return;
                }
            }
        }
    }

    // Determine daemon executable: allow explicit override for tests, otherwise only
    // auto-spawn when the current binary is actually `starfane` (never fork a cargo test runner).
    let exe = match std::env::var_os("STARFANE_DAEMON_BIN") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => match std::env::current_exe() {
            Ok(p) if p.file_stem().and_then(|s| s.to_str()) == Some("starfane") => p,
            _ => return,
        },
    };

    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&lock_path, format!("{}\n", std::process::id()));

    let _ = Command::new(exe)
        .args(["daemon", "--detach"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Cursor};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sf-{name}-{}-{}", std::process::id(), now_ms()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hook_reply_matches_contract() {
        assert_eq!(hook_reply(AgentKind::Agy), "{}\n");
        assert_eq!(hook_reply(AgentKind::Claude), "");
        assert_eq!(hook_reply(AgentKind::Codex), "");
    }

    #[test]
    fn emits_over_uds_and_replies_correctly() {
        let dir = temp_dir("uds");
        let sock = dir.join("s.sock");
        let spool = dir.join("spool");
        let listener = UnixListener::bind(&sock).unwrap();

        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line
        });

        let input = r#"{"hookEventName":"Stop","conversationId":"c-1","fullyIdle":true}"#;
        let mut out = Vec::new();
        let code = run_emit_with(
            AgentKind::Agy,
            Some("Stop"),
            EnvCtx {
                tmux_pane: Some("%9".into()),
                ..EnvCtx::default()
            },
            &sock,
            &spool,
            Cursor::new(input.as_bytes()),
            &mut out,
        );

        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), "{}\n");

        let received = handle.join().unwrap();
        let frame: ClientFrame = serde_json::from_str(received.trim()).unwrap();
        match frame {
            ClientFrame::Emit(req) => {
                assert_eq!(req.v, PROTO_VERSION);
                assert_eq!(req.agent, AgentKind::Agy);
                assert_eq!(req.hook_event.as_deref(), Some("Stop"));
                assert_eq!(req.env.tmux_pane.as_deref(), Some("%9"));
                assert_eq!(req.payload["conversationId"], "c-1");
            }
            _ => panic!("expected ClientFrame::Emit"),
        }
        assert!(
            !spool.exists(),
            "spool must not be written when UDS succeeds"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_spool_when_daemon_offline() {
        let dir = temp_dir("spool");
        let sock = dir.join("missing.sock");
        let spool = dir.join("spool");

        let input = r#"{"hook_event_name":"Stop","session_id":"s-1"}"#;
        let mut out = Vec::new();
        let code = run_emit_with(
            AgentKind::Claude,
            Some("Stop"),
            EnvCtx::default(),
            &sock,
            &spool,
            Cursor::new(input.as_bytes()),
            &mut out,
        );

        assert_eq!(code, 0);
        assert_eq!(out, b"");

        let entries: Vec<_> = fs::read_dir(&spool)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let content = fs::read_to_string(entries[0].path()).unwrap();
        let req: EmitRequest = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(req.agent, AgentKind::Claude);
        assert_eq!(req.payload["session_id"], "s-1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wraps_invalid_json_stdin_into_raw_field() {
        let dir = temp_dir("invalid");
        let sock = dir.join("missing.sock");
        let spool = dir.join("spool");

        let mut out = Vec::new();
        let code = run_emit_with(
            AgentKind::Agy,
            None,
            EnvCtx::default(),
            &sock,
            &spool,
            Cursor::new(b"not json at all\n"),
            &mut out,
        );

        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), "{}\n");

        let entries: Vec<_> = fs::read_dir(&spool)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let content = fs::read_to_string(entries[0].path()).unwrap();
        let req: EmitRequest = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(req.payload["_raw"], "not json at all");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_stdin_at_one_mib() {
        let dir = temp_dir("cap");
        let sock = dir.join("missing.sock");
        let spool = dir.join("spool");

        let huge = vec![b'x'; (MAX_STDIN_BYTES as usize) + 4096];
        let mut out = Vec::new();
        let code = run_emit_with(
            AgentKind::Codex,
            Some("Stop"),
            EnvCtx::default(),
            &sock,
            &spool,
            Cursor::new(huge),
            &mut out,
        );

        assert_eq!(code, 0);
        assert!(out.is_empty());

        let entries: Vec<_> = fs::read_dir(&spool)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let content = fs::read_to_string(entries[0].path()).unwrap();
        let req: EmitRequest = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(
            req.payload["_raw"].as_str().unwrap().len(),
            MAX_STDIN_BYTES as usize
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
