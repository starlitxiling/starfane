//! MCP tool schema contract tests (contracts §7.3 / §10).
//!
//! * exactly 10 tools with the frozen names and required fields;
//! * snapshot of the full schema at `tests/snapshots/mcp_tool_schemas.json`
//!   (regenerate with `STARFANE_UPDATE_SNAPSHOTS=1 cargo test -p starfane-proto`);
//! * example tool arguments deserialize into the proto types.

use serde_json::{json, Value};
use starfane_proto::*;
use std::path::PathBuf;

fn tools() -> Vec<Value> {
    mcp_tool_schemas().as_array().cloned().expect("array")
}

fn tool(name: &str) -> Value {
    tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("tool {name} missing"))
}

fn required(t: &Value) -> Vec<String> {
    t["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn ten_tools_with_frozen_names() {
    let names: Vec<String> = tools()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "list_sessions",
            "create_handoff",
            "read_inbox",
            "ack_handoff",
            "submit_review",
            "submit_falsifier_verdict",
            "submit_triage",
            "report_milestone",
            "start_pipeline",
            "spawn_session",
        ]
    );
}

#[test]
fn every_schema_is_well_formed() {
    for t in tools() {
        let name = t["name"].as_str().unwrap();
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{name} lacks description"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}");
        let props = schema["properties"].as_object().expect("properties");
        for r in required(&t) {
            assert!(
                props.contains_key(&r),
                "{name}: required {r} not in properties"
            );
        }
        // every tool except list_sessions acts as the caller and accepts from_session
        if name != "list_sessions" {
            assert!(
                props.contains_key("from_session"),
                "{name} lacks from_session"
            );
        }
    }
}

