//! JSON / TOML round-trip tests for the frozen protocol types.

use serde_json::json;
use starfane_proto::*;
use std::collections::BTreeMap;

fn sample_pipeline() -> PipelineSpec {
    let plan = StageBinding {
        session: SessionTarget::Pane(PaneId::new("%3")),
        ..StageBinding::from_shorthand("claude:opus@high").unwrap()
    };
    let implement = StageBinding {
        session: SessionTarget::Spawn,
        ..StageBinding::from_shorthand("codex").unwrap()
    };
    let review = StageBinding {
        session: SessionTarget::Spawn,
        depth: Some(ReviewDepth::D2),
        ..StageBinding::from_shorthand("agy /swarm").unwrap()
    };
    let report = StageBinding::from_shorthand("same_as:plan").unwrap();
    let mut stages = vec![
        StageSpec::template(StageKind::Plan, plan, 2),
        StageSpec::template(StageKind::Implement, implement, 2),
        StageSpec::template(StageKind::TestReview, review, 2),
        StageSpec::template(StageKind::Report, report, 2),
    ];
    stages[0].status = StageStatus::Done;
    stages[0].artifacts.push(ArtifactId::new("A-1"));
    stages[0].started_ms = Some(10);
    stages[0].ended_ms = Some(20);
    stages[1].status = StageStatus::Running;
    stages[1].session_id = Some(SessionId::new("sess-codex"));
    stages[1].pane = Some(PaneId::new("%9"));
    PipelineSpec {
        v: PROTO_VERSION,
        id: PipelineId::new("P-01JD"),
        playbook: PlaybookKind::Feature,
        goal: "给 starfane 加飞书静音命令".into(),
        status: PipelineStatus::Running,
        created_ms: 1,
        updated_ms: 2,
        project: ProjectRef {
            root: "/Users/x/proj".into(),
            base_branch: Some("main".into()),
            worktree: WorktreeMode::Auto,
        },
        source: PipelineSource {
            via: Via::Feishu,
            utterance: Some("计划用 Claude opus，实现用 Codex，Review 用 agy /swarm".into()),
            operator: Some("ou_xxx".into()),
        },
        options: PipelineOptions::default(),
        feishu_thread: Some(FeishuThreadRef {
            chat_id: "oc_1".into(),
            root_message_id: "om_1".into(),
        }),
        stages,
    }
}

#[test]
fn pipeline_json_roundtrip() {
    let p = sample_pipeline();
    let s = serde_json::to_string(&p).unwrap();
    let back: PipelineSpec = serde_json::from_str(&s).unwrap();
    assert_eq!(back, p);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["stages"][2]["loop"]["back_to"], "implement");
    assert_eq!(v["stages"][2]["binding"]["native"], "swarm");
    assert_eq!(v["stages"][0]["binding"]["session"], "%3");
    assert_eq!(v["options"]["review_depth"], "D1");
}

#[test]
fn pipeline_toml_roundtrip() {
    let p = sample_pipeline();
    let text = toml::to_string(&p).expect("serialize toml");
    assert!(text.contains("[[stages]]"), "{text}");
    let back: PipelineSpec = toml::from_str(&text).expect("parse toml");
    assert_eq!(back, p);
}

#[test]
fn pipeline_toml_handwritten() {
    let text = r#"
v = 1
id = "P-01JD"
playbook = "debug"
goal = "登录偶发 500"
status = "awaiting_binding_confirm"
created_ms = 1
updated_ms = 1
project = { root = "/repo", worktree = "none" }
source = { via = "cli" }

[[stages]]
id = "triage"
binding = { agent = "codex" }
gate = "evidence_g2"
artifact = "triage"
status = "pending"

[[stages]]
id = "deep"
binding = { agent = "agy", native = "boost", session = "spawn" }
artifact = "rca"
status = "pending"
"#;
    let p: PipelineSpec = toml::from_str(text).expect("parse");
    assert_eq!(p.playbook, PlaybookKind::Debug);
    assert_eq!(p.options, PipelineOptions::default());
    assert_eq!(p.project.worktree, WorktreeMode::None);
    assert_eq!(p.stages[1].binding.session, SessionTarget::Spawn);
    assert_eq!(p.stages[1].binding.native, Some(NativeMode::Boost));
    assert_eq!(p.current_stage().map(|s| s.id), Some(StageKind::Triage));
}

