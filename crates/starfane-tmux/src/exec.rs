//! Synchronous command execution transports (local process & SSH ControlMaster) with hard timeout.

use crate::error::TmuxError;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound for any single local mux invocation.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Default upper bound for SSH ControlMaster remote invocations.
pub const DEFAULT_SSH_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// macOS `sockaddr_un.sun_path` is 104 bytes; keep ControlPath strictly shorter.
const MAX_CONTROL_PATH_BYTES: usize = 92;

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
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
        run_with_timeout(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            None,
            self.timeout,
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
}

impl SshExec {
    pub fn new(target: impl Into<String>, control_path: PathBuf) -> Self {
        let target = target.into();
        let control_path = ensure_short_control_path(&target, control_path);
        Self {
            ssh_bin: PathBuf::from("ssh"),
            target,
            control_path,
            extra_args: Vec::new(),
            timeout: DEFAULT_SSH_TIMEOUT,
        }
    }

    pub fn with_extra_args(mut self, extra_args: Vec<String>) -> Self {
        self.extra_args = extra_args;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// Best-effort cleanup of a dead/stale OpenSSH ControlMaster socket after a transport error.
    fn clear_stale_control_socket(&self) {
        if self.control_path.exists() {
            let _ = std::fs::remove_file(&self.control_path);
        }
    }

    fn ssh_prefix_args(&self) -> Vec<String> {
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
            "ControlPersist=300".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", self.control_path.display()),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=2".to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());
        args.push(self.target.clone());
        args.push("--".to_string());
        args
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
        let res = run_with_timeout(&self.ssh_bin, &ssh_args, stdin, self.timeout);
        match &res {
            Err(TmuxError::Timeout { .. }) => self.clear_stale_control_socket(),
            Ok(out) if out.code == Some(255) => self.clear_stale_control_socket(),
            _ => {}
        }
        res
    }

    fn run_shell(&self, script: &str) -> Result<ExecOutput, TmuxError> {
        // Pipe script via stdin to `/bin/sh -s`: completely bypasses remote login shell
        // quoting quirks (`fish`, `csh`, `tcsh`, `nu`, etc.).
        let mut ssh_args = self.ssh_prefix_args();
        ssh_args.push("/bin/sh -s".to_string());
        let res = run_with_timeout(
            &self.ssh_bin,
            &ssh_args,
            Some(script.as_bytes()),
            self.timeout,
        );
        match &res {
            Err(TmuxError::Timeout { .. }) => self.clear_stale_control_socket(),
            Ok(out) if out.code == Some(255) => self.clear_stale_control_socket(),
            _ => {}
        }
        res
    }

    fn is_local(&self) -> bool {
        false
    }
}

pub(crate) fn run_with_timeout(
    bin: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, TmuxError> {
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
    let out_reader = drain(child.stdout.take());
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
    Ok(Output {
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        success: status.success(),
        code: status.code(),
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
}
