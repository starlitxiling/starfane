//! Hook payload normalization for `claude`, `codex`, and `agy` (contracts §3.2).

use crate::error::AdapterError;
use baton_proto::{
    id::{new_id, prefix},
    truncate_chars, AgentKind, Confidence, EmitRequest, EnvCtx, EventDetail, EventId, EventKind,
    NormalizedEvent, PaneId, SessionId, PROTO_VERSION,
};
use serde_json::Value;

const MAX_TOOL_SUMMARY_CHARS: usize = 200;
const MAX_LAST_MESSAGE_CHARS: usize = 500;

/// Normalize a raw hook payload into a [`NormalizedEvent`] per contracts §3.2.
pub fn normalize_hook(
    agent: AgentKind,
    event_name: &str,
    payload: &Value,
    env: &EnvCtx,
    recv_ts_ms: u64,
) -> Result<NormalizedEvent, AdapterError> {
    match agent {
        AgentKind::Claude => normalize_claude(event_name, payload, env, recv_ts_ms),
        AgentKind::Codex => normalize_codex(event_name, payload, env, recv_ts_ms),
        AgentKind::Agy => normalize_agy(event_name, payload, env, recv_ts_ms),
    }
}

/// Convenience wrapper normalizing an [`EmitRequest`].
pub fn normalize_request(req: &EmitRequest) -> Result<NormalizedEvent, AdapterError> {
    let event_name = req.event_name().ok_or(AdapterError::MissingField {
        agent: req.agent,
        field: match req.agent {
            AgentKind::Agy => "hookEventName",
            AgentKind::Claude | AgentKind::Codex => "hook_event_name",
        },
    })?;
    normalize_hook(
        req.agent,
        event_name,
        &req.payload,
        &req.env,
        req.recv_ts_ms,
    )
}

fn normalize_claude(
    event_name: &str,
    payload: &Value,
    env: &EnvCtx,
    recv_ts_ms: u64,
) -> Result<NormalizedEvent, AdapterError> {
    let session_id = required_str(payload, "session_id", AgentKind::Claude)?;
    let notification_type = opt_nonempty_str(payload, "notification_type");

    let kind = match event_name {
        "SessionStart" => EventKind::SessionStart,
        "UserPromptSubmit" => EventKind::Working,
        "PermissionRequest" => EventKind::NeedsPermission,
        "Notification" => match notification_type.as_deref() {
            Some("permission_prompt") => EventKind::NeedsPermission,
            Some("elicitation_dialog") => EventKind::NeedsInput,
            Some("idle_prompt") => EventKind::Ignored,
            _ => EventKind::Ignored,
        },
        "Stop" => EventKind::TurnComplete,
        "StopFailure" => EventKind::Error,
        "SessionEnd" => EventKind::SessionEnd,
        "SubagentStart" => EventKind::SubagentStart,
        "SubagentStop" | "TeammateIdle" => EventKind::SubagentEnd,
        _ => EventKind::Ignored,
    };

    let error = match (
        opt_nonempty_str(payload, "error"),
        opt_nonempty_str(payload, "error_details"),
    ) {
        (Some(err), Some(details)) => Some(format!("{err}: {details}")),
        (Some(err), None) => Some(err),
        (None, Some(details)) => Some(details),
        (None, None) => None,
    };

    let detail = EventDetail {
        tool_name: opt_nonempty_str(payload, "tool_name"),
        tool_summary: payload.get("tool_input").and_then(summarize_tool_input),
        last_message: opt_nonempty_str(payload, "last_assistant_message")
            .or_else(|| {
                if event_name == "Notification" {
                    opt_nonempty_str(payload, "message")
                } else {
                    None
                }
            })
            .map(|s| truncate_chars(&s, MAX_LAST_MESSAGE_CHARS)),
        notification_type,
        termination_reason: None,
        fully_idle: None,
        continuation_active: None,
        error,
        agent_type: opt_nonempty_str(payload, "agent_type"),
    };

    Ok(NormalizedEvent {
        v: PROTO_VERSION,
        id: EventId::new(new_id(prefix::EVENT)),
        ts_ms: recv_ts_ms,
        agent: AgentKind::Claude,
        kind,
        confidence: Confidence::Exact,
        session_id: SessionId::new(session_id),
        subagent_id: opt_nonempty_str(payload, "agent_id"),
        pane: env.tmux_pane.clone().map(PaneId::new),
        cwd: opt_nonempty_str(payload, "cwd"),
        model: opt_nonempty_str(payload, "model"),
        native_event: event_name.to_string(),
        transcript_path: opt_nonempty_str(payload, "transcript_path"),
        bind_token: env.bind_token.clone(),
        detail,
    })
}