#[test]
fn daemon_commands_roundtrip() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        StageKind::Implement,
        StageBinding::from_shorthand("claude:sonnet").unwrap(),
    );
    let cmds = vec![
        DaemonCommand::Ping,
        DaemonCommand::ResolveIdentity {
            agent: AgentKind::Codex,
            cwd: Some("/repo".into()),
            pane: None,
            bind_token: Some(BindToken::new("B-1")),
            session_hint: None,
            pid_chain: vec![4200, 4100],
        },
        DaemonCommand::ConfirmPipeline {
            pipeline_id: PipelineId::new("P-1"),
            overrides: Some(overrides),
        },
        DaemonCommand::PipelineAction {
            pipeline_id: PipelineId::new("P-1"),
            action: PipelineAction::RetryStage {
                stage: StageKind::TestReview,
            },
        },
        DaemonCommand::Dispatch {
            target: DispatchTarget::Pane {
                id: PaneId::new("%4"),
            },
            text: "starfane: 请调用 read_inbox".into(),
            attachments: Vec::new(),
            allow: vec![DeliveryLevel::L0Inbox, DeliveryLevel::L2Inject],
            interactive: false,
        },
        DaemonCommand::HumanVerdict {
            review_id: ReviewId::new("R-1"),
            finding_id: FindingId::new("F-1"),
            verdict: FalsifierVerdict::Downgraded { to: Severity::Nit },
            note: None,
            by: "ou_x".into(),
        },
        DaemonCommand::SubmitArtifact {
            session_id: SessionId::new("s"),
            artifact: ArtifactPayload::Report(FinalReport {
                header: ArtifactHeader::new(ArtifactKind::Report),
                summary_md: "ok".into(),
                changes: vec![],
                decisions: vec![],
                follow_ups: vec![],
                doc_path: None,
            }),
        },
        DaemonCommand::CreatePipeline {
            req: CreatePipelineReq {
                playbook: PlaybookKind::Feature,
                project_root: "/repo".into(),
                goal: "g".into(),
                bindings: BTreeMap::new(),
                options: None,
                source: PipelineSource {
                    via: Via::Mcp,
                    utterance: None,
                    operator: None,
                },
            },
        },
        DaemonCommand::Shutdown,
    ];
    for c in cmds {
        let frame = ClientFrame::Req { id: 3, cmd: c };
        let line = encode_line(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(line.trim_end()).unwrap();
        // DaemonCommand has no PartialEq (contains f32 deep inside); compare JSON.
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&frame).unwrap()
        );
    }
}

#[test]
fn state_snapshot_and_sse_roundtrip() {
    let session = SessionRecord {
        session_id: SessionId::new("root/a1"),
        agent: AgentKind::Claude,
        pane: Some(PaneId::new("%2")),
        pane_resolution: PaneResolution::Env,
        cwd: Some("/repo".into()),
        git_root: Some("/repo".into()),
        model: Some("opus".into()),
        effort: Some("high".into()),
        state: SessionState::Working,
        state_since_ms: 1,
        last_event_id: Some(EventId::new("E-1")),
        last_event_ms: 1,
        last_message: None,
        last_error: None,
        parent_session_id: Some(SessionId::new("root")),
        is_subagent: true,
        subagent_count: 0,
        active_subagents: 0,
        stage: Some(StageRef {
            pipeline_id: PipelineId::new("P-1"),
            stage: StageKind::Implement,
        }),
        unread_inbox: 0,
        confidence: Confidence::Exact,
        tmux_target: Some("prome:1.0".into()),
        tmux_label: Some("prome-train".into()),
        discovered_only: false,
        transcript_path: Some("/tmp/t.jsonl".into()),
        metrics: Some(SessionMetrics {
            context_used_tokens: Some(120_000),
            context_window_tokens: Some(200_000),
            context_used_pct: Some(60.0),
            tps: Some(64.5),
            ttft_ms: Some(1_200),
            avg_ttft_ms: Some(1_200),
            last_output_tokens: Some(645),
            last_duration_ms: Some(10_000),
            total_input_tokens: Some(300_000),
            total_output_tokens: Some(4_200),
            recent_turns: vec![TurnMetricSample {
                ts_ms: 1234,
                ttft_ms: Some(1_200),
                tps: Some(64.5),
                output_tokens: Some(645),
                duration_ms: Some(10_000),
            }],
            source: "transcript".into(),
            updated_ms: 1234,
        }),
        host_id: Some(HostId::new("local")),
        host_display_name: Some("Local".into()),
        mux_kind: Some("tmux".into()),
    };
    let snap = StateSnapshot {
        v: 1,
        version: VERSION.into(),
        started_ms: 0,
        sessions: vec![session.clone()],
        pipelines: vec![sample_pipeline()],
        attention: vec![AttentionItem {
            kind: attention::PLAN_APPROVAL.into(),
            title: "approve plan".into(),
            pipeline_id: Some(PipelineId::new("P-1")),
            session_id: None,
            review_id: None,
            since_ms: 5,
        }],
        event_seq: 9,
    };
    let back: StateSnapshot = serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
    assert_eq!(back, snap);

    let ev = DaemonEvent::Session { seq: 10, session };
    let frame = ev.to_sse_frame().unwrap();
    assert!(frame.starts_with("id: 10\nevent: session\ndata: {"));
    assert!(frame.ends_with("\n\n"));

    let out_ev = DaemonEvent::Output {
        seq: 11,
        session_id: SessionId::new("mac-xiling:tmux-%39"),
        start_seq: Some(25),
        total_messages: Some(26),
        appended_messages: vec![json!({"role": "assistant", "text": "hello"})],
        live_tail: Some("tail".into()),
        metrics: None,
        dirty: false,
    };
    let out_frame = out_ev.to_sse_frame().unwrap();
    assert!(out_frame.starts_with("id: 11\nevent: output\ndata: {"));
    let parsed_out: DaemonEvent = serde_json::from_str(
        out_frame
            .lines()
            .nth(2)
            .unwrap()
            .trim_start_matches("data: "),
    )
    .unwrap();
    assert_eq!(parsed_out, out_ev);
}

