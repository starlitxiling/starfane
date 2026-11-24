//! Synchronous tmux process runner with a hard timeout.

use crate::error::TmuxError;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound for any single tmux invocation.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

pub(crate) struct Output {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

/// Run `bin args…`, optionally feeding `stdin`, killing it after [`COMMAND_TIMEOUT`].
pub(crate) fn run(bin: &Path, args: &[String], stdin: Option<&[u8]>) -> Result<Output, TmuxError> {
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
            // A broken pipe here means tmux exited early; its exit status tells the story.
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
            None if started.elapsed() >= COMMAND_TIMEOUT => {
                // Best effort: the child may have exited between try_wait and kill.
                let _ = child.kill();
                let _ = child.wait();
                return Err(TmuxError::Timeout {
                    args: shown,
                    secs: COMMAND_TIMEOUT.as_secs(),
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
