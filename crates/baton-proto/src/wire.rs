//! UDS JSON-line wire protocol (contracts §1.7).
//!
//! * emit: connect → write one `ClientFrame::Emit` line → close (no reply).
//! * request: connect → write `ClientFrame::Req` lines → read one
//!   `DaemonResponse` line per request; connections may be reused.

use crate::agent::AgentKind;
use crate::api::DaemonCommand;
use crate::event::EnvCtx;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmitRequest {
    pub v: u32,
    pub agent: AgentKind,
    /// From `--event`; otherwise the daemon reads `hook_event_name` / `hookEventName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    /// Raw stdin JSON; `{"_raw": "<string>"}` when it was not valid JSON.
    pub payload: serde_json::Value,
    pub env: EnvCtx,
    pub recv_ts_ms: u64,
}

impl EmitRequest {
    /// Hook event name: explicit `hook_event`, else payload `hook_event_name`
    /// (claude/codex) or `hookEventName` (agy).
    pub fn event_name(&self) -> Option<&str> {
        self.hook_event.as_deref().or_else(|| {
            self.payload
                .get("hook_event_name")
                .or_else(|| self.payload.get("hookEventName"))
                .and_then(|v| v.as_str())
        })
    }
}

/// Transient frame; see the note on `DaemonCommand` for the allowed lint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    Emit(EmitRequest),
    Req { id: u64, cmd: DaemonCommand },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
    GateRejected,
    Unavailable,
    Internal,
}

impl ErrorCode {
    /// HTTP status mapping used by the REST layer (contracts §7.2).
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::BadRequest => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::GateRejected => 422,
            ErrorCode::Unavailable => 503,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, m)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl DaemonResponse {
    pub fn ok(id: u64, data: serde_json::Value) -> Self {
        DaemonResponse {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(id: u64, error: ApiError) -> Self {
        DaemonResponse {
            id,
            ok: false,
            data: None,
            error: Some(error),
        }
    }

    pub fn from_result(id: u64, r: Result<serde_json::Value, ApiError>) -> Self {
        match r {
            Ok(v) => Self::ok(id, v),
            Err(e) => Self::err(id, e),
        }
    }

    pub fn into_result(self) -> Result<serde_json::Value, ApiError> {
        if self.ok {
            Ok(self.data.unwrap_or(serde_json::Value::Null))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| ApiError::internal("error response without error body")))
        }
    }
}

/// Serialize as a single JSON line terminated by `\n`.
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn emit() -> EmitRequest {
        EmitRequest {
            v: 1,
            agent: AgentKind::Agy,
            hook_event: None,
            payload: json!({"hookEventName": "Stop", "conversationId": "c1"}),
            env: EnvCtx::default(),
            recv_ts_ms: 5,
        }
    }

    #[test]
    fn emit_frame_shape() {
        let line = encode_line(&ClientFrame::Emit(emit())).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["t"], "emit");
        assert_eq!(v["agent"], "agy");
        let back: ClientFrame = serde_json::from_value(v).unwrap();
        assert!(matches!(back, ClientFrame::Emit(e) if e.event_name() == Some("Stop")));
    }

    #[test]
    fn req_frame_and_response() {
        let f = ClientFrame::Req {
            id: 7,
            cmd: DaemonCommand::Ping,
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v, json!({"t":"req","id":7,"cmd":{"cmd":"ping"}}));
        let r = DaemonResponse::err(7, ApiError::not_found("x"));
        let rv = serde_json::to_value(&r).unwrap();
        assert_eq!(rv["error"]["code"], "not_found");
        assert!(rv.get("data").is_none());
        assert_eq!(r.into_result().unwrap_err().code.http_status(), 404);
        let ok = DaemonResponse::ok(1, json!({"pong": true}));
        assert_eq!(ok.into_result().unwrap()["pong"], true);
    }

    #[test]
    fn event_name_precedence() {
        let mut e = emit();
        e.hook_event = Some("PreInvocation".into());
        assert_eq!(e.event_name(), Some("PreInvocation"));
        e.hook_event = None;
        e.payload = json!({"hook_event_name": "Stop"});
        assert_eq!(e.event_name(), Some("Stop"));
        e.payload = json!({"_raw": "garbage"});
        assert_eq!(e.event_name(), None);
    }
}
