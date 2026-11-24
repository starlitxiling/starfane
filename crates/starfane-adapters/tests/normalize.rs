//! Contract tests asserting every hook fixture in `tests/fixtures/` (contracts §3.2 & §10).

use serde_json::{json, Value};
use starfane_adapters::{normalize_hook, normalize_request, AdapterError};
use starfane_proto::{AgentKind, Confidence, EmitRequest, EnvCtx, EventKind, PROTO_VERSION};
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

struct Case {
    agent: AgentKind,
    rel_path: &'static str,
    event_name: &'static str,
    kind: EventKind,
    confidence: Confidence,
    subagent_id: Option<&'static str>,
    model: Option<&'static str>,
}

const CASES: &[Case] = &[
    // ---- Claude (16) ----
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/SessionStart.json",
        event_name: "SessionStart",
        kind: EventKind::SessionStart,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("claude-opus-5-5"),
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/UserPromptSubmit.json",
        event_name: "UserPromptSubmit",
        kind: EventKind::Working,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/PermissionRequest.json",
        event_name: "PermissionRequest",
        kind: EventKind::NeedsPermission,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/PermissionRequest__subagent.json",
        event_name: "PermissionRequest",
        kind: EventKind::NeedsPermission,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-claude-01"),
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/Notification__permission_prompt.json",
        event_name: "Notification",
        kind: EventKind::NeedsPermission,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/Notification__idle_prompt.json",
        event_name: "Notification",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/Notification__elicitation_dialog.json",
        event_name: "Notification",
        kind: EventKind::NeedsInput,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/Notification__auth_success.json",
        event_name: "Notification",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/Stop.json",
        event_name: "Stop",
        kind: EventKind::TurnComplete,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/StopFailure.json",
        event_name: "StopFailure",
        kind: EventKind::Error,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/SessionEnd.json",
        event_name: "SessionEnd",
        kind: EventKind::SessionEnd,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/SubagentStart.json",
        event_name: "SubagentStart",
        kind: EventKind::SubagentStart,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-claude-01"),
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/SubagentStop.json",
        event_name: "SubagentStop",
        kind: EventKind::SubagentEnd,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-claude-01"),
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/TeammateIdle.json",
        event_name: "TeammateIdle",
        kind: EventKind::SubagentEnd,
        confidence: Confidence::Exact,
        subagent_id: Some("teammate-01"),
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/PreToolUse__subagent.json",
        event_name: "PreToolUse",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-claude-01"),
        model: None,
    },
    Case {
        agent: AgentKind::Claude,
        rel_path: "claude/UnknownEvent.json",
        event_name: "FutureClaudeEvent",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: None,
    },
    // ---- Codex (12) ----
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/SessionStart.json",
        event_name: "SessionStart",
        kind: EventKind::SessionStart,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/UserPromptSubmit.json",
        event_name: "UserPromptSubmit",
        kind: EventKind::Working,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/PermissionRequest.json",
        event_name: "PermissionRequest",
        kind: EventKind::NeedsPermission,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/PermissionRequest__subagent.json",
        event_name: "PermissionRequest",
        kind: EventKind::NeedsPermission,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-codex-01"),
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/PreToolUse.json",
        event_name: "PreToolUse",
        kind: EventKind::ToolStart,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/PostToolUse.json",
        event_name: "PostToolUse",
        kind: EventKind::ToolEnd,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/Stop.json",
        event_name: "Stop",
        kind: EventKind::TurnComplete,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/Interrupt.json",
        event_name: "Interrupt",
        kind: EventKind::Interrupted,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/SessionEnd.json",
        event_name: "SessionEnd",
        kind: EventKind::SessionEnd,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/SubagentStart.json",
        event_name: "SubagentStart",
        kind: EventKind::SubagentStart,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-codex-01"),
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/SubagentStop.json",
        event_name: "SubagentStop",
        kind: EventKind::SubagentEnd,
        confidence: Confidence::Exact,
        subagent_id: Some("sub-codex-01"),
        model: Some("gpt-6.2-sol"),
    },
    Case {
        agent: AgentKind::Codex,
        rel_path: "codex/UnknownEvent.json",
        event_name: "FutureCodexEvent",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gpt-6.2-sol"),
    },
    // ---- Agy (13) ----
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/SessionStart.json",
        event_name: "SessionStart",
        kind: EventKind::SessionStart,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/PreInvocation.json",
        event_name: "PreInvocation",
        kind: EventKind::Working,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/PostInvocation.json",
        event_name: "PostInvocation",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/PreToolUse.json",
        event_name: "PreToolUse",
        kind: EventKind::ToolStart,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/PostToolUse.json",
        event_name: "PostToolUse",
        kind: EventKind::ToolEnd,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__idle.json",
        event_name: "Stop",
        kind: EventKind::TurnComplete,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__not_idle.json",
        event_name: "Stop",
        kind: EventKind::Working,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__continuation.json",
        event_name: "Stop",
        kind: EventKind::Working,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__user_canceled.json",
        event_name: "Stop",
        kind: EventKind::Interrupted,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__error.json",
        event_name: "Stop",
        kind: EventKind::Error,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__max_invocations.json",
        event_name: "Stop",
        kind: EventKind::Error,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/Stop__max_token_budget.json",
        event_name: "Stop",
        kind: EventKind::Error,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
    Case {
        agent: AgentKind::Agy,
        rel_path: "agy/UnknownEvent.json",
        event_name: "FutureAgyEvent",
        kind: EventKind::Ignored,
        confidence: Confidence::Exact,
        subagent_id: None,
        model: Some("gemini-4-neon-high"),
    },
];

