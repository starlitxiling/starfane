//! MCP tool definitions (contracts §7.3): names, descriptions and JSON Schemas.
//!
//! `baton mcp` serves exactly these via `tools/list`; the snapshot test in
//! `tests/mcp_schemas.rs` guards against accidental drift.

use serde_json::{json, Value};

/// MCP protocol versions the hand-rolled server echoes back.
pub const MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
/// Version returned when the client requests an unsupported one.
pub const MCP_LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Tool names in `tools/list` order.
pub const MCP_TOOL_NAMES: [&str; 9] = [
    "list_sessions",
    "create_handoff",
    "read_inbox",
    "ack_handoff",
    "submit_review",
    "submit_falsifier_verdict",
    "submit_triage",
    "report_milestone",
    "start_pipeline",
];

/// Pick the protocol version to answer `initialize` with.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| MCP_PROTOCOL_VERSIONS.iter().copied().find(|v| *v == r))
        .unwrap_or(MCP_LATEST_PROTOCOL_VERSION)
}

fn string() -> Value {
    json!({"type": "string"})
}
fn string_desc(d: &str) -> Value {
    json!({"type": "string", "description": d})
}
fn boolean() -> Value {
    json!({"type": "boolean"})
}
fn line() -> Value {
    json!({"type": "integer", "minimum": 1})
}
fn str_enum(values: &[&str]) -> Value {
    json!({"type": "string", "enum": values})
}
fn string_array() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}
fn array_of(items: Value) -> Value {
    json!({"type": "array", "items": items})
}
fn confidence() -> Value {
    json!({"type": "number", "minimum": 0.0, "maximum": 1.0})
}
fn from_session() -> Value {
    string_desc("Optional explicit caller session id; overrides the cached identity.")
}

fn stage_enum() -> Value {
    str_enum(&[
        "plan",
        "implement",
        "test_review",
        "report",
        "triage",
        "deep",
    ])
}

fn severity_enum() -> Value {
    str_enum(&["blocker", "major", "minor", "nit"])
}

fn code_ref() -> Value {
    json!({
        "type": "object",
        "properties": {"file": string(), "line": line(), "note": string()},
        "required": ["file", "line"]
    })
}

fn evidence_quote() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": string(),
            "line_start": line(),
            "line_end": line(),
            "quote": string_desc("Verbatim lines copied from the file at the reviewed commit.")
        },
        "required": ["file", "line_start", "line_end", "quote"]
    })
}

fn evidence_ref() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": {"const": "code"},
                    "file": string(),
                    "line_start": line(),
                    "line_end": line(),
                    "quote": string()
                },
                "required": ["type", "file", "line_start", "line_end", "quote"]
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "log"},
                    "path": string_desc("Log file path, relative to repo root or absolute."),
                    "line": line(),
                    "quote": string_desc("Verbatim log text.")
                },
                "required": ["type", "path", "quote"]
            }
        ]
    })
}

fn test_run() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cmd": string(),
            "outcome": str_enum(&["pass", "fail", "not_run"]),
            "output_tail": string_desc("Last ≤2000 chars of output.")
        },
        "required": ["cmd", "outcome"]
    })
}

fn stage_ref() -> Value {
    json!({
        "type": "object",
        "properties": {"pipeline_id": string(), "stage": stage_enum()},
        "required": ["pipeline_id", "stage"]
    })
}

fn review_finding() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": string_desc("Leave empty; assigned by baton."),
            "category": str_enum(&["correctness", "concurrency", "security", "perf", "api", "style"]),
            "severity": severity_enum(),
            "confidence": confidence(),
            "claim": string_desc("One falsifiable sentence."),
            "location": {
                "type": "object",
                "properties": {
                    "file": string(), "line_start": line(), "line_end": line(), "symbol": string()
                },
                "required": ["file", "line_start", "line_end"]
            },
            "evidence": json!({"type": "array", "items": evidence_quote(), "minItems": 1}),
            "trigger_path": array_of(code_ref()),
            "preconditions": string(),
            "checked_guards": array_of(code_ref()),
            "observable_impact": string(),
            "verification_method": str_enum(&["code_trace", "static_check", "repro_test"]),
            "repro": {
                "type": "object",
                "properties": {"kind": str_enum(&["test", "input", "none"]), "content": string()},
                "required": ["kind"]
            },
            "suggested_fix": string()
        },
        "required": [
            "category", "severity", "confidence", "claim", "location", "evidence",
            "verification_method"
        ]
    })
}

