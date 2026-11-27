//! FD-isolated, binary-safe persistent SSH `/bin/sh` conduit and dual-lane pool.
//!
//! Eliminates per-call OpenSSH ControlMaster channel creation overhead by keeping up to two
//! persistent `ssh [ControlMaster args] -T <target> -- /bin/sh -s` lanes per host:
//! - [`ConduitLane::Interactive`]: low-latency `PING -> SF_PONG` RTT probes, interactive
//!   dispatch, and targeted pane captures.
//! - [`ConduitLane::Background`]: periodic discovery scripts and transcript delta syncs.
//!
//! Protocol pipes are isolated on FD 3 (request stream) and FD 4 (response stream) with loop
//! FD 0/1 pointed at `/dev/null`, and every command subshell closes `3<&- 4>&-` before `exec`ing
//! `/bin/sh` so no inner command can ever read conduit commands or corrupt response framing.

use crate::error::TmuxError;
use crate::exec::{ExecBytesOutput, ExecOutput, ExecTransport, SshExec};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// ASCII Record Separator (`0x1e`) prefixing every conduit response header line.
const FRAME_PREFIX: u8 = 0x1e;
/// Protocol version announced in `\x1eSF_READY <ver> <stat_mode> <remote_home>\n`.
const CONDUIT_PROTO_VERSION: &str = "1";
/// Maximum non-protocol banner lines skipped before `SF_READY` (e.g. chatty login profiles).
const MAX_HANDSHAKE_BANNER_LINES: usize = 64;
/// Upper bound on any single framing header line (bytes) to defend against runaway output.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
/// Upper bound on a single `SF_RES` stdout or stderr payload (64 MiB).
const MAX_CONDUIT_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Upper bound on captured child stderr bytes kept in memory for diagnostics.
const MAX_STDERR_CAPTURE_BYTES: usize = 8 * 1024;
/// Graceful wait window when sending `QUIT` or reaping an exited child.
const TEARDOWN_WAIT: Duration = Duration::from_millis(200);
/// Consecutive `SF_READY` handshake failures before engaging one-shot `SshExec` fallback.
pub const CONDUIT_HANDSHAKE_FAILURE_THRESHOLD: u32 = 2;
/// Duration of one-shot `SshExec` fallback cooldown after repeated `SF_READY` failures.
pub const CONDUIT_FALLBACK_COOLDOWN: Duration = Duration::from_secs(600);

/// FD-isolated POSIX `/bin/sh` conduit loop.
///
/// Launched by piping `exec /bin/sh -c '<quoted>'` into `ssh ... -T <target> -- /bin/sh -s`:
/// - Passing `/bin/sh -s` on the SSH command line avoids all remote login shell quoting quirks
///   (`fish`, `csh`, `tcsh`, `nu`, etc.).
/// - Immediately `exec`ing `/bin/sh -c '...'` replaces `-s` in-place so the loop runs from a
///   string script instead of a buffered `stdin` script stream — avoiding a macOS `/bin/sh`
///   (bash 3.2) `st_bstream` SIGABRT bug after reading > 1 KiB from duplicated FD 3.
/// - Inside each `EXEC` subshell we run `(exec 3<&- 4>&-; exec /bin/sh "$SF_RUN/cmd.sh" ...)`
///   to close conduit FDs cleanly and save an extra fork.
/// - `SF_RUN` is never exported and is removed via `trap` on exit/signal.
const CONDUIT_LOOP_BODY: &str = r#"exec 3<&0 4>&1 0</dev/null 1>/dev/null
umask 077
SF_RUN=$(mktemp -d "${TMPDIR:-/tmp}/sf-c.XXXXXX") || exit 11
trap 'rm -rf "$SF_RUN"' EXIT HUP INT TERM PIPE 2>/dev/null || trap 'rm -rf "$SF_RUN"' EXIT HUP INT TERM
for _d in "$HOME/.local/bin" "$HOME/.cargo/bin" "$HOME/.npm-global/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
  [ -d "$_d" ] && case ":$PATH:" in *":$_d:"*) ;; *) PATH="$PATH:$_d" ;; esac
done
export PATH
if stat -c '%s' "$SF_RUN" >/dev/null 2>&1; then SF_STAT_MODE=gnu
elif stat -f '%z' "$SF_RUN" >/dev/null 2>&1; then SF_STAT_MODE=bsd
else SF_STAT_MODE=unknown; fi
export SF_STAT_MODE
SF_B64_DEC=""
if [ "$(printf 'b2s=' | base64 -d 2>/dev/null)" = "ok" ]; then SF_B64_DEC="base64 -d"
elif [ "$(printf 'b2s=' | base64 -D 2>/dev/null)" = "ok" ]; then SF_B64_DEC="base64 -D"
elif [ "$(printf 'b2s=' | openssl base64 -d -A 2>/dev/null)" = "ok" ]; then SF_B64_DEC="openssl base64 -d -A"
else exit 12; fi
printf '\036SF_READY 1 %s %s\n' "$SF_STAT_MODE" "$HOME" >&4
while IFS=' ' read -r _op _seq _sb64 _ib64 <&3; do
  case "$_op" in
    "") continue ;;
    QUIT) break ;;
    PING) printf '\036SF_PONG %s\n' "$_seq" >&4 ;;
    EXEC)
      : >"$SF_RUN/out"
      : >"$SF_RUN/err"
      _dec_ok=1
      if [ "$_sb64" = "-" ]; then : >"$SF_RUN/cmd.sh"
      else printf '%s\n' "$_sb64" | $SF_B64_DEC >"$SF_RUN/cmd.sh" 2>"$SF_RUN/err" || _dec_ok=0; fi
      if [ "$_dec_ok" -eq 1 ]; then
        if [ "$_ib64" = "-" ] || [ -z "$_ib64" ]; then _in=/dev/null
        else printf '%s\n' "$_ib64" | $SF_B64_DEC >"$SF_RUN/in" 2>"$SF_RUN/err" || _dec_ok=0; _in="$SF_RUN/in"; fi
      fi
      if [ "$_dec_ok" -eq 1 ]; then
        (exec 3<&- 4>&-; exec /bin/sh "$SF_RUN/cmd.sh" <"$_in" >"$SF_RUN/out" 2>"$SF_RUN/err")
        _rc=$?
      else
        _rc=125
      fi
      case "$SF_STAT_MODE" in
        gnu) set -- $(stat -c '%s' "$SF_RUN/out" "$SF_RUN/err"); _olen=${1:-0}; _elen=${2:-0} ;;
        bsd) set -- $(stat -f '%z' "$SF_RUN/out" "$SF_RUN/err"); _olen=${1:-0}; _elen=${2:-0} ;;
        *) _olen=$(wc -c <"$SF_RUN/out" | tr -d ' '); _elen=$(wc -c <"$SF_RUN/err" | tr -d ' ') ;;
      esac
      printf '\036SF_RES %s %s %s %s\n' "$_seq" "$_rc" "$_olen" "$_elen" >&4
      [ "$_olen" -gt 0 ] && cat "$SF_RUN/out" >&4
      [ "$_elen" -gt 0 ] && cat "$SF_RUN/err" >&4
      ;;
  esac