fn normalize_codex(
    event_name: &str,
    payload: &Value,
    env: &EnvCtx,
    recv_ts_ms: u64,
) -> Result<NormalizedEvent, AdapterError> {
    let session_id = required_str(payload, "session_id", AgentKind::Codex)?;

    let kind = match event_name {
        "SessionStart" => EventKind::SessionStart,
        "UserPromptSubmit" => EventKind::Working,
        "PermissionRequest" => EventKind::NeedsPermission,
        "PreToolUse" => EventKind::ToolStart,
        "PostToolUse" => EventKind::ToolEnd,
        "Stop" => EventKind::TurnComplete,
        "Interrupt" => EventKind::Interrupted,
        "SessionEnd" => EventKind::SessionEnd,
        "SubagentStart" => EventKind::SubagentStart,
        "SubagentStop" => EventKind::SubagentEnd,
        _ => EventKind::Ignored,
    };

    let detail = EventDetail {
        tool_name: opt_nonempty_str(payload, "tool_name"),
        tool_summary: payload.get("tool_input").and_then(summarize_tool_input),
        last_message: opt_nonempty_str(payload, "last_assistant_message")
            .map(|s| truncate_chars(&s, MAX_LAST_MESSAGE_CHARS)),
        notification_type: None,
        termination_reason: None,
        fully_idle: None,
        continuation_active: None,
        error: opt_nonempty_str(payload, "error"),
        agent_type: opt_nonempty_str(payload, "agent_type"),
    };

    Ok(NormalizedEvent {
        v: PROTO_VERSION,
        id: EventId::new(new_id(prefix::EVENT)),
        ts_ms: recv_ts_ms,
        agent: AgentKind::Codex,
        kind,
        confidence: Confidence::Exact,
        session_id: SessionId::new(session_id),
        subagent_id: opt_nonempty_str(payload, "agent_id"),
        pane: env.tmux_pane.clone().map(PaneId::new),
        cwd: opt_nonempty_str(payload, "cwd"),
        model: opt_nonempty_str(payload, "model"),
        native_event: event_name.to_string(),
        transcript_path: opt_nonempty_str(payload, "transcript_path"),
        bind_token: env.bind_token.clone(),
        detail,
    })
}

fn normalize_agy(
    event_name: &str,
    payload: &Value,
    env: &EnvCtx,
    recv_ts_ms: u64,
) -> Result<NormalizedEvent, AdapterError> {
    let conversation_id = required_str(payload, "conversationId", AgentKind::Agy)?;
    let termination_reason = opt_nonempty_str(payload, "terminationReason");
    let fully_idle = payload.get("fullyIdle").and_then(Value::as_bool);
    let continuation_active = payload.get("continuationActive").and_then(Value::as_bool);

    let kind = match event_name {
        "SessionStart" => EventKind::SessionStart,
        "PreInvocation" => EventKind::Working,
        "PostInvocation" => EventKind::Ignored,
        "PreToolUse" => EventKind::ToolStart,
        "PostToolUse" => EventKind::ToolEnd,
        "Stop" => match termination_reason.as_deref() {
            Some("ERROR" | "MAX_INVOCATIONS" | "MAX_TOKEN_BUDGET_EXCEEDED") => EventKind::Error,
            Some("USER_CANCELED") => EventKind::Interrupted,
            _ if fully_idle == Some(true) && continuation_active != Some(true) => {
                EventKind::TurnComplete
            }
            _ => EventKind::Working,
        },
        _ => EventKind::Ignored,
    };

    let tool_call = payload.get("toolCall");
    let tool_name = tool_call.and_then(|tc| opt_nonempty_str(tc, "name"));
    let tool_summary = tool_call
        .and_then(|tc| tc.get("args"))
        .and_then(summarize_tool_input);

    let detail = EventDetail {
        tool_name,
        tool_summary,
        last_message: None,
        notification_type: None,
        termination_reason,
        fully_idle,
        continuation_active,
        error: opt_nonempty_str(payload, "error"),
        agent_type: None,
    };

    Ok(NormalizedEvent {
        v: PROTO_VERSION,
        id: EventId::new(new_id(prefix::EVENT)),
        ts_ms: recv_ts_ms,
        agent: AgentKind::Agy,
        kind,
        confidence: Confidence::Exact,
        session_id: SessionId::new(conversation_id),
        subagent_id: None,
        pane: env.tmux_pane.clone().map(PaneId::new),
        cwd: opt_nonempty_str(payload, "cwd"),
        model: opt_nonempty_str(payload, "modelName"),
        native_event: event_name.to_string(),
        transcript_path: opt_nonempty_str(payload, "transcriptPath"),
        bind_token: env.bind_token.clone(),
        detail,
    })
}

fn required_str(
    payload: &Value,
    field: &'static str,
    agent: AgentKind,
) -> Result<String, AdapterError> {
    opt_nonempty_str(payload, field).ok_or(AdapterError::MissingField { agent, field })
}

fn opt_nonempty_str(payload: &Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn summarize_tool_input(v: &Value) -> Option<String> {
    if v.is_null() {
        return None;
    }
    let raw = if let Some(obj) = v.as_object() {
        for key in ["command", "CommandLine", "file_path", "Path", "TargetFile"] {
            if let Some(s) = obj.get(key).and_then(Value::as_str) {
                let single = collapse_whitespace(s);
                if !single.is_empty() {
                    return Some(truncate_chars(&single, MAX_TOOL_SUMMARY_CHARS));
                }
            }
        }
        serde_json::to_string(v).ok()?
    } else if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(v).ok()?
    };

    let single = collapse_whitespace(&raw);
    if single.is_empty() {
        None
    } else {
        Some(truncate_chars(&single, MAX_TOOL_SUMMARY_CHARS))
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