fn review_report() -> Value {
    json!({
        "type": "object",
        "description": "review.v1 (header may be omitted). Zero findings with checked_areas is a valid, encouraged result.",
        "properties": {
            "depth": str_enum(&["D0", "D1", "D2", "D3"]),
            "base": string_desc("Base commit of the reviewed range."),
            "head": string_desc("Reviewed commit; evidence is verified against this tree."),
            "scope_files": string_array(),
            "checked_areas": string_array(),
            "findings": array_of(review_finding()),
            "summary": string()
        },
        "required": ["depth", "base", "head"]
    })
}

fn plan_doc() -> Value {
    json!({
        "type": "object",
        "description": "plan.v1 (header may be omitted).",
        "properties": {
            "goal": string(),
            "design_md": string(),
            "tasks": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {"id": string(), "title": string(), "files": string_array()},
                    "required": ["id", "title"]
                }
            },
            "acceptance": json!({"type": "array", "items": string(), "minItems": 1}),
            "test_plan": json!({"type": "array", "items": string(), "minItems": 1}),
            "risks": string_array(),
            "native_strategy": str_enum(&["swarm", "boost", "teamwork", "subagents"])
        },
        "required": ["goal", "design_md", "tasks", "acceptance", "test_plan"]
    })
}

fn triage_report() -> Value {
    json!({
        "type": "object",
        "description": "triage.v1 (header may be omitted). Every conclusion is a hypothesis, never a confirmed root cause.",
        "properties": {
            "symptom": string(),
            "hypotheses": {
                "type": "array",
                "minItems": 1,
                "maxItems": 2,
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "hypothesis"},
                        "claim": string(),
                        "confidence": confidence(),
                        "evidence_refs": json!({"type": "array", "items": evidence_ref(), "minItems": 1}),
                        "how_to_confirm": string()
                    },
                    "required": ["claim", "confidence", "evidence_refs", "how_to_confirm"]
                }
            },
            "mitigation": string(),
            "probes": string_array(),
            "elapsed_secs": json!({"type": "integer", "minimum": 0})
        },
        "required": ["symptom", "hypotheses"]
    })
}

fn rca_report() -> Value {
    json!({
        "type": "object",
        "description": "rca.v1 (header may be omitted).",
        "properties": {
            "triage_id": string(),
            "root_cause": {
                "type": "object",
                "properties": {
                    "claim": string(),
                    "trigger_path": array_of(code_ref()),
                    "evidence": array_of(evidence_ref())
                },
                "required": ["claim", "trigger_path", "evidence"]
            },
            "repro_before": test_run(),
            "repro_note": string(),
            "fix": {
                "type": "object",
                "properties": {"branch": string(), "commit": string(), "files": string_array()},
                "required": ["branch", "files"]
            },
            "verification_after": test_run(),
            "regression": array_of(test_run()),
            "ruled_out": array_of(json!({
                "type": "object",
                "properties": {"hypothesis": string(), "evidence": array_of(evidence_ref())},
                "required": ["hypothesis", "evidence"]
            }))
        },
        "required": ["root_cause", "repro_before"]
    })
}

fn final_report() -> Value {
    json!({
        "type": "object",
        "description": "report.v1 (header may be omitted).",
        "properties": {
            "summary_md": string(),
            "changes": string_array(),
            "decisions": string_array(),
            "follow_ups": string_array(),
            "doc_path": string()
        },
        "required": ["summary_md"]
    })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        }
    })
}

