//! Synchronous UDS client used by the CLI and `starfane mcp` (no tokio).

use starfane_proto::{encode_line, ApiError, ClientFrame, DaemonCommand, DaemonResponse};
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-call read timeout (pipeline operations can take a few seconds).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const AUTOSTART_WAIT: Duration = Duration::from_secs(2);
const AUTOSTART_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum ClientError {
    /// Daemon not reachable (and autostart failed or disabled).
    Unavailable(String),
    Io(io::Error),
    Protocol(String),
    Api(ApiError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Unavailable(m) => write!(f, "starfane daemon unavailable: {m}"),
            ClientError::Io(e) => write!(f, "io error talking to daemon: {e}"),
            ClientError::Protocol(m) => write!(f, "protocol error: {m}"),
            ClientError::Api(e) => write!(f, "{}", e.message),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub struct DaemonClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl DaemonClient {
    /// Connect; with `autostart`, spawn `starfane daemon --detach` and retry every
    /// 50ms for up to 2s.
    pub fn connect(socket: &Path, autostart: bool) -> Result<Self, ClientError> {
        match UnixStream::connect(socket) {
            Ok(s) => return Self::from_stream(s),
            Err(e) if !autostart => {
                return Err(ClientError::Unavailable(format!(
                    "{}: {e}",
                    socket.display()
                )))
            }
            Err(_) => {}
        }
        spawn_daemon_detached()
            .map_err(|e| ClientError::Unavailable(format!("failed to start daemon: {e}")))?;
        let deadline = Instant::now() + AUTOSTART_WAIT;
        loop {
            match UnixStream::connect(socket) {
                Ok(s) => return Self::from_stream(s),
                Err(e) if Instant::now() >= deadline => {
                    return Err(ClientError::Unavailable(format!(
                        "daemon did not come up at {}: {e}",
                        socket.display()
                    )))
                }
                Err(_) => std::thread::sleep(AUTOSTART_POLL),
            }
        }
    }

    fn from_stream(stream: UnixStream) -> Result<Self, ClientError> {
        stream.set_read_timeout(Some(CALL_TIMEOUT))?;
        let writer = stream.try_clone()?;
        Ok(DaemonClient {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    /// Send one command and wait for its response.
    pub fn call(&mut self, cmd: DaemonCommand) -> Result<serde_json::Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode_line(&ClientFrame::Req { id, cmd })
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf)?;
            if n == 0 {
                return Err(ClientError::Unavailable(
                    "daemon closed the connection".into(),
                ));
            }
            let resp: DaemonResponse = serde_json::from_str(buf.trim_end())
                .map_err(|e| ClientError::Protocol(format!("bad response: {e}")))?;
            if resp.id != id {
                // Stale response from an earlier timed-out call on this connection.
                continue;
            }
            return resp.into_result().map_err(ClientError::Api);
        }
    }

    /// Call and deserialize the response data.
    pub fn call_as<T: serde::de::DeserializeOwned>(
        &mut self,
        cmd: DaemonCommand,
    ) -> Result<T, ClientError> {
        let v = self.call(cmd)?;
        serde_json::from_value(v).map_err(|e| ClientError::Protocol(format!("bad data: {e}")))
    }
}

/// Spawn `<current_exe> daemon --detach` fully detached (null stdio, not awaited).
pub fn spawn_daemon_detached() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .args(["daemon", "--detach"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}
