//! Synchronous command execution transports (local process & SSH ControlMaster) with hard timeout.

use crate::error::TmuxError;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound for any single local mux invocation.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Default upper bound for SSH ControlMaster remote invocations.
pub const DEFAULT_SSH_TIMEOUT: Duration = Duration::from_secs(8);
/// Default OpenSSH `ConnectTimeout` in seconds.
pub const DEFAULT_SSH_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Environment variable fallback for overriding the `ssh` binary path (e.g. `fake-ssh` in tests).
pub const SSH_BIN_ENV: &str = "STARFANE_SSH_BIN";

const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// macOS `sockaddr_un.sun_path` is 104 bytes; keep ControlPath strictly shorter.
const MAX_CONTROL_PATH_BYTES: usize = 92;
/// Bounded timeout for best-effort `ssh -O exit` zombie ControlMaster cleanup.
const CONTROL_EXIT_TIMEOUT: Duration = Duration::from_millis(500);
/// Upper elapsed bound for classifying an exit-255 failure as a stale local mux socket.
const STALE_MUX_MAX_ELAPSED: Duration = Duration::from_millis(1_500);
/// Sentinel stored in `rtt_ewma_ms` before any successful round-trip has been measured.
const RTT_UNSET: u64 = u64::MAX;

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

/// Binary-safe command output preserving raw `stdout` bytes (e.g. gzipped transcript deltas).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBytesOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl ExecBytesOutput {
    /// True when the process exited cleanly with status code `0`.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Convert raw stdout bytes to UTF-8 lossily, yielding an [`ExecOutput`].
    pub fn into_utf8_lossy(self) -> ExecOutput {
        let success = self.success();
        ExecOutput {
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: self.stderr,
            success,
            code: self.exit_code,
        }
    }
}

pub(crate) type Output = ExecOutput;