/// The 9 MCP tools (`tools/list` payload entries), in [`MCP_TOOL_NAMES`] order.
pub fn mcp_tool_schemas() -> Value {
    let tools = vec![
        tool(
            "list_sessions",
            "List agent sessions known to baton (agent, pane, state, model, pipeline stage).",
            json!({
                "project": string_desc("Git root path or project name filter."),
                "include_subagents": boolean()
            }),
            &[],
        ),
        tool(
            "create_handoff",
            "Hand work or context to another agent session. The body is stored by baton; the target only receives a one-line pointer. Builders finish the implement stage with intent=review and a self_test.",
            json!({
                "to": {
                    "type": "object",
                    "properties": {
                        "session_id": string(),
                        "pane": string_desc("tmux pane id, e.g. %12"),
                        "agent": str_enum(&["claude", "codex", "agy"]),
                        "stage": stage_ref()
                    }
                },
                "intent": str_enum(&["review", "implement", "continue", "ask", "fyi"]),
                "mode": str_enum(&["read_only", "write"]),
                "summary": json!({"type": "string", "maxLength": 500}),
                "body_md": string(),
                "todo": string_array(),
                "refs": string_array(),
                "self_test": test_run(),
                "from_session": from_session()
            }),
            &["to", "intent", "mode", "summary", "body_md"],
        ),
        tool(
            "read_inbox",
            "Fetch tasks, handoffs, messages and review requests addressed to this session.",
            json!({
                "mark_delivered": json!({"type": "boolean", "default": true}),
                "from_session": from_session()
            }),
            &[],
        ),
        tool(
            "ack_handoff",
            "Acknowledge a handoff (acked = started, done = finished).",
            json!({
                "handoff_id": string(),
                "status": str_enum(&["acked", "done"]),
                "note": string(),
                "from_session": from_session()
            }),
            &["handoff_id", "status"],
        ),
        tool(
            "submit_review",
            "Submit a review.v1 report. Every finding needs verbatim evidence; quotes are mechanically verified against the head commit and fabricated ones are rejected.",
            json!({"review": review_report(), "from_session": from_session()}),
            &["review"],
        ),
        tool(
            "submit_falsifier_verdict",
            "As falsifier, record your attempt to disprove a finding. 'disproved' requires verbatim counter_evidence; without it the verdict becomes needs_human.",
            json!({
                "review_id": string(),
                "finding_id": string(),
                "verdict": str_enum(&["confirmed", "needs_human", "downgraded", "disproved"]),
                "to": severity_enum(),
                "rationale": string(),
                "counter_evidence": array_of(evidence_quote()),
                "from_session": from_session()
            }),
            &["review_id", "finding_id", "verdict", "rationale"],
        ),
        tool(
            "submit_triage",
            "Submit Phase A fast-triage results (triage.v1): at most 2 evidence-backed hypotheses plus mitigation.",
            json!({"triage": triage_report(), "from_session": from_session()}),
            &["triage"],
        ),
        tool(
            "report_milestone",
            "Report a stage milestone: plan (plan.v1), rca (rca.v1), final_report (report.v1), or a progress note.",
            json!({
                "kind": str_enum(&["plan", "rca", "final_report", "progress"]),
                "plan": plan_doc(),
                "rca": rca_report(),
                "report": final_report(),
                "text": string_desc("Required for kind=progress."),
                "notify_human": boolean(),
                "from_session": from_session()
            }),
            &["kind"],
        ),
        tool(
            "start_pipeline",
            "Start a Feature or Debug pipeline from the user's words. Pass the user's phrasing in utterance (e.g. 'plan with claude, implement with codex, review with agy /swarm'); bindings use shorthand '<agent>[:<model>][@<effort>][ /<native>]'. The human must confirm before anything runs.",
            json!({
                "playbook": str_enum(&["feature", "debug"]),
                "goal": string(),
                "utterance": string(),
                "bindings": {
                    "type": "object",
                    "description": "Stage name -> binding shorthand.",
                    "additionalProperties": string()
                },
                "project_root": string(),
                "from_session": from_session()
            }),
            &["goal"],
        ),
    ];
    Value::Array(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        assert_eq!(negotiate_protocol_version(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_protocol_version(Some("1999-01-01")), "2025-06-18");
        assert_eq!(negotiate_protocol_version(None), "2025-06-18");
    }

    #[test]
    fn names_match_constant() {
        let tools = mcp_tool_schemas();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, MCP_TOOL_NAMES);
    }
}