done
exit 0"#;

fn build_bootstrap_launcher() -> String {
    let mut quoted = String::with_capacity(CONDUIT_LOOP_BODY.len() + 64);
    quoted.push('\'');
    for ch in CONDUIT_LOOP_BODY.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    format!("exec /bin/sh -c {quoted}\n")
}

/// Remote host `stat` CLI flavor detected once during conduit bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteStatMode {
    /// macOS / FreeBSD (`stat -f`).
    Bsd,
    /// GNU coreutils (`stat -c`).
    Gnu,
    /// Fallback when neither `stat -c` nor `stat -f` is available.
    Unknown,
}

impl RemoteStatMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteStatMode::Bsd => "bsd",
            RemoteStatMode::Gnu => "gnu",
            RemoteStatMode::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "bsd" => RemoteStatMode::Bsd,
            "gnu" => RemoteStatMode::Gnu,
            _ => RemoteStatMode::Unknown,
        }
    }
}

/// Host metadata negotiated once on `\x1eSF_READY 1 <stat_mode> <remote_home>\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitMeta {
    pub stat_mode: RemoteStatMode,
    pub remote_home: String,
}

/// Dedicated persistent lane inside [`SshConduitPool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConduitLane {
    /// Fast lane for `PING` RTT probes, interactive dispatch, and targeted captures.
    Interactive,
    /// Background lane for periodic discovery scripts and transcript mirroring.
    Background,
}

struct ConduitSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_buf: Arc<std::sync::Mutex<Vec<u8>>>,
    stderr_task: Option<JoinHandle<()>>,
    meta: ConduitMeta,
}

impl ConduitSession {
    /// Reap or kill the child, returning `(exit_code, captured_stderr)`.
    async fn terminate_and_collect(self, force_kill: bool) -> (Option<i32>, String) {
        let ConduitSession {
            mut child,
            stdin,
            stderr_buf,
            stderr_task,
            ..
        } = self;
        drop(stdin);
        let status = if force_kill {
            let _ = child.kill().await;
            tokio::time::timeout(TEARDOWN_WAIT, child.wait())
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            match tokio::time::timeout(TEARDOWN_WAIT, child.wait()).await {
                Ok(Ok(s)) => Some(s),
                _ => {
                    let _ = child.kill().await;
                    tokio::time::timeout(TEARDOWN_WAIT, child.wait())
                        .await
                        .ok()
                        .and_then(Result::ok)
                }
            }
        };
        if let Some(handle) = stderr_task {
            let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
        }
        let stderr = {
            let guard = stderr_buf.lock().unwrap_or_else(|p| p.into_inner());
            String::from_utf8_lossy(&guard).trim().to_string()
        };
        (status.and_then(|s| s.code()), stderr)
    }

    async fn close_gracefully(mut self) {
        let _ = tokio::time::timeout(TEARDOWN_WAIT, async {
            self.stdin.write_all(b"QUIT\n").await?;
            self.stdin.flush().await
        })
        .await;
        let _ = self.terminate_and_collect(false).await;
    }
}

enum ConduitCallError {
    /// Failed before `\x1eSF_READY 1 ...` completed (e.g. restricted shell / missing `base64`).
    HandshakeFailed {
        /// True when the timeout budget was not exhausted and immediate `SshExec` fallback is safe.
        can_fallback: bool,
        err: TmuxError,
    },
    /// Transport/timeout/framing error unrelated to restricted-shell incompatibility.
    Other(TmuxError),
}

impl ConduitCallError {
    fn into_tmux_error(self) -> TmuxError {
        match self {
            ConduitCallError::HandshakeFailed { err, .. } | ConduitCallError::Other(err) => err,
        }
    }
}

/// Single persistent, FD-isolated SSH `/bin/sh` conduit lane.
///
/// Lazily spawns `ssh [ControlMaster args] -T <target> -- /bin/sh -s` on first use and reuses
/// the same remote shell across subsequent [`PersistentSshConduit::ping`] and
/// [`PersistentSshConduit::exec_bytes`] calls. On timeout, EOF, or framing error, the child is
/// killed cleanly so the next call respawns a fresh session.
#[derive(Debug)]
pub struct PersistentSshConduit {
    ssh: SshExec,
    session: AsyncMutex<Option<ConduitSession>>,
    meta_cache: std::sync::Mutex<Option<ConduitMeta>>,
    next_seq: AtomicU64,
}

impl std::fmt::Debug for ConduitSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConduitSession")
            .field("pid", &self.child.id())
            .field("meta", &self.meta)
            .finish()
    }
}

impl PersistentSshConduit {
    pub fn new(ssh: SshExec) -> Self {
        Self {
            ssh,
            session: AsyncMutex::new(None),
            meta_cache: std::sync::Mutex::new(None),
            next_seq: AtomicU64::new(1),
        }
    }

    pub fn ssh(&self) -> &SshExec {
        &self.ssh
    }

    /// Last negotiated [`ConduitMeta`] for this lane (`None` before first successful handshake).
    pub async fn meta(&self) -> Option<ConduitMeta> {
        self.meta_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// True if a live child session is currently parked in this lane.
    pub async fn is_connected(&self) -> bool {
        self.session.lock().await.is_some()
    }

    /// Cleanly shut down the persistent child (`QUIT\n`, falling back to kill).
    pub async fn close(&self) {
        let existing = self.session.lock().await.take();
        if let Some(sess) = existing {
            sess.close_gracefully().await;
        }
    }

    /// Execute a zero-fork `PING <seq> -> SF_PONG <seq>` round-trip probe and record the
    /// pure transport latency sample into [`SshExec::rtt_ms`].
    pub async fn ping(&self, timeout: Duration) -> Result<u64, TmuxError> {
        self.ping_detailed(timeout)
            .await
            .map_err(ConduitCallError::into_tmux_error)
    }

    /// Execute a POSIX shell `script` with optional binary `stdin`, returning raw `stdout` bytes,
    /// lossy UTF-8 `stderr`, and exit status.
    pub async fn exec_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        self.exec_bytes_detailed(script, stdin, timeout)
            .await
            .map_err(ConduitCallError::into_tmux_error)
    }