#[test]
fn review_record_roundtrip() {
    let finding: ReviewFinding = serde_json::from_value(json!({
        "id": "F-1", "category": "security", "severity": "blocker", "confidence": 0.9,
        "claim": "path traversal", "location": {"file": "src/f.rs", "line_start": 3, "line_end": 4},
        "evidence": [{"file": "src/f.rs", "line_start": 3, "line_end": 3, "quote": "File::open(user_path)"}],
        "verification_method": "repro_test",
        "repro": {"kind": "input", "content": "../../etc/passwd"}
    }))
    .unwrap();
    let rec = ReviewRecord {
        v: 1,
        id: ReviewId::new("R-1"),
        report: ReviewReport {
            header: ArtifactHeader::new(ArtifactKind::Review),
            depth: ReviewDepth::D3,
            base: "a".into(),
            head: "b".into(),
            scope_files: vec![],
            checked_areas: vec![],
            findings: vec![finding.clone()],
            summary: None,
        },
        report_g1: vec![],
        findings: vec![AdjudicatedFinding {
            finding,
            g1: vec![],
            g2: Some(G2Outcome::from_issues(vec![])),
            falsifier: None,
            disposition: Disposition::PendingFalsifier,
            feedback: None,
        }],
        returned_once: false,
        complete: false,
    };
    let back: ReviewRecord = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
    assert_eq!(back, rec);
}

#[test]
fn emit_request_with_raw_payload() {
    let e = EmitRequest {
        v: 1,
        agent: AgentKind::Claude,
        hook_event: Some("Stop".into()),
        payload: json!({"_raw": "not json"}),
        env: EnvCtx {
            tmux_pane: Some("%1".into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: 1,
    };
    let back: EmitRequest = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(back, e);
}

#[test]
fn id_generation_prefixes() {
    let p = id::new_id(id::prefix::PIPELINE);
    let parsed = PipelineId::new(p.clone());
    assert!(parsed.as_str().starts_with("P-"));
    assert!(id::timestamp_ms(&p).is_some());
}

#[test]
fn session_metrics_legacy_json_and_recent_turns_roundtrip() {
    // Legacy JSON missing `avg_ttft_ms` and `recent_turns` must deserialize cleanly,
    // and serialize without emitting empty `avg_ttft_ms` / `recent_turns` fields.
    let legacy_json = json!({
        "context_used_tokens": 50_000,
        "context_window_tokens": 200_000,
        "context_used_pct": 25.0,
        "tps": 72.3,
        "ttft_ms": 950,
        "source": "transcript",
        "updated_ms": 1_700_000_000_000u64
    });
    let legacy: SessionMetrics = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(legacy.avg_ttft_ms, None);
    assert!(legacy.recent_turns.is_empty());
    let ser_legacy = serde_json::to_value(&legacy).unwrap();
    assert!(ser_legacy.get("avg_ttft_ms").is_none());
    assert!(ser_legacy.get("recent_turns").is_none());

    // Populated `avg_ttft_ms` and `recent_turns` roundtrip losslessly.
    let populated = SessionMetrics {
        avg_ttft_ms: Some(1_150),
        recent_turns: vec![
            TurnMetricSample {
                ts_ms: 1_700_000_001_000,
                ttft_ms: Some(950),
                tps: Some(80.5),
                output_tokens: Some(322),
                duration_ms: Some(4_000),
            },
            TurnMetricSample {
                ts_ms: 1_700_000_005_000,
                ttft_ms: Some(1_350),
                tps: None,
                output_tokens: Some(45),
                duration_ms: None,
            },
        ],
        source: "agy_db".into(),
        ..legacy
    };
    let ser = serde_json::to_value(&populated).unwrap();
    assert_eq!(ser["avg_ttft_ms"], 1_150);
    assert_eq!(ser["recent_turns"].as_array().unwrap().len(), 2);
    assert!(ser["recent_turns"][1].get("tps").is_none());
    assert!(ser["recent_turns"][1].get("duration_ms").is_none());
    let back: SessionMetrics = serde_json::from_value(ser).unwrap();
    assert_eq!(back, populated);
}
