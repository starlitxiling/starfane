//! Unix-socket server: JSON-line `ClientFrame`s (contracts §1.7).

use crate::core::Core;
use baton_proto::{encode_line, ApiError, ClientFrame, DaemonResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Max accepted line length (emit payloads are capped at 1 MiB + envelope).
const MAX_LINE: usize = 2 * 1024 * 1024;

pub async fn serve(core: Core, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let core = core.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(core, stream).await {
                        log::debug!("uds connection ended with error: {e}");
                    }
                });
            }
            Err(e) => {
                log::error!("uds accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_conn(core: Core, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        if line.len() > MAX_LINE {
            let resp = DaemonResponse::err(0, ApiError::bad_request("frame too large"));
            write_resp(&mut write, &resp).await?;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<ClientFrame>(trimmed) {
            Ok(ClientFrame::Emit(req)) => {
                let core = core.clone();
                tokio::spawn(async move { core.ingest(req).await });
            }
            Ok(ClientFrame::Req { id, cmd }) => {
                let resp = DaemonResponse::from_result(id, core.handle(cmd).await);
                write_resp(&mut write, &resp).await?;
            }
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
                    .unwrap_or(0);
                let resp =
                    DaemonResponse::err(id, ApiError::bad_request(format!("bad frame: {e}")));
                write_resp(&mut write, &resp).await?;
            }
        }
    }
}

async fn write_resp(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &DaemonResponse,
) -> std::io::Result<()> {
    let line = encode_line(resp).map_err(std::io::Error::other)?;
    write.write_all(line.as_bytes()).await?;
    write.flush().await
}