    /// Convenience wrapper over [`PersistentSshConduit::exec_bytes`] returning UTF-8 lossy stdout.
    pub async fn exec_text(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, TmuxError> {
        self.exec_bytes(script, stdin, timeout)
            .await
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    async fn ping_detailed(&self, timeout: Duration) -> Result<u64, ConduitCallError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.session.lock().await;
        // Take ownership out of `guard` before any awaitable I/O so future cancellation or
        // timeout drops the session (`kill_on_drop(true)`) instead of leaving a desynced pipe.
        let mut sess = match guard.take() {
            Some(s) => s,
            None => self.spawn_and_handshake(deadline, timeout).await?,
        };

        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let remaining = remaining_until(deadline);
        if remaining.is_zero() {
            let _ = sess.terminate_and_collect(true).await;
            clear_stale_control_socket_async(&self.ssh).await;
            return Err(ConduitCallError::Other(TmuxError::Timeout {
                args: format!("ssh conduit ping {}", self.ssh.target()),
                secs: timeout.as_secs(),
            }));
        }

        let started = Instant::now();
        let req = format!("PING {seq}\n");
        let ping_io = async {
            sess.stdin
                .write_all(req.as_bytes())
                .await
                .map_err(|source| TmuxError::Io {
                    what: format!("conduit write PING ({})", self.ssh.target()),
                    source,
                })?;
            sess.stdin.flush().await.map_err(|source| TmuxError::Io {
                what: format!("conduit flush PING ({})", self.ssh.target()),
                source,
            })?;
            let line = read_line_bounded(&mut sess.stdout, MAX_HEADER_LINE_BYTES)
                .await
                .map_err(|source| TmuxError::Io {
                    what: format!("conduit read SF_PONG ({})", self.ssh.target()),
                    source,
                })?;
            let Some(line) = line else {
                return Err(TmuxError::Parse(
                    "unexpected EOF waiting for SF_PONG".into(),
                ));
            };
            parse_pong_header(&line, seq)
        };

        match tokio::time::timeout(remaining, ping_io).await {
            Ok(Ok(())) => {
                let sample_ms = started.elapsed().as_millis().min(u64::MAX as u128 - 1) as u64;
                self.ssh.record_rtt_sample(sample_ms);
                *guard = Some(sess);
                Ok(sample_ms)
            }
            Ok(Err(err)) => {
                let (code, stderr) = sess.terminate_and_collect(false).await;
                if code == Some(255) {
                    clear_stale_control_socket_async(&self.ssh).await;
                }
                Err(ConduitCallError::Other(enrich_conduit_error(
                    err,
                    code,
                    &stderr,
                    self.ssh.target(),
                )))
            }
            Err(_) => {
                // Zero-fork PING timed out: transport/ControlMaster is wedged or half-open.
                let _ = sess.terminate_and_collect(true).await;
                clear_stale_control_socket_async(&self.ssh).await;
                Err(ConduitCallError::Other(TmuxError::Timeout {
                    args: format!("ssh conduit ping {}", self.ssh.target()),
                    secs: timeout.as_secs(),
                }))
            }
        }
    }

    async fn exec_bytes_detailed(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, ConduitCallError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.session.lock().await;
        let mut sess = match guard.take() {
            Some(s) => s,
            None => self.spawn_and_handshake(deadline, timeout).await?,
        };

        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let remaining = remaining_until(deadline);
        if remaining.is_zero() {
            let (code, _) = sess.terminate_and_collect(true).await;
            if code == Some(255) {
                clear_stale_control_socket_async(&self.ssh).await;
            }
            return Err(ConduitCallError::Other(TmuxError::Timeout {
                args: format!("ssh conduit exec {}", self.ssh.target()),
                secs: timeout.as_secs(),
            }));
        }

        let script_b64 = if script.is_empty() {
            "-".to_string()
        } else {
            base64_encode(script.as_bytes())
        };
        let stdin_b64 = match stdin {
            Some(bytes) if !bytes.is_empty() => base64_encode(bytes),
            _ => "-".to_string(),
        };
        let frame = format!("EXEC {seq} {script_b64} {stdin_b64}\n");

        let exec_io = async {
            sess.stdin
                .write_all(frame.as_bytes())
                .await
                .map_err(|source| TmuxError::Io {
                    what: format!("conduit write EXEC ({})", self.ssh.target()),
                    source,
                })?;
            sess.stdin.flush().await.map_err(|source| TmuxError::Io {
                what: format!("conduit flush EXEC ({})", self.ssh.target()),
                source,
            })?;
            let line = read_line_bounded(&mut sess.stdout, MAX_HEADER_LINE_BYTES)
                .await
                .map_err(|source| TmuxError::Io {
                    what: format!("conduit read SF_RES ({})", self.ssh.target()),
                    source,
                })?;
            let Some(line) = line else {
                return Err(TmuxError::Parse("unexpected EOF waiting for SF_RES".into()));
            };
            let (rc, out_len, err_len) = parse_res_header(&line, seq)?;
            let mut stdout = vec![0u8; out_len];
            if out_len > 0 {
                sess.stdout
                    .read_exact(&mut stdout)
                    .await
                    .map_err(|source| TmuxError::Io {
                        what: format!("conduit read stdout ({out_len}B)"),
                        source,
                    })?;
            }
            let mut stderr_raw = vec![0u8; err_len];
            if err_len > 0 {
                sess.stdout
                    .read_exact(&mut stderr_raw)
                    .await
                    .map_err(|source| TmuxError::Io {
                        what: format!("conduit read stderr ({err_len}B)"),
                        source,
                    })?;
            }
            Ok(ExecBytesOutput {
                stdout,
                stderr: String::from_utf8_lossy(&stderr_raw).into_owned(),
                exit_code: Some(rc),
            })
        };

        match tokio::time::timeout(remaining, exec_io).await {
            Ok(Ok(out)) => {
                *guard = Some(sess);
                Ok(out)
            }
            Ok(Err(err)) => {
                let (code, stderr) = sess.terminate_and_collect(false).await;
                if code == Some(255) {
                    clear_stale_control_socket_async(&self.ssh).await;
                }
                Err(ConduitCallError::Other(enrich_conduit_error(
                    err,
                    code,
                    &stderr,
                    self.ssh.target(),
                )))
            }
            Err(_) => {
                // Only kill this lane's child; keep ControlMaster alive unless child exited 255.
                let (code, _) = sess.terminate_and_collect(true).await;
                if code == Some(255) {
                    clear_stale_control_socket_async(&self.ssh).await;
                }
                Err(ConduitCallError::Other(TmuxError::Timeout {
                    args: format!("ssh conduit exec {}", self.ssh.target()),
                    secs: timeout.as_secs(),
                }))
            }
        }
    }

    async fn spawn_and_handshake(
        &self,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<ConduitSession, ConduitCallError> {
        let mut args = self.ssh.build_ssh_base_args();
        args.push("-T".to_string());
        args.push(self.ssh.target().to_string());
        args.push("--".to_string());
        args.push("/bin/sh -s".to_string());

        let mut cmd = Command::new(self.ssh.ssh_bin());
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|source| {
            ConduitCallError::Other(TmuxError::Io {
                what: format!("spawn ssh conduit ({})", self.ssh.ssh_bin().display()),
                source,
            })
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ConduitCallError::Other(TmuxError::Io {
                what: "conduit stdin pipe missing".into(),
                source: std::io::Error::other("missing stdin pipe"),
            })
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ConduitCallError::Other(TmuxError::Io {
                what: "conduit stdout pipe missing".into(),
                source: std::io::Error::other("missing stdout pipe"),
            })
        })?;
        let stderr_pipe = child.stderr.take();
        let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_task = stderr_pipe.map(|mut pipe| {
            let buf_ref = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut tmp = [0u8; 1024];
                loop {
                    match pipe.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut guard = buf_ref.lock().unwrap_or_else(|p| p.into_inner());
                            let avail = MAX_STDERR_CAPTURE_BYTES.saturating_sub(guard.len());
                            if avail > 0 {
                                guard.extend_from_slice(&tmp[..n.min(avail)]);
                            }
                        }
                    }
                }
            })
        });

        let mut sess = ConduitSession {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_buf,
            stderr_task,
            meta: ConduitMeta {
                stat_mode: RemoteStatMode::Unknown,
                remote_home: String::new(),
            },
        };

        let remaining = remaining_until(deadline);
        if remaining.is_zero() {
            let _ = sess.terminate_and_collect(true).await;
            clear_stale_control_socket_async(&self.ssh).await;
            return Err(ConduitCallError::HandshakeFailed {
                can_fallback: false,
                err: TmuxError::Timeout {
                    args: format!("ssh conduit handshake {}", self.ssh.target()),
                    secs: timeout.as_secs(),
                },
            });
        }

        let bootstrap_launcher = build_bootstrap_launcher();
        let handshake = async {
            // BrokenPipe on write can occur if remote shell rejected `/bin/sh -s` immediately;
            // still attempt to read stdout/reap exit code so we report accurate diagnostics.
            if let Err(e) = sess.stdin.write_all(bootstrap_launcher.as_bytes()).await {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(TmuxError::Io {
                        what: format!("conduit write bootstrap ({})", self.ssh.target()),
                        source: e,
                    });
                }
            }
            if let Err(e) = sess.stdin.flush().await {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(TmuxError::Io {
                        what: format!("conduit flush bootstrap ({})", self.ssh.target()),
                        source: e,
                    });
                }
            }

            for _ in 0..=MAX_HANDSHAKE_BANNER_LINES {
                let line = read_line_bounded(&mut sess.stdout, MAX_HEADER_LINE_BYTES)
                    .await
                    .map_err(|source| TmuxError::Io {
                        what: format!("conduit read SF_READY ({})", self.ssh.target()),
                        source,
                    })?;
                let Some(line) = line else {
                    return Err(TmuxError::Parse(
                        "unexpected EOF before SF_READY handshake".into(),
                    ));
                };
                if line.first().copied() == Some(FRAME_PREFIX) {
                    return parse_ready_header(&line);
                }
            }
            Err(TmuxError::Parse(format!(
                "exceeded {MAX_HANDSHAKE_BANNER_LINES} banner lines without SF_READY"
            )))
        };

        match tokio::time::timeout(remaining, handshake).await {
            Ok(Ok(meta)) => {
                *self.meta_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(meta.clone());
                sess.meta = meta;
                Ok(sess)
            }
            Ok(Err(err)) => {
                let (code, stderr) = sess.terminate_and_collect(false).await;
                if code == Some(255) {
                    // SSH transport/network/auth failure — do NOT classify as restricted shell.
                    clear_stale_control_socket_async(&self.ssh).await;
                    return Err(ConduitCallError::Other(enrich_conduit_error(
                        err,
                        code,
                        &stderr,
                        self.ssh.target(),
                    )));
                }
                Err(ConduitCallError::HandshakeFailed {
                    can_fallback: true,
                    err: enrich_conduit_error(err, code, &stderr, self.ssh.target()),
                })
            }
            Err(_) => {
                let (code, _) = sess.terminate_and_collect(true).await;
                clear_stale_control_socket_async(&self.ssh).await;
                let err = TmuxError::Timeout {
                    args: format!("ssh conduit handshake {}", self.ssh.target()),
                    secs: timeout.as_secs(),
                };
                if code == Some(255) {
                    Err(ConduitCallError::Other(err))
                } else {
                    Err(ConduitCallError::HandshakeFailed {
                        can_fallback: false,
                        err,
                    })
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct FallbackState {
    consecutive_handshake_failures: u32,
    fallback_until: Option<Instant>,
}

/// Dual-lane persistent SSH conduit pool with automatic restricted-shell fallback.
///
/// Maintains two independent persistent lanes ([`ConduitLane::Interactive`] and
/// [`ConduitLane::Background`]) sharing one OpenSSH ControlMaster socket (`2` of `MaxSessions 10`).
/// If the remote host fails the `SF_READY` handshake twice consecutively (e.g. restricted shell
/// or missing `mktemp`/`base64`), transparently falls back to one-shot [`SshExec`] for 10 minutes.
#[derive(Debug)]
pub struct SshConduitPool {
    ssh: SshExec,
    lane_interactive: PersistentSshConduit,
    lane_background: PersistentSshConduit,
    fallback: std::sync::Mutex<FallbackState>,
    fallback_cooldown: Duration,
}

impl SshConduitPool {
    pub fn new(ssh: SshExec) -> Self {
        Self::with_fallback_cooldown(ssh, CONDUIT_FALLBACK_COOLDOWN)
    }

    pub fn with_fallback_cooldown(ssh: SshExec, fallback_cooldown: Duration) -> Self {
        let lane_interactive = PersistentSshConduit::new(ssh.clone());
        let lane_background = PersistentSshConduit::new(ssh.clone());
        Self {
            ssh,
            lane_interactive,
            lane_background,
            fallback: std::sync::Mutex::new(FallbackState::default()),
            fallback_cooldown,
        }
    }

    pub fn ssh(&self) -> &SshExec {
        &self.ssh
    }

    /// True when consecutive `SF_READY` handshake failures have engaged the one-shot `SshExec`
    /// fallback cooldown window.
    pub fn is_fallback_active(&self) -> bool {
        let mut st = self.fallback.lock().unwrap_or_else(|p| p.into_inner());
        match st.fallback_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                st.fallback_until = None;
                st.consecutive_handshake_failures = 0;
                false
            }
            None => false,
        }
    }

    fn record_handshake_success(&self) {
        let mut st = self.fallback.lock().unwrap_or_else(|p| p.into_inner());
        st.consecutive_handshake_failures = 0;
        st.fallback_until = None;
    }

    fn record_handshake_failure(&self) {
        let mut st = self.fallback.lock().unwrap_or_else(|p| p.into_inner());
        st.consecutive_handshake_failures = st.consecutive_handshake_failures.saturating_add(1);
        if st.consecutive_handshake_failures >= CONDUIT_HANDSHAKE_FAILURE_THRESHOLD {
            st.fallback_until = Some(Instant::now() + self.fallback_cooldown);
        }
    }

    fn lane(&self, lane: ConduitLane) -> &PersistentSshConduit {
        match lane {
            ConduitLane::Interactive => &self.lane_interactive,
            ConduitLane::Background => &self.lane_background,
        }
    }

    /// Measure pure transport RTT (`PING -> SF_PONG` on [`ConduitLane::Interactive`], or
    /// lightweight `SshExec::probe_rtt` when fallback mode is active) and update
    /// [`SshExec::rtt_ms`].
    pub async fn ping(&self, timeout: Duration) -> Result<u64, TmuxError> {
        if self.is_fallback_active() {
            return self.fallback_probe_rtt(timeout).await;
        }
        match self.lane_interactive.ping_detailed(timeout).await {
            Ok(ms) => {
                self.record_handshake_success();
                Ok(ms)
            }
            Err(ConduitCallError::HandshakeFailed { can_fallback, err }) => {
                self.record_handshake_failure();
                if can_fallback {
                    self.fallback_probe_rtt(timeout).await
                } else {
                    Err(err)
                }
            }
            Err(ConduitCallError::Other(err)) => Err(err),
        }
    }

    /// Execute a POSIX shell `script` with optional binary `stdin` on the requested `lane`,
    /// preserving raw `stdout` bytes.
    pub async fn exec_bytes(
        &self,
        lane: ConduitLane,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        if self.is_fallback_active() {
            return self.fallback_exec_bytes(script, stdin, timeout).await;
        }
        match self
            .lane(lane)
            .exec_bytes_detailed(script, stdin, timeout)
            .await
        {
            Ok(out) => {
                self.record_handshake_success();
                Ok(out)
            }
            Err(ConduitCallError::HandshakeFailed { can_fallback, err }) => {
                self.record_handshake_failure();
                if can_fallback {
                    self.fallback_exec_bytes(script, stdin, timeout).await
                } else {
                    Err(err)
                }
            }
            Err(ConduitCallError::Other(err)) => Err(err),
        }
    }

    /// Convenience wrapper over [`SshConduitPool::exec_bytes`] returning lossy UTF-8 stdout.
    pub async fn exec_text(
        &self,
        lane: ConduitLane,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, TmuxError> {
        self.exec_bytes(lane, script, stdin, timeout)
            .await
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    /// Return cached [`ConduitMeta`] negotiated by either lane (`None` before first handshake).
    pub async fn meta(&self) -> Option<ConduitMeta> {
        if let Some(m) = self.lane_interactive.meta().await {
            return Some(m);
        }
        self.lane_background.meta().await
    }

    /// Close both persistent lanes cleanly and reset any active fallback cooldown.
    pub async fn reset(&self) {
        tokio::join!(self.lane_interactive.close(), self.lane_background.close());
        let mut st = self.fallback.lock().unwrap_or_else(|p| p.into_inner());
        st.consecutive_handshake_failures = 0;
        st.fallback_until = None;
    }

    async fn fallback_probe_rtt(&self, timeout: Duration) -> Result<u64, TmuxError> {
        let ssh = self.ssh.clone();
        tokio::task::spawn_blocking(move || ssh.probe_rtt_with_timeout(timeout))
            .await
            .map_err(|e| TmuxError::Io {
                what: "conduit fallback probe_rtt join".into(),
                source: std::io::Error::other(e.to_string()),
            })?
    }

    async fn fallback_exec_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        let ssh = self.ssh.clone();
        let script_owned = script.to_string();
        let stdin_owned = stdin.map(<[u8]>::to_vec);
        tokio::task::spawn_blocking(move || {
            ssh.run_shell_bytes(&script_owned, stdin_owned.as_deref(), timeout)
        })
        .await
        .map_err(|e| TmuxError::Io {
            what: "conduit fallback exec_bytes join".into(),
            source: std::io::Error::other(e.to_string()),
        })?
    }
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn clear_stale_control_socket_async(ssh: &SshExec) {
    let ssh = ssh.clone();
    let _ = tokio::task::spawn_blocking(move || ssh.clear_stale_control_socket()).await;
}

fn enrich_conduit_error(
    err: TmuxError,
    exit_code: Option<i32>,
    stderr: &str,
    target: &str,
) -> TmuxError {
    if exit_code.is_some() || !stderr.is_empty() {
        let detail = if stderr.is_empty() {
            err.to_string()
        } else {
            format!("{err}; stderr: {stderr}")
        };
        TmuxError::Command {
            args: format!("ssh conduit {target}"),
            code: exit_code,
            stderr: detail,
        }
    } else {
        err
    }
}

async fn read_line_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(64);
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(out))
            };
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if out.len() + take > max_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("conduit header line exceeded {max_bytes} bytes"),
                ));
            }
            out.extend_from_slice(&avail[..take]);
            reader.consume(take);
            return Ok(Some(out));
        }
        let n = avail.len();
        if out.len() + n > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("conduit header line exceeded {max_bytes} bytes"),
            ));
        }
        out.extend_from_slice(avail);
        reader.consume(n);
    }
}