#[test]
fn all_41_fixtures_are_covered_and_normalized_accurately() {
    let root = fixtures_root();
    let mut disk_files = Vec::new();
    for dir in ["claude", "codex", "agy"] {
        for entry in fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                disk_files.push(format!(
                    "{dir}/{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    disk_files.sort();
    assert_eq!(disk_files.len(), 41, "expected 41 JSON fixtures on disk");
    assert_eq!(
        CASES.len(),
        disk_files.len(),
        "every fixture file must have a test case"
    );

    let env = EnvCtx {
        tmux_pane: Some("%12".into()),
        cwd: "/wrong/hooks/dir".into(),
        ..EnvCtx::default()
    };

    for case in CASES {
        let raw = fs::read_to_string(root.join(case.rel_path))
            .unwrap_or_else(|e| panic!("failed reading {}: {e}", case.rel_path));
        let payload: Value = serde_json::from_str(&raw).unwrap();

        // Exercise both normalize_hook and normalize_request (event name inferred from payload).
        let ev = normalize_hook(
            case.agent,
            case.event_name,
            &payload,
            &env,
            1_700_000_000_000,
        )
        .unwrap_or_else(|e| panic!("normalize_hook failed for {}: {e}", case.rel_path));

        assert_eq!(ev.v, PROTO_VERSION);
        assert_eq!(ev.agent, case.agent, "agent mismatch on {}", case.rel_path);
        assert_eq!(
            ev.kind, case.kind,
            "EventKind mismatch on {}",
            case.rel_path
        );
        assert_eq!(
            ev.confidence, case.confidence,
            "Confidence mismatch on {}",
            case.rel_path
        );
        assert_eq!(
            ev.subagent_id.as_deref(),
            case.subagent_id,
            "subagent_id mismatch on {}",
            case.rel_path
        );
        assert_eq!(
            ev.model.as_deref(),
            case.model,
            "model mismatch on {}",
            case.rel_path
        );
        assert_eq!(ev.native_event, case.event_name);
        assert_eq!(ev.pane.as_ref().map(|p| p.as_str()), Some("%12"));
        assert_eq!(
            ev.cwd.as_deref(),
            Some("/Users/demo/work/starfane"),
            "cwd must come from payload, not env.cwd ({})",
            case.rel_path
        );

        let req = EmitRequest {
            v: PROTO_VERSION,
            agent: case.agent,
            hook_event: None,
            payload,
            env: env.clone(),
            recv_ts_ms: 1_700_000_000_000,
        };
        let ev2 = normalize_request(&req).unwrap();
        assert_eq!(ev2.kind, case.kind);
        assert_eq!(ev2.native_event, case.event_name);
    }
}

#[test]
fn missing_session_field_returns_missing_field_error() {
    let env = EnvCtx::default();
    let err_claude = normalize_hook(AgentKind::Claude, "Stop", &json!({}), &env, 1).unwrap_err();
    assert!(matches!(
        err_claude,
        AdapterError::MissingField {
            agent: AgentKind::Claude,
            field: "session_id"
        }
    ));

    let err_codex = normalize_hook(AgentKind::Codex, "Stop", &json!({}), &env, 1).unwrap_err();
    assert!(matches!(
        err_codex,
        AdapterError::MissingField {
            agent: AgentKind::Codex,
            field: "session_id"
        }
    ));

    let err_agy = normalize_hook(AgentKind::Agy, "Stop", &json!({}), &env, 1).unwrap_err();
    assert!(matches!(
        err_agy,
        AdapterError::MissingField {
            agent: AgentKind::Agy,
            field: "conversationId"
        }
    ));
}