#[test]
fn required_fields_match_contract() {
    let cases: &[(&str, &[&str])] = &[
        ("list_sessions", &[]),
        (
            "create_handoff",
            &["to", "intent", "mode", "summary", "body_md"],
        ),
        ("read_inbox", &[]),
        ("ack_handoff", &["handoff_id", "status"]),
        ("submit_review", &["review"]),
        (
            "submit_falsifier_verdict",
            &["review_id", "finding_id", "verdict", "rationale"],
        ),
        ("submit_triage", &["triage"]),
        ("report_milestone", &["kind"]),
        ("start_pipeline", &["goal"]),
        ("spawn_session", &["agent"]),
    ];
    for (name, req) in cases {
        let got = required(&tool(name));
        let want: Vec<String> = req.iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn snapshot() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/mcp_tool_schemas.json");
    let current = serde_json::to_string_pretty(&mcp_tool_schemas()).unwrap() + "\n";
    if std::env::var("STARFANE_UPDATE_SNAPSHOTS").ok().as_deref() == Some("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &current).unwrap();
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {} ({e}); run with STARFANE_UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    assert_eq!(
        stored, current,
        "MCP schema drift: review the change, then STARFANE_UPDATE_SNAPSHOTS=1"
    );
}

// ---------------------------------------------------------------------------
// Example tool arguments must deserialize into proto types (header omitted).
// ---------------------------------------------------------------------------

fn sample_review_args() -> Value {
    json!({
        "review": {
            "depth": "D2",
            "base": "a1b2c3",
            "head": "d4e5f6",
            "findings": [{
                "category": "correctness",
                "severity": "major",
                "confidence": 0.8,
                "claim": "parse_header panics on empty input",
                "location": {"file": "src/a.rs", "line_start": 118, "line_end": 122, "symbol": "parse_header"},
                "evidence": [{"file": "src/a.rs", "line_start": 118, "line_end": 119, "quote": "let first = buf[0];"}],
                "trigger_path": [
                    {"file": "src/api.rs", "line": 40, "note": "entry handle_req"},
                    {"file": "src/a.rs", "line": 120}
                ],
                "preconditions": "empty request body",
                "checked_guards": [{"file": "src/api.rs", "line": 35, "note": "only content-type checked"}],
                "observable_impact": "worker thread panic",
                "verification_method": "code_trace"
            }]
        }
    })
}

#[test]
fn review_args_deserialize() {
    let args = sample_review_args();
    let r: ReviewReport = serde_json::from_value(args["review"].clone()).unwrap();
    assert_eq!(r.depth, ReviewDepth::D2);
    assert_eq!(r.findings.len(), 1);
    let f = &r.findings[0];
    assert!(f.id.is_empty());
    assert_eq!(f.severity, Severity::Major);
    assert_eq!(f.checked_guards.len(), 1);
    assert_eq!(r.header.v, PROTO_VERSION);
    let payload = ArtifactPayload::Review(r);
    let v = serde_json::to_value(&payload).unwrap();
    assert_eq!(v["kind"], "review");
    let back: ArtifactPayload = serde_json::from_value(v).unwrap();
    assert_eq!(back, payload);
}

#[test]
fn empty_review_is_valid_shape() {
    let r: ReviewReport = serde_json::from_value(json!({
        "depth": "D1", "base": "a", "head": "b", "checked_areas": ["src/lib.rs entrypoints"]
    }))
    .unwrap();
    assert!(r.findings.is_empty());
    assert_eq!(r.checked_areas.len(), 1);
}

#[test]
fn falsifier_args_deserialize() {
    let s: FalsifierSubmission = serde_json::from_value(json!({
        "review_id": "R-1", "finding_id": "F-1", "verdict": "disproved",
        "rationale": "length checked in middleware",
        "counter_evidence": [{"file": "src/mw.rs", "line_start": 10, "line_end": 12, "quote": "if body.is_empty() { return Err(..) }"}]
    }))
    .unwrap();
    assert_eq!(s.verdict, FalsifierVerdict::Disproved);
    assert_eq!(s.counter_evidence.len(), 1);
    assert_eq!(s.falsifier, AuthorRef::default());
}

#[test]
fn triage_args_deserialize() {
    let t: TriageReport = serde_json::from_value(json!({
        "symptom": "login returns 500 intermittently",
        "hypotheses": [{
            "claim": "connection pool exhausted",
            "confidence": 0.6,
            "evidence_refs": [
                {"type": "log", "path": "logs/api.log", "line": 1042, "quote": "pool timed out"},
                {"type": "code", "file": "src/db.rs", "line_start": 20, "line_end": 20, "quote": "max_connections: 5,"}
            ],
            "how_to_confirm": "raise pool size and replay"
        }],
        "mitigation": "restart api pods"
    }))
    .unwrap();
    assert_eq!(t.hypotheses[0].kind, HypothesisKind::Hypothesis);
    assert_eq!(t.hypotheses[0].evidence_refs.len(), 2);
}

#[test]
fn milestone_payloads_deserialize() {
    let plan: PlanDoc = serde_json::from_value(json!({
        "goal": "mute command", "design_md": "...",
        "tasks": [{"id": "t1", "title": "parser"}],
        "acceptance": ["mute 30m works"], "test_plan": ["unit tests"],
        "native_strategy": "teamwork"
    }))
    .unwrap();
    assert_eq!(plan.native_strategy, Some(NativeMode::Teamwork));

    let rca: RcaReport = serde_json::from_value(json!({
        "root_cause": {
            "claim": "pool too small",
            "trigger_path": [{"file": "src/db.rs", "line": 20}],
            "evidence": [{"type": "log", "path": "logs/api.log", "quote": "pool timed out"}]
        },
        "repro_before": {"cmd": "cargo test pool", "outcome": "fail"},
        "fix": {"branch": "starfane/P-1", "files": ["src/db.rs"]},
        "verification_after": {"cmd": "cargo test pool", "outcome": "pass"}
    }))
    .unwrap();
    assert_eq!(rca.repro_before.outcome, TestOutcome::Fail);

    let rep: FinalReport = serde_json::from_value(json!({"summary_md": "done"})).unwrap();
    assert!(rep.changes.is_empty());
}

#[test]
fn handoff_args_map_to_draft() {
    let args = json!({
        "to": {"stage": {"pipeline_id": "P-1", "stage": "test_review"}},
        "intent": "review", "mode": "read_only",
        "summary": "impl done", "body_md": "details",
        "self_test": {"cmd": "cargo test", "outcome": "pass"}
    });
    let mut draft_json = args.clone();
    draft_json["from_session"] = json!("s1");
    let d: HandoffDraft = serde_json::from_value(draft_json).unwrap();
    assert_eq!(d.intent, HandoffIntent::Review);
    assert_eq!(d.mode, AccessMode::ReadOnly);
    assert_eq!(
        d.to.stage.as_ref().map(|s| s.stage),
        Some(StageKind::TestReview)
    );
}

#[test]
fn start_pipeline_bindings_are_shorthands() {
    let schema = tool("start_pipeline");
    assert_eq!(
        schema["inputSchema"]["properties"]["bindings"]["additionalProperties"]["type"],
        "string"
    );
    for s in ["claude:opus@high", "codex", "agy /swarm"] {
        StageBinding::from_shorthand(s).unwrap();
    }
}

#[test]
fn spawn_session_schema_and_command_roundtrip() {
    let schema = tool("spawn_session");
    let props = schema["inputSchema"]["properties"].as_object().unwrap();
    assert_eq!(props["agent"]["enum"], json!(["claude", "codex", "agy"]));
    assert_eq!(
        props["effort"]["enum"],
        json!(["low", "medium", "high", "xhigh", "max"])
    );
    for key in [
        "agent",
        "host_id",
        "model",
        "effort",
        "project_root",
        "tmux_session",
        "name",
        "prompt",
        "from_session",
    ] {
        assert!(props.contains_key(key), "spawn_session missing prop {key}");
    }

    let cmd: DaemonCommand = serde_json::from_value(json!({
        "cmd": "spawn_session",
        "agent": "codex",
        "model": "gpt-6.2-astra",
        "effort": "xhigh",
        "project_root": "/repo",
        "tmux_session": "agent-session",
        "name": "codex-audit",
        "prompt": "starfane: read_inbox",
        "from_session": "s-caller"
    }))
    .unwrap();
    assert_eq!(cmd.name(), "spawn_session");
    match cmd {
        DaemonCommand::SpawnSession {
            agent,
            host_id,
            model,
            effort,
            project_root,
            tmux_session,
            name,
            prompt,
            from_session,
        } => {
            assert_eq!(agent, AgentKind::Codex);
            assert_eq!(host_id, None);
            assert_eq!(model.as_deref(), Some("gpt-6.2-astra"));
            assert_eq!(effort, Some(Effort::Xhigh));
            assert_eq!(project_root.as_deref(), Some("/repo"));
            assert_eq!(tmux_session.as_deref(), Some("agent-session"));
            assert_eq!(name.as_deref(), Some("codex-audit"));
            assert_eq!(prompt.as_deref(), Some("starfane: read_inbox"));
            assert_eq!(from_session, Some(SessionId::new("s-caller")));
        }
        other => panic!("unexpected {other:?}"),
    }

    let remote_cmd: DaemonCommand = serde_json::from_value(json!({
        "cmd": "spawn_session",
        "agent": "claude",
        "host_id": "remote-gpu"
    }))
    .unwrap();
    let ser_remote = serde_json::to_value(&remote_cmd).unwrap();
    assert_eq!(ser_remote["host_id"], "remote-gpu");
    match remote_cmd {
        DaemonCommand::SpawnSession { agent, host_id, .. } => {
            assert_eq!(agent, AgentKind::Claude);
            assert_eq!(host_id, Some(HostId::new("remote-gpu")));
        }
        other => panic!("unexpected {other:?}"),
    }
}