fn trim_header_line(raw: &[u8]) -> Result<&str, TmuxError> {
    let s = std::str::from_utf8(raw)
        .map_err(|e| TmuxError::Parse(format!("conduit header invalid utf-8: {e}")))?;
    Ok(s.trim_end_matches(['\n', '\r']))
}

fn parse_ready_header(raw: &[u8]) -> Result<ConduitMeta, TmuxError> {
    let line = trim_header_line(raw)?;
    let rest = line
        .strip_prefix("\x1eSF_READY ")
        .ok_or_else(|| TmuxError::Parse(format!("malformed SF_READY header: {line:?}")))?;
    let mut parts = rest.splitn(3, ' ');
    let ver = parts.next().unwrap_or("");
    let stat_raw = parts.next().unwrap_or("");
    let remote_home = parts.next().unwrap_or("").to_string();
    if ver != CONDUIT_PROTO_VERSION || stat_raw.is_empty() {
        return Err(TmuxError::Parse(format!(
            "unsupported SF_READY header: {line:?}"
        )));
    }
    Ok(ConduitMeta {
        stat_mode: RemoteStatMode::parse(stat_raw),
        remote_home,
    })
}

fn parse_pong_header(raw: &[u8], expected_seq: u64) -> Result<(), TmuxError> {
    let line = trim_header_line(raw)?;
    let seq_str = line
        .strip_prefix("\x1eSF_PONG ")
        .ok_or_else(|| TmuxError::Parse(format!("malformed SF_PONG header: {line:?}")))?;
    let got: u64 = seq_str
        .trim()
        .parse()
        .map_err(|e| TmuxError::Parse(format!("invalid SF_PONG seq {seq_str:?}: {e}")))?;
    if got != expected_seq {
        return Err(TmuxError::Parse(format!(
            "SF_PONG seq mismatch: expected {expected_seq}, got {got}"
        )));
    }
    Ok(())
}