/// Pluggable process execution transport for local or SSH-forwarded multiplexers.
pub trait ExecTransport: Send + Sync + fmt::Debug {
    /// Execute `bin args...`, optionally piping `stdin`.
    fn run(
        &self,
        bin: &Path,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<ExecOutput, TmuxError>;

    /// Execute a POSIX shell script (used for single-round-trip remote discovery over SSH).
    fn run_shell(&self, script: &str) -> Result<ExecOutput, TmuxError>;

    /// Execute a POSIX shell script with optional raw `stdin` and custom `timeout`,
    /// preserving raw binary `stdout` bytes without UTF-8 coercion.
    fn run_shell_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError>;

    /// Convenience wrapper over [`ExecTransport::run_shell_bytes`] returning lossy UTF-8 stdout.
    fn run_shell_with_stdin(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, TmuxError> {
        self.run_shell_bytes(script, stdin, timeout)
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    /// True if commands run on the local machine (enables local filesystem lookups like git root).
    fn is_local(&self) -> bool;
}

/// Direct local child-process transport.
#[derive(Debug, Clone)]
pub struct LocalExec {
    timeout: Duration,
}

impl Default for LocalExec {
    fn default() -> Self {
        Self {
            timeout: COMMAND_TIMEOUT,
        }
    }
}

impl LocalExec {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl ExecTransport for LocalExec {
    fn run(
        &self,
        bin: &Path,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<ExecOutput, TmuxError> {
        run_with_timeout(bin, args, stdin, self.timeout)
    }

    fn run_shell(&self, script: &str) -> Result<ExecOutput, TmuxError> {
        self.run_shell_bytes(script, None, self.timeout)
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    fn run_shell_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        run_bytes_with_timeout(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            stdin,
            timeout,
        )
    }

    fn is_local(&self) -> bool {
        true
    }
}

/// Zero-install remote execution over persistent OpenSSH ControlMaster multiplexing.
#[derive(Debug, Clone)]
pub struct SshExec {
    ssh_bin: PathBuf,
    target: String,
    control_path: PathBuf,
    extra_args: Vec<String>,
    timeout: Duration,
    connect_timeout_secs: u64,
    compression: bool,
    rtt_ewma_ms: Arc<AtomicU64>,
}

fn resolve_ssh_bin(explicit: Option<&str>) -> PathBuf {
    if let Some(bin) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(bin);
    }
    if let Ok(env_bin) = std::env::var(SSH_BIN_ENV) {
        let trimmed = env_bin.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from("ssh")
}

fn is_stale_mux_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("controlsocket")
        || lower.contains("control socket connect")
        || lower.contains("mux_client_request_session")
        || lower.contains("mux_client_hello_exchange")
        || lower.contains("broken pipe")
}

fn should_retry_stale_mux(exit_code: Option<i32>, elapsed: Duration, stderr: &str) -> bool {
    exit_code == Some(255) && elapsed < STALE_MUX_MAX_ELAPSED && is_stale_mux_stderr(stderr)
}

impl SshExec {
    pub fn new(target: impl Into<String>, control_path: PathBuf) -> Self {
        let target = target.into();
        let control_path = ensure_short_control_path(&target, control_path);
        Self {
            ssh_bin: resolve_ssh_bin(None),
            target,
            control_path,
            extra_args: Vec::new(),
            timeout: DEFAULT_SSH_TIMEOUT,
            connect_timeout_secs: DEFAULT_SSH_CONNECT_TIMEOUT_SECS,
            compression: true,
            rtt_ewma_ms: Arc::new(AtomicU64::new(RTT_UNSET)),
        }
    }

    /// Override the `ssh` binary path. When `None` or blank, falls back to
    /// `STARFANE_SSH_BIN` env var or `"ssh"`.
    pub fn with_ssh_bin(mut self, ssh_bin: Option<String>) -> Self {
        self.ssh_bin = resolve_ssh_bin(ssh_bin.as_deref());
        self
    }

    /// Configure OpenSSH `ConnectTimeout` in seconds (clamped to `>= 1`; defaults to `5`).
    pub fn with_connect_timeout_secs(mut self, connect_timeout_secs: u64) -> Self {
        self.connect_timeout_secs = connect_timeout_secs.max(1);
        self
    }

    /// Enable or disable OpenSSH zlib compression (`-o Compression=yes`, enabled by default).
    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compression = compression;
        self
    }

    pub fn with_extra_args(mut self, extra_args: Vec<String>) -> Self {
        self.extra_args = extra_args;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn ssh_bin(&self) -> &Path {
        &self.ssh_bin
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn connect_timeout_secs(&self) -> u64 {
        self.connect_timeout_secs
    }

    pub fn compression(&self) -> bool {
        self.compression
    }

    pub fn extra_args(&self) -> &[String] {
        &self.extra_args
    }

    /// Exponentially weighted moving average (EWMA) pure transport round-trip latency in
    /// milliseconds (`None` before first sample).
    ///
    /// Updated exclusively via [`SshExec::record_rtt_sample`] (e.g. persistent conduit zero-fork
    /// `PING -> SF_PONG`) or [`SshExec::probe_rtt`], and never polluted by discovery/dispatch
    /// script wall-clock durations.
    pub fn rtt_ms(&self) -> Option<u64> {
        let v = self.rtt_ewma_ms.load(Ordering::Acquire);
        (v != RTT_UNSET).then_some(v)
    }

    /// Record a pure transport round-trip latency sample (in milliseconds) into the EWMA.
    pub fn record_rtt_sample(&self, sample_ms: u64) {
        let sample = sample_ms.min(RTT_UNSET - 1);
        let mut prev = self.rtt_ewma_ms.load(Ordering::Acquire);
        loop {
            let next = if prev == RTT_UNSET {
                sample
            } else {
                // alpha = 0.25 with rounding: smooths WAN jitter while tracking shifts quickly.
                prev.saturating_mul(3)
                    .saturating_add(sample)
                    .saturating_add(2)
                    / 4
            };
            match self.rtt_ewma_ms.compare_exchange_weak(
                prev,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }
    }

    /// Lightweight `printf pong` transport RTT probe for non-conduit fallback.
    ///
    /// Records the measured round-trip duration into [`SshExec::rtt_ms`] and returns the raw
    /// sample in milliseconds. Retries once on a fast stale ControlMaster socket (exit `255`).
    pub fn probe_rtt(&self) -> Result<u64, TmuxError> {
        self.probe_rtt_with_timeout(self.timeout)
    }

    /// Variant of [`SshExec::probe_rtt`] with an explicit per-attempt timeout.
    pub fn probe_rtt_with_timeout(&self, timeout: Duration) -> Result<u64, TmuxError> {
        let mut ssh_args = self.ssh_prefix_args();
        ssh_args.push("printf pong".to_string());
        let (mut res, mut elapsed) = self.exec_ssh_bytes_once(&ssh_args, None, timeout);
        if let Ok(ref out) = res {
            if should_retry_stale_mux(out.exit_code, elapsed, &out.stderr) {
                (res, elapsed) = self.exec_ssh_bytes_once(&ssh_args, None, timeout);
            }
        }
        let out = res?;
        if !out.success() {
            return Err(TmuxError::Command {
                args: "ssh probe_rtt (printf pong)".to_string(),
                code: out.exit_code,
                stderr: out.stderr,
            });
        }
        let ok_pong = out.stdout == b"pong"
            || String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == "pong");
        if !ok_pong {
            return Err(TmuxError::Parse(format!(
                "ssh probe_rtt expected 'pong', got {:?}",
                String::from_utf8_lossy(&out.stdout)
            )));
        }
        let sample_ms = elapsed.as_millis().min((RTT_UNSET - 1) as u128) as u64;
        self.record_rtt_sample(sample_ms);
        Ok(sample_ms)
    }

    /// Best-effort cleanup of a dead/stale OpenSSH ControlMaster socket after a transport error.
    /// Runs `ssh -O exit -o ControlPath=<sock> <target>` (capped at 500ms) before unlinking any
    /// leftover socket file so wedged background `ssh [mux]` processes are reaped cleanly.
    pub fn clear_stale_control_socket(&self) {
        let exit_args = vec![
            "-O".to_string(),
            "exit".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", self.control_path.display()),
            self.target.clone(),
        ];
        let _ = run_bytes_with_timeout(&self.ssh_bin, &exit_args, None, CONTROL_EXIT_TIMEOUT);
        if self.control_path.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&self.control_path);
        }
    }

    /// Build the shared hardened OpenSSH option flags (ControlMaster, timeouts, keepalive,
    /// compression, and `extra_args`) that precede `<target>` on the `ssh` command line.
    ///
    /// Also ensures the `ControlPath` parent directory exists with `0o700` permissions so
    /// persistent conduits and `tmux -C` watchers reuse the exact same mux configuration.
    pub fn build_ssh_base_args(&self) -> Vec<String> {
        if let Some(parent) = self.control_path.parent() {
            let _ = std::fs::create_dir_all(parent);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let mut args = vec![
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPersist=600".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", self.control_path.display()),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectionAttempts=1".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            "-o".to_string(),
            "TCPKeepAlive=yes".to_string(),
        ];
        if self.compression {
            args.push("-o".to_string());
            args.push("Compression=yes".to_string());
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    fn ssh_prefix_args(&self) -> Vec<String> {
        let mut args = self.build_ssh_base_args();
        args.push(self.target.clone());
        args.push("--".to_string());
        args
    }

    fn build_shell_invocation<'a>(
        &self,
        script: &'a str,
        stdin: Option<&'a [u8]>,
    ) -> (Vec<String>, Option<&'a [u8]>) {
        let mut ssh_args = self.ssh_prefix_args();
        match stdin {
            None => {
                // Pipe script via stdin to `/bin/sh -s`: completely bypasses remote login shell
                // quoting quirks (`fish`, `csh`, `tcsh`, `nu`, etc.).
                ssh_args.push("/bin/sh -s".to_string());
                (ssh_args, Some(script.as_bytes()))
            }
            Some(payload) => {
                // Keep stdin free for binary/text payload by passing the script via `/bin/sh -c`.
                ssh_args.push(format!("/bin/sh -c {}", ssh_universal_quote(script)));
                (ssh_args, Some(payload))
            }
        }
    }

    fn exec_ssh_bytes_once(
        &self,
        ssh_args: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> (Result<ExecBytesOutput, TmuxError>, Duration) {
        let started = Instant::now();
        let res = run_bytes_with_timeout(&self.ssh_bin, ssh_args, stdin, timeout);
        let elapsed = started.elapsed();
        match &res {
            Err(TmuxError::Timeout { .. }) => self.clear_stale_control_socket(),
            Ok(out) if out.exit_code == Some(255) => self.clear_stale_control_socket(),
            Ok(_) | Err(_) => {}
        }
        (res, elapsed)
    }

    /// Execute a strictly **read-only / idempotent** remote shell script, preserving raw binary
    /// `stdout` bytes.
    ///
    /// **NEVER** use this for mutating operations (`dispatch`, `spawn`, `kill`, file staging).
    /// If the first attempt fails quickly (`< 1.5s`) with SSH transport exit `255` and `stderr`
    /// indicating a stale ControlMaster socket, cleans up the stale socket and retries once.
    pub fn run_idempotent_shell_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        let (ssh_args, effective_stdin) = self.build_shell_invocation(script, stdin);
        let (res, elapsed) = self.exec_ssh_bytes_once(&ssh_args, effective_stdin, timeout);
        if let Ok(ref out) = res {
            if should_retry_stale_mux(out.exit_code, elapsed, &out.stderr) {
                // `exec_ssh_bytes_once` already invoked `clear_stale_control_socket()` on exit 255.
                return self
                    .exec_ssh_bytes_once(&ssh_args, effective_stdin, timeout)
                    .0;
            }
        }
        res
    }

    /// Execute a strictly **read-only / idempotent** remote shell script using the default
    /// transport timeout, with 1-shot transparent recovery on a stale ControlMaster socket.
    ///
    /// **NEVER** use this for mutating operations (`dispatch`, `spawn`, `kill`, file staging).
    pub fn run_idempotent_shell(&self, script: &str) -> Result<ExecOutput, TmuxError> {
        self.run_idempotent_shell_bytes(script, None, self.timeout)
            .map(ExecBytesOutput::into_utf8_lossy)
    }
}

/// Cross-shell single-quote escaping (`'"'"'`) that works identically across POSIX `sh`,
/// `bash`, `zsh`, `csh`/`tcsh`, and `fish` (unlike `'\''` which breaks in `fish`).
pub fn ssh_universal_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn ensure_short_control_path(target: &str, preferred: PathBuf) -> PathBuf {
    if preferred.to_string_lossy().len() <= MAX_CONTROL_PATH_BYTES {
        return preferred;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut h);
    preferred.hash(&mut h);
    let digest = h.finish();
    let dir = PathBuf::from(format!("/tmp/sf-ssh-{}", std::process::id()));
    dir.join(format!("c-{digest:016x}.sock"))
}

impl ExecTransport for SshExec {
    fn run(
        &self,
        bin: &Path,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<ExecOutput, TmuxError> {
        let mut cmd_parts = Vec::with_capacity(1 + args.len());
        cmd_parts.push(ssh_universal_quote(&bin.to_string_lossy()));
        for a in args {
            cmd_parts.push(ssh_universal_quote(a));
        }
        let remote_cmd = cmd_parts.join(" ");
        let mut ssh_args = self.ssh_prefix_args();
        ssh_args.push(remote_cmd);
        self.exec_ssh_bytes_once(&ssh_args, stdin, self.timeout)
            .0
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    fn run_shell(&self, script: &str) -> Result<ExecOutput, TmuxError> {
        self.run_shell_bytes(script, None, self.timeout)
            .map(ExecBytesOutput::into_utf8_lossy)
    }

    fn run_shell_bytes(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecBytesOutput, TmuxError> {
        let (ssh_args, effective_stdin) = self.build_shell_invocation(script, stdin);
        self.exec_ssh_bytes_once(&ssh_args, effective_stdin, timeout)
            .0
    }

    fn is_local(&self) -> bool {
        false
    }
}

pub(crate) fn run_bytes_with_timeout(
    bin: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<ExecBytesOutput, TmuxError> {
    let shown = args.join(" ");
    let io_err = |what: &str, source| TmuxError::Io {
        what: format!("{what}: {} {shown}", bin.display()),
        source,
    };
    let mut child = Command::new(bin)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io_err("spawn", e))?;

    let writer = match (stdin, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => {
            let data = data.to_vec();
            // A broken pipe here means the child exited early; its exit status tells the story.
            Some(thread::spawn(move || pipe.write_all(&data)))
        }
        _ => None,
    };
    let out_reader = drain_bytes(child.stdout.take());
    let err_reader = drain(child.stderr.take());

    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| io_err("wait", e))? {
            Some(s) => break s,
            None if started.elapsed() >= timeout => {
                // Best effort: the child may have exited between try_wait and kill.
                let _ = child.kill();
                let _ = child.wait();
                return Err(TmuxError::Timeout {
                    args: shown,
                    secs: timeout.as_secs(),
                });
            }
            None => thread::sleep(POLL_INTERVAL),
        }
    };
    if let Some(w) = writer {
        if let Ok(Err(e)) = w.join() {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(io_err("write stdin", e));
            }
        }
    }
    Ok(ExecBytesOutput {
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        exit_code: status.code(),
    })
}

pub(crate) fn run_with_timeout(
    bin: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, TmuxError> {
    run_bytes_with_timeout(bin, args, stdin, timeout).map(ExecBytesOutput::into_utf8_lossy)
}

fn drain_bytes<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            // A read error just truncates output; the exit status is authoritative.
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            // A read error just truncates diagnostics output; the exit status is authoritative.
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_test_dir(tag: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sf-exec-{tag}-{}-{seq}", std::process::id()));
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

    #[test]
    fn ssh_universal_quote_handles_single_quotes_without_backslashes() {
        assert_eq!(ssh_universal_quote("hello"), "'hello'");
        assert_eq!(ssh_universal_quote("it's ok"), "'it'\"'\"'s ok'");
    }

    #[test]
    fn ssh_control_path_stays_below_macos_sun_path_limit() {
        let short = PathBuf::from("/tmp/cm-a.sock");
        assert_eq!(ensure_short_control_path("host", short.clone()), short);

        let long = PathBuf::from(
            "/Users/someone/Library/Application Support/starfane/very-long-state-dir/ssh-cm/cm-production-gpu-cluster-node-01.sock",
        );
        let bounded = ensure_short_control_path("gpu-01", long);
        assert!(
            bounded.to_string_lossy().len() <= MAX_CONTROL_PATH_BYTES,
            "path too long: {}",
            bounded.display()
        );
    }

    #[test]
    fn local_exec_run_shell_bytes_preserves_binary_stdout_and_stdin() {
        let exec = LocalExec::default();
        let payload: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0xff, 0xfe, 0x00, 0x42];
        let out = exec
            .run_shell_bytes("cat", Some(&payload), Duration::from_secs(5))
            .unwrap();
        assert!(out.success());
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout, payload);
        assert!(out.stderr.is_empty());

        let via_stdin = exec
            .run_shell_with_stdin("cat", Some(b"hello-world"), Duration::from_secs(5))
            .unwrap();
        assert!(via_stdin.success);
        assert_eq!(via_stdin.stdout, "hello-world");
    }

    #[test]
    fn ssh_prefix_args_include_hardened_control_master_options() {
        let dir = temp_test_dir("prefix");
        let sock = dir.join("cm").join("host.sock");
        let ssh = SshExec::new("ubuntu@gpu-01", sock.clone())
            .with_connect_timeout_secs(9)
            .with_compression(true)
            .with_extra_args(vec!["-p".into(), "2222".into()]);

        let args = ssh.ssh_prefix_args();
        let base_args = ssh.build_ssh_base_args();
        assert_eq!(&args[..base_args.len()], base_args.as_slice());
        assert_eq!(&args[base_args.len()..], &["ubuntu@gpu-01", "--"]);
        let joined = args.join(" ");
        for expected in [
            "-o ControlMaster=auto",
            "-o ControlPersist=600",
            &format!("-o ControlPath={}", sock.display()),
            "-o BatchMode=yes",
            "-o ConnectionAttempts=1",
            "-o ConnectTimeout=9",
            "-o ServerAliveInterval=15",
            "-o ServerAliveCountMax=3",
            "-o TCPKeepAlive=yes",
            "-o Compression=yes",
            "-p 2222",
            "ubuntu@gpu-01 --",
        ] {
            assert!(
                joined.contains(expected),
                "missing expected ssh arg {expected:?} in: {joined}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(sock.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }

        let no_comp = SshExec::new("ubuntu@gpu-01", sock).with_compression(false);
        assert!(!no_comp
            .ssh_prefix_args()
            .join(" ")
            .contains("Compression=yes"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_exec_binary_roundtrip_and_ewma_rtt() {
        let dir = temp_test_dir("binary-rtt");
        let sock = dir.join("cm.sock");
        let fake_ssh = dir.join("fake-ssh");
        let log_file = dir.join("calls.log");
        write_executable(
            &fake_ssh,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--" ]; then
    shift
    break
  fi
  shift
done
exec /bin/sh -c "$1"
"#,
                log_file.display()
            ),
        );

        let ssh = SshExec::new("gpu-01", sock)
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()))
            .with_timeout(Duration::from_secs(5));
        let ssh_clone = ssh.clone();
        assert_eq!(ssh.rtt_ms(), None);

        // 1. run_shell_bytes with stdin = None executes script via /bin/sh -s and returns raw bytes,
        //    and MUST NOT pollute `rtt_ms()` with arbitrary script execution wall-clock time.
        let out = ssh
            .run_shell_bytes(
                "printf '\\037\\213\\000\\377'",
                None,
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, vec![0x1f, 0x8b, 0x00, 0xff]);
        assert_eq!(ssh.rtt_ms(), None);

        // 2. run_shell_bytes with stdin = Some(binary) keeps stdin intact via /bin/sh -c '<quoted>'.
        let payload = vec![0x00, 0x89, 0x50, 0x4e, 0x47, 0xff];
        let echoed = ssh
            .run_shell_bytes("cat", Some(&payload), Duration::from_secs(5))
            .unwrap();
        assert!(echoed.success());
        assert_eq!(echoed.stdout, payload);
        assert_eq!(ssh.rtt_ms(), None);

        // 3. Explicit lightweight `probe_rtt()` measures transport RTT and updates EWMA across clones.
        let probed = ssh.probe_rtt().unwrap();
        assert!(probed < 5_000);
        assert_eq!(ssh.rtt_ms(), Some(probed));
        assert_eq!(ssh.rtt_ms(), ssh_clone.rtt_ms());

        let log = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("-- /bin/sh -s"), "{}", lines[0]);
        assert!(lines[1].contains("-- /bin/sh -c 'cat'"), "{}", lines[1]);
        assert!(lines[2].ends_with("-- printf pong"), "{}", lines[2]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_exec_stale_mux_retry_and_zombie_exit_cleanup() {
        let dir = temp_test_dir("stale-mux");
        let sock = dir.join("cm.sock");
        let fake_ssh = dir.join("fake-ssh");
        let log_file = dir.join("calls.log");
        let fail_flag = dir.join("fail_once");
        write_executable(
            &fake_ssh,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "-O" ] && [ "$2" = "exit" ]; then
  exit 0
fi
if [ -f "{flag}" ]; then
  rm -f "{flag}"
  printf 'mux_client_request_session: read from master failed: Broken pipe\n' >&2
  exit 255
fi
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
                flag = fail_flag.display()
            ),
        );

        // Warm `fake-ssh` once before timed assertions so macOS Gatekeeper/syspolicyd cold
        // first-exec scanning does not push `elapsed` past `STALE_MUX_MAX_ELAPSED` (1.5s).
        let _ = Command::new(&fake_ssh)
            .args(["-O", "exit", "__warmup__"])
            .output();
        std::fs::write(&log_file, "").unwrap();

        let ssh = SshExec::new("gpu-01", sock.clone())
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()))
            .with_timeout(Duration::from_secs(5));

        // 1. Idempotent call retries once on fast stale-mux exit 255 after `-O exit` + unlink.
        std::fs::write(&sock, "stale-socket").unwrap();
        std::fs::write(&fail_flag, "1").unwrap();
        let ok = ssh.run_idempotent_shell("printf recovered").unwrap();
        assert!(ok.success, "expected recovery: {ok:?}");
        assert_eq!(ok.stdout, "recovered");
        assert!(!sock.exists(), "stale control socket should be removed");
        assert_eq!(ssh.rtt_ms(), None);

        let log1 = std::fs::read_to_string(&log_file).unwrap();
        let lines1: Vec<&str> = log1.lines().collect();
        assert_eq!(lines1.len(), 3, "expected fail + -O exit + retry:\n{log1}");
        assert!(
            lines1[1].starts_with("-O exit -o ControlPath="),
            "expected -O exit cleanup, got: {}",
            lines1[1]
        );
        assert!(lines1[1].ends_with(" gpu-01"), "{}", lines1[1]);

        // 2. Non-idempotent `run_shell` NEVER retries even on fast stale-mux exit 255.
        std::fs::write(&log_file, "").unwrap();
        std::fs::write(&sock, "stale-socket-2").unwrap();
        std::fs::write(&fail_flag, "1").unwrap();
        let no_retry = ssh.run_shell("printf mutating").unwrap();
        assert!(!no_retry.success);
        assert_eq!(no_retry.code, Some(255));
        assert!(!sock.exists());

        let log2 = std::fs::read_to_string(&log_file).unwrap();
        let lines2: Vec<&str> = log2.lines().collect();
        assert_eq!(lines2.len(), 2, "must NOT retry mutating call:\n{log2}");
        assert!(lines2[1].starts_with("-O exit -o ControlPath="));

        // 3. Timeout also triggers `-O exit` cleanup and socket removal without recording RTT.
        let ssh_timeout = SshExec::new("gpu-02", sock.clone())
            .with_ssh_bin(Some(fake_ssh.to_string_lossy().into_owned()))
            .with_timeout(Duration::from_millis(80));
        std::fs::write(&log_file, "").unwrap();
        std::fs::write(&sock, "wedged-socket").unwrap();
        let err = ssh_timeout
            .run_shell("sleep 2")
            .expect_err("expected timeout");
        assert!(matches!(err, TmuxError::Timeout { .. }), "{err:?}");
        assert!(!sock.exists(), "wedged control socket should be removed");
        assert_eq!(ssh_timeout.rtt_ms(), None);
        let log3 = std::fs::read_to_string(&log_file).unwrap();
        assert!(
            log3.lines()
                .any(|l| l.starts_with("-O exit -o ControlPath=")),
            "expected -O exit cleanup after timeout:\n{log3}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_mux_retry_predicate_enforces_code_elapsed_and_stderr() {
        assert!(should_retry_stale_mux(
            Some(255),
            Duration::from_millis(120),
            "ControlSocket /tmp/x.sock already exists, disabling multiplexing"
        ));
        assert!(should_retry_stale_mux(
            Some(255),
            Duration::from_millis(400),
            "Control socket connect(/tmp/x.sock): Connection refused"
        ));
        assert!(should_retry_stale_mux(
            Some(255),
            Duration::from_millis(50),
            "mux_client_request_session: session request failed: Broken pipe"
        ));
        // Elapsed >= 1.5s -> never retry.
        assert!(!should_retry_stale_mux(
            Some(255),
            Duration::from_millis(1_500),
            "Broken pipe"
        ));
        // Non-255 exit -> never retry.
        assert!(!should_retry_stale_mux(
            Some(1),
            Duration::from_millis(20),
            "Broken pipe"
        ));
        // Auth / unrelated 255 stderr -> never retry.
        assert!(!should_retry_stale_mux(
            Some(255),
            Duration::from_millis(80),
            "Permission denied (publickey)."
        ));
    }

    #[test]
    fn ewma_rtt_initializes_and_smooths_samples() {
        let ssh = SshExec::new("gpu-01", PathBuf::from("/tmp/cm-ewma.sock"));
        assert_eq!(ssh.rtt_ms(), None);
        ssh.record_rtt_sample(200);
        assert_eq!(ssh.rtt_ms(), Some(200));
        ssh.record_rtt_sample(100);
        assert_eq!(ssh.rtt_ms(), Some(175));
        ssh.record_rtt_sample(100);
        assert_eq!(ssh.rtt_ms(), Some(156));
    }
}