fn parse_res_header(raw: &[u8], expected_seq: u64) -> Result<(i32, usize, usize), TmuxError> {
    let line = trim_header_line(raw)?;
    let rest = line
        .strip_prefix("\x1eSF_RES ")
        .ok_or_else(|| TmuxError::Parse(format!("malformed SF_RES header: {line:?}")))?;
    let mut parts = rest.split_whitespace();
    let seq_s = parts
        .next()
        .ok_or_else(|| TmuxError::Parse(format!("missing SF_RES seq: {line:?}")))?;
    let rc_s = parts
        .next()
        .ok_or_else(|| TmuxError::Parse(format!("missing SF_RES exit_code: {line:?}")))?;
    let olen_s = parts
        .next()
        .ok_or_else(|| TmuxError::Parse(format!("missing SF_RES out_len: {line:?}")))?;
    let elen_s = parts
        .next()
        .ok_or_else(|| TmuxError::Parse(format!("missing SF_RES err_len: {line:?}")))?;
    if parts.next().is_some() {
        return Err(TmuxError::Parse(format!(
            "unexpected trailing fields in SF_RES: {line:?}"
        )));
    }

    let got_seq: u64 = seq_s
        .parse()
        .map_err(|e| TmuxError::Parse(format!("invalid SF_RES seq {seq_s:?}: {e}")))?;
    if got_seq != expected_seq {
        return Err(TmuxError::Parse(format!(
            "SF_RES seq mismatch: expected {expected_seq}, got {got_seq}"
        )));
    }
    let rc: i32 = rc_s
        .parse()
        .map_err(|e| TmuxError::Parse(format!("invalid SF_RES exit_code {rc_s:?}: {e}")))?;
    let out_len: usize = olen_s
        .parse()
        .map_err(|e| TmuxError::Parse(format!("invalid SF_RES out_len {olen_s:?}: {e}")))?;
    let err_len: usize = elen_s
        .parse()
        .map_err(|e| TmuxError::Parse(format!("invalid SF_RES err_len {elen_s:?}: {e}")))?;
    if out_len > MAX_CONDUIT_FRAME_BYTES || err_len > MAX_CONDUIT_FRAME_BYTES {
        return Err(TmuxError::Parse(format!(
            "SF_RES payload exceeds max limit ({MAX_CONDUIT_FRAME_BYTES}B): out={out_len}, err={err_len}"
        )));
    }
    Ok((rc, out_len, err_len))
}

// ---------------------------------------------------------------------------
// Minimal pure-Rust RFC 4648 Base64 codec (zero external crates)
// ---------------------------------------------------------------------------

const B64_ENC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(input: &[u8]) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ENC[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ENC[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ENC[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ENC[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();
    if cleaned.len() % 4 != 0 {
        return Err(format!("invalid base64 length {}", cleaned.len()));
    }
    let mut out = Vec::with_capacity((cleaned.len() / 4) * 3);
    for (idx, chunk) in cleaned.chunks_exact(4).enumerate() {
        let is_last = (idx + 1) * 4 == cleaned.len();
        let d0 = b64_val(chunk[0])?;
        let d1 = b64_val(chunk[1])?;
        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' {
                return Err("invalid base64 padding".into());
            }
            out.push((d0 << 2) | (d1 >> 4));
            break;
        }
        let d2 = b64_val(chunk[2])?;
        if chunk[3] == b'=' {
            if !is_last {
                return Err("invalid base64 padding".into());
            }
            out.push((d0 << 2) | (d1 >> 4));
            out.push((d1 << 4) | (d2 >> 2));
            break;
        }
        let d3 = b64_val(chunk[3])?;
        out.push((d0 << 2) | (d1 >> 4));
        out.push((d1 << 4) | (d2 >> 2));
        out.push((d2 << 6) | d3);
    }
    Ok(out)
}

#[cfg(test)]
fn b64_val(b: u8) -> Result<u8, String> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("invalid base64 byte 0x{b:02x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_test_dir(tag: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("sf-conduit-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_executable(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn create_fake_ssh(dir: &Path, log_file: &Path, extra_logic: &str) -> PathBuf {
        let fake_ssh = dir.join("fake-ssh");
        write_executable(
            &fake_ssh,
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  printf 'EXIT|%s\n' "$*" >> "{log}"
  exit 0
fi
printf 'SPAWN|%s\n' "$*" >> "{log}"
{extra}
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then
    shift
    break
  fi
  shift
done
exec /bin/sh -c "$1"
"#,
                log = log_file.display(),
                extra = extra_logic,
            ),
        );
        // Warm once to avoid macOS Gatekeeper first-exec latency in timed tests.
        let _ = std::process::Command::new(&fake_ssh)
            .args(["-O", "exit", "__warmup__"])
            .output();
        std::fs::write(log_file, "").unwrap();
        fake_ssh
    }

    fn spawn_count(log_file: &Path) -> usize {
        std::fs::read_to_string(log_file)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("SPAWN|"))
            .count()
    }

    #[test]
    fn pure_rust_base64_roundtrips_rfc4648_and_binary() {
        let vectors = [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, enc) in vectors {
            assert_eq!(base64_encode(raw), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw);
        }
        let all_bytes: Vec<u8> = (0u8..=255u8).collect();
        let enc = base64_encode(&all_bytes);
        assert!(!enc.contains('\n'));
        assert_eq!(base64_decode(&enc).unwrap(), all_bytes);
        assert!(base64_decode("abc").is_err());
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn framing_header_parsers_validate_seq_and_bounds() {
        let ready = parse_ready_header(b"\x1eSF_READY 1 bsd /Users/alice with space\n").unwrap();
        assert_eq!(ready.stat_mode, RemoteStatMode::Bsd);
        assert_eq!(ready.remote_home, "/Users/alice with space");
        assert!(parse_ready_header(b"\x1eSF_READY 2 bsd /tmp\n").is_err());

        parse_pong_header(b"\x1eSF_PONG 42\n", 42).unwrap();
        assert!(parse_pong_header(b"\x1eSF_PONG 43\n", 42).is_err());

        assert_eq!(
            parse_res_header(b"\x1eSF_RES 7 0 12 3\n", 7).unwrap(),
            (0, 12, 3)
        );
        assert!(parse_res_header(b"\x1eSF_RES 8 0 12 3\n", 7).is_err());
        assert!(parse_res_header(b"\x1eSF_RES 7 0 999999999 0\n", 7).is_err());
    }

    #[tokio::test]
    async fn conduit_reuses_single_ssh_spawn_across_50_ping_and_exec_calls() {
        let dir = temp_test_dir("single-spawn-50");
        let log_file = dir.join("calls.log");
        let fake_ssh = create_fake_ssh(&dir, &log_file, "");
        let ssh = SshExec::new("gpu-01", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let pool = SshConduitPool::new(ssh.clone());

        assert_eq!(ssh.rtt_ms(), None);

        for i in 0..25u32 {
            let rtt = pool.ping(Duration::from_secs(5)).await.unwrap();
            assert!(rtt < 5_000);
            let out = pool
                .exec_text(
                    ConduitLane::Interactive,
                    &format!("printf 'iter=%d' {i}"),
                    None,
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
            assert!(out.success, "{out:?}");
            assert_eq!(out.stdout, format!("iter={i}"));
            assert!(out.stderr.is_empty());
        }

        // Exactly 1 `ssh` process spawned across all 50 sequential PING + EXEC calls!
        assert_eq!(
            spawn_count(&log_file),
            1,
            "expected exactly 1 ssh spawn across 50 calls:\n{}",
            std::fs::read_to_string(&log_file).unwrap()
        );
        assert!(ssh.rtt_ms().is_some());
        let meta = pool.meta().await.expect("meta should be cached");
        assert!(!meta.remote_home.is_empty());
        assert_ne!(meta.stat_mode, RemoteStatMode::Unknown);

        // Background lane stays lazy and spawns its own 1 persistent child on first use.
        let bg = pool
            .exec_text(
                ConduitLane::Background,
                "printf 'bg-ok'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(bg.stdout, "bg-ok");
        assert_eq!(spawn_count(&log_file), 2);

        pool.reset().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduit_fd_isolation_blocks_stdin_theft_and_stdout_noise() {
        let dir = temp_test_dir("fd-isolation");
        let log_file = dir.join("calls.log");
        let fake_ssh = create_fake_ssh(&dir, &log_file, "");
        let ssh = SshExec::new("gpu-iso", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let pool = SshConduitPool::new(ssh);

        // Inner script tries to steal conduit stdin (FD 0 & FD 3) and inject fake frames on FD 1 & FD 4.
        let hostile = r#"
read line
cat
read stolen <&3 2>/dev/null
printf '\036SF_PONG 999\n' >&4 2>/dev/null
printf 'noise_out:%s:%s' "$line" "$stolen"
printf 'noise_err' >&2
exit 7
"#;
        let out1 = pool
            .exec_text(
                ConduitLane::Interactive,
                hostile,
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(out1.code, Some(7));
        assert!(!out1.success);
        assert_eq!(out1.stdout, "noise_out::");
        assert!(
            out1.stderr.contains("noise_err"),
            "expected stderr marker, got {:?}",
            out1.stderr
        );
        assert!(
            out1.stderr.contains("Bad file descriptor"),
            "closed conduit FDs 3/4 should reject access with Bad file descriptor, got {:?}",
            out1.stderr
        );

        // Subsequent PING and EXEC frames must remain completely uncorrupted on the exact same conduit!
        let rtt = pool.ping(Duration::from_secs(5)).await.unwrap();
        assert!(rtt < 5_000);
        let out2 = pool
            .exec_text(
                ConduitLane::Interactive,
                "printf 'still-clean'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out2.success);
        assert_eq!(out2.stdout, "still-clean");
        assert_eq!(spawn_count(&log_file), 1);

        pool.reset().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduit_binary_fidelity_roundtrips_gzip_nul_rs_and_invalid_utf8() {
        let dir = temp_test_dir("binary-fidelity");
        let log_file = dir.join("calls.log");
        let fake_ssh = create_fake_ssh(&dir, &log_file, "");
        let ssh = SshExec::new("gpu-bin", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let pool = SshConduitPool::new(ssh);

        // 1. Small payload containing NUL, gzip header (0x1f 0x8b), frame RS (0x1e), invalid UTF-8.
        let small: Vec<u8> = vec![
            0x00, 0x1f, 0x8b, 0x08, 0x1e, 0x53, 0x46, 0x5f, 0x52, 0x45, 0x53, 0x0a, 0x0d, 0xff,
            0xfe, 0x80, 0x00, 0x42,
        ];
        let out_small = pool
            .exec_bytes(
                ConduitLane::Interactive,
                "cat; printf 'bin-err\\n' >&2",
                Some(&small),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out_small.success());
        assert_eq!(out_small.stdout, small);
        assert_eq!(out_small.stderr, "bin-err\n");

        // 2. Large >64 KiB binary payload (exercises full byte range & guards against macOS sh fd bug).
        let mut large = Vec::with_capacity(66_000);
        large.extend_from_slice(&small);
        for i in 0..65_536u32 {
            large.push(((i.wrapping_mul(31) ^ (i >> 3)) & 0xff) as u8);
        }
        let out_large = pool
            .exec_bytes(
                ConduitLane::Background,
                "cat",
                Some(&large),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(out_large.success());
        assert_eq!(out_large.stdout, large);
        assert!(out_large.stderr.is_empty());

        // 3. Direct binary generation on stdout with no stdin.
        let gen = pool
            .exec_bytes(
                ConduitLane::Interactive,
                "printf '\\000\\037\\213\\036\\377'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(gen.success());
        assert_eq!(gen.stdout, vec![0x00, 0x1f, 0x8b, 0x1e, 0xff]);

        pool.reset().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduit_timeout_kills_only_hung_lane_and_respawns_cleanly() {
        let dir = temp_test_dir("timeout-recovery");
        let log_file = dir.join("calls.log");
        let fake_ssh = create_fake_ssh(&dir, &log_file, "");
        let ssh = SshExec::new("gpu-to", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let pool = SshConduitPool::new(ssh);

        // Warm both lanes (2 spawns).
        pool.ping(Duration::from_secs(5)).await.unwrap();
        let bg0 = pool
            .exec_text(
                ConduitLane::Background,
                "printf 'bg0'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(bg0.stdout, "bg0");
        assert_eq!(spawn_count(&log_file), 2);

        // Hang ONLY the Background lane and trigger a short timeout.
        let started = Instant::now();
        let err = pool
            .exec_bytes(
                ConduitLane::Background,
                "sleep 30",
                None,
                Duration::from_millis(180),
            )
            .await
            .expect_err("hung background command must time out");
        assert!(matches!(err, TmuxError::Timeout { .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(3));

        // Interactive lane must still be alive (still 2 spawns total!).
        let inter = pool
            .exec_text(
                ConduitLane::Interactive,
                "printf 'inter-alive'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(inter.stdout, "inter-alive");
        assert_eq!(spawn_count(&log_file), 2);

        // Next Background call lazily respawns cleanly (3 spawns total).
        let bg1 = pool
            .exec_text(
                ConduitLane::Background,
                "printf 'bg-recovered'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(bg1.stdout, "bg-recovered");
        assert_eq!(spawn_count(&log_file), 3);

        pool.reset().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduit_pool_restricted_shell_fallback_after_two_handshake_failures() {
        let dir = temp_test_dir("restricted-fallback");
        let log_file = dir.join("calls.log");
        // Simulate a host where persistent `-T` conduit handshake fails (e.g. missing base64 /
        // restricted shell -> exit 12 without SF_READY), while one-shot SshExec still works.
        let reject_conduit = r#"
for _a in "$@"; do
  if [ "$_a" = "-T" ]; then
    printf 'restricted: base64 not found\n' >&2
    exit 12
  fi
done
"#;
        let fake_ssh = create_fake_ssh(&dir, &log_file, reject_conduit);
        let ssh = SshExec::new("gpu-restricted", dir.join("cm.sock"))
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()));
        let pool = SshConduitPool::new(ssh.clone());

        assert!(!pool.is_fallback_active());

        // Failure #1: conduit handshake fails, transparent fallback executes via SshExec,
        // cooldown NOT engaged yet (threshold = 2).
        let out1 = pool
            .exec_text(
                ConduitLane::Interactive,
                "printf 'fb-1'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out1.success);
        assert_eq!(out1.stdout, "fb-1");
        assert!(!pool.is_fallback_active());

        // Failure #2: second consecutive SF_READY failure engages 10-minute fallback cooldown.
        let out2 = pool
            .exec_text(
                ConduitLane::Interactive,
                "printf 'fb-2'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out2.success);
        assert_eq!(out2.stdout, "fb-2");
        assert!(pool.is_fallback_active());

        // Call #3 & ping() MUST bypass conduit handshake completely (0 `-T` attempts!).
        std::fs::write(&log_file, "").unwrap();
        let out3 = pool
            .exec_text(
                ConduitLane::Background,
                "printf 'fb-3'",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out3.success);
        assert_eq!(out3.stdout, "fb-3");
        let probed_rtt = pool.ping(Duration::from_secs(5)).await.unwrap();
        assert!(probed_rtt < 5_000);
        assert_eq!(ssh.rtt_ms(), Some(probed_rtt));

        let log_after = std::fs::read_to_string(&log_file).unwrap();
        assert!(
            !log_after.contains(" -T "),
            "active fallback must skip conduit handshake entirely:\n{log_after}"
        );
        assert_eq!(spawn_count(&log_file), 2);

        // `reset()` clears fallback state cleanly.
        pool.reset().await;
        assert!(!pool.is_fallback_active());

        // Verify pure SSH transport failure (exit 255) does NOT engage restricted-shell fallback.
        let log_255 = dir.join("calls-255.log");
        let fake_255 = create_fake_ssh(&dir, &log_255, "exit 255\n");
        let ssh_255 = SshExec::new("gpu-down", dir.join("cm255.sock"))
            .with_ssh_bin(Some(fake_255.to_string_lossy().into_owned()));
        let pool_255 = SshConduitPool::new(ssh_255);
        assert!(pool_255
            .exec_bytes(
                ConduitLane::Interactive,
                "true",
                None,
                Duration::from_secs(5)
            )
            .await
            .is_err());
        assert!(pool_255
            .exec_bytes(
                ConduitLane::Interactive,
                "true",
                None,
                Duration::from_secs(5)
            )
            .await
            .is_err());
        assert!(
            !pool_255.is_fallback_active(),
            "exit 255 transport errors must not trigger restricted-shell fallback"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
