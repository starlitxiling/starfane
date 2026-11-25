//! End-to-end core flows without tmux side effects (tmux bin = /usr/bin/false):
//! hook ingest + subagent aggregation, Feature pipeline binding → plan →
//! approval, standalone review gates and precision telemetry.

use serde_json::json;
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    root: PathBuf,
    repo: String,
    core: Core,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.root) {
            eprintln!("cleanup {}: {e}", self.root.display());
        }
    }
}

fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!("starfane-core-{}", id::new_id("T")));
    let repo = root.join("proj");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn parse_header(buf: &[u8]) -> u8 {\n    let first = buf[0];\n    first\n}\n\npub fn helper() -> usize { 42 }\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = std::fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut config = StarfaneConfig::default();
    config.paths.tmux = "/usr/bin/false".into();
    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    Fixture { root, repo, core }
}

fn emit(agent: AgentKind, pane: &str, payload: serde_json::Value) -> EmitRequest {
    EmitRequest {
        v: 1,
        agent,
        hook_event: None,
        payload,
        env: EnvCtx {
            tmux_pane: Some(pane.into()),
            ..EnvCtx::default()
        },
        recv_ts_ms: id::now_ms(),
    }
}

async fn call(core: &Core, cmd: DaemonCommand) -> serde_json::Value {
    let name = cmd.name();
    core.handle(cmd)
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

async fn seed_sessions(f: &Fixture) {
    let r = &f.repo;
    for req in [
        emit(
            AgentKind::Claude,
            "%1",
            json!({"hook_event_name":"SessionStart","session_id":"c1","cwd":r,"model":"opus","source":"startup"}),
        ),
        emit(
            AgentKind::Claude,
            "%1",
            json!({"hook_event_name":"Stop","session_id":"c1","cwd":r,"last_assistant_message":"ready","stop_hook_active":false}),
        ),
        emit(
            AgentKind::Codex,
            "%2",
            json!({"hook_event_name":"SessionStart","session_id":"x1","cwd":r,"model":"gpt-5","source":"startup","transcript_path":null}),
        ),
        emit(
            AgentKind::Codex,
            "%2",
            json!({"hook_event_name":"Stop","session_id":"x1","cwd":r,"model":"gpt-5","turn_id":"t1","last_assistant_message":"ok","stop_hook_active":false,"permission_mode":"default","transcript_path":null}),
        ),
        emit(
            AgentKind::Claude,
            "%1",
            json!({"hook_event_name":"SubagentStart","session_id":"c1","cwd":r,"agent_id":"a1","agent_type":"general-purpose"}),
        ),
    ] {
        f.core.ingest(req).await;
    }
}

#[tokio::test]
async fn ingest_aggregates_sessions_and_subagents() {
    let f = fixture();
    seed_sessions(&f).await;
    let roots: Vec<SessionRecord> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ListSessions {
                project: None,
                include_subagents: false,
                include_ended: false,
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(roots.len(), 2, "{roots:?}");
    let c1 = roots
        .iter()
        .find(|s| s.session_id.as_str() == "c1")
        .unwrap();
    assert_eq!(c1.state, SessionState::Idle);
    assert_eq!(c1.pane.as_ref().map(|p| p.as_str()), Some("%1"));
    assert_eq!(c1.git_root.as_deref(), Some(f.repo.as_str()));
    assert_eq!(c1.model.as_deref(), Some("opus"));
    assert_eq!((c1.subagent_count, c1.active_subagents), (1, 1));

    let all: Vec<SessionRecord> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ListSessions {
                project: None,
                include_subagents: true,
                include_ended: false,
            },
        )
        .await,
    )
    .unwrap();
    let sub = all
        .iter()
        .find(|s| s.session_id.as_str() == "c1/a1")
        .expect("sub record");
    assert!(sub.is_subagent);
    assert_eq!(
        sub.parent_session_id.as_ref().map(|s| s.as_str()),
        Some("c1")
    );

    // Identity resolution by pane and by cwd.
    let r = call(
        &f.core,
        DaemonCommand::ResolveIdentity {
            agent: AgentKind::Codex,
            cwd: None,
            pane: Some(PaneId::new("%2")),
            bind_token: None,
            session_hint: None,
            pid_chain: Vec::new(),
        },
    )
    .await;
    assert_eq!(r["status"], "resolved");
    assert_eq!(r["session_id"], "x1");
    let r = call(
        &f.core,
        DaemonCommand::ResolveIdentity {
            agent: AgentKind::Claude,
            cwd: Some(f.repo.clone()),
            pane: None,
            bind_token: None,
            session_hint: None,
            pid_chain: Vec::new(),
        },
    )
    .await;
    assert_eq!(r["session_id"], "c1");
}

#[tokio::test]
async fn feature_pipeline_binding_plan_and_approval() {
    let f = fixture();
    seed_sessions(&f).await;
    let parsed = call(
        &f.core,
        DaemonCommand::ParseUtterance {
            text: "加个飞书静音命令，计划用 claude，实现用 codex，review 用 agy".into(),
            playbook: None,
            project: Some(f.repo.clone()),
        },
    )
    .await;
    assert_eq!(parsed["playbook"], "feature");
    assert_eq!(parsed["resolved"]["plan"]["agent"], "claude");
    let bindings = serde_json::from_value(parsed["bindings"].clone()).unwrap();

    let spec: PipelineSpec = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::CreatePipeline {
                req: CreatePipelineReq {
                    playbook: PlaybookKind::Feature,
                    project_root: f.repo.clone(),
                    goal: "加个飞书静音命令".into(),
                    bindings,
                    options: None,
                    source: PipelineSource {
                        via: Via::Cli,
                        utterance: None,
                        operator: None,
                    },
                },
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(spec.status, PipelineStatus::AwaitingBindingConfirm);
    assert!(f
        .core
        .snapshot()
        .attention
        .iter()
        .any(|a| a.kind == attention::BINDING_CONFIRM));

    let spec: PipelineSpec = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ConfirmPipeline {
                pipeline_id: spec.id.clone(),
                overrides: None,
            },
        )
        .await,
    )
    .unwrap();
    let plan = spec.stage(StageKind::Plan).unwrap();
    assert_eq!(plan.status, StageStatus::Running);
    assert_eq!(
        plan.session_id.as_ref().map(|s| s.as_str()),
        Some("c1"),
        "idle claude adopted"
    );

    let inbox: Vec<InboxItem> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ReadInbox {
                session_id: SessionId::new("c1"),
                mark_delivered: true,
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::Task);
    assert!(inbox[0].body_md.contains("report_milestone"));

    // An incomplete plan is rejected by G1, a complete one is accepted.
    let bad: PlanDoc = serde_json::from_value(
        json!({"goal":"g","design_md":"d","tasks":[],"acceptance":[],"test_plan":[]}),
    )
    .unwrap();
    let out = call(
        &f.core,
        DaemonCommand::SubmitArtifact {
            session_id: SessionId::new("c1"),
            artifact: ArtifactPayload::Plan(bad),
        },
    )
    .await;
    assert_eq!(out["accepted"], false);
    let good: PlanDoc = serde_json::from_value(json!({
        "goal":"mute","design_md":"add mute command",
        "tasks":[{"id":"t1","title":"parser"}],
        "acceptance":["mute 30m silences feishu"],"test_plan":["unit test parser"]
    }))
    .unwrap();
    let out = call(
        &f.core,
        DaemonCommand::SubmitArtifact {
            session_id: SessionId::new("c1"),
            artifact: ArtifactPayload::Plan(good),
        },
    )
    .await;
    assert_eq!(out["accepted"], true, "{out}");
    let p: PipelineSpec = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::GetPipeline {
                pipeline_id: spec.id.clone(),
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        p.stage(StageKind::Plan).unwrap().status,
        StageStatus::AwaitingHuman
    );
    assert!(f
        .core
        .snapshot()
        .attention
        .iter()
        .any(|a| a.kind == attention::PLAN_APPROVAL));

    // Approval moves to Implement: a worktree is created; spawning fails (no tmux) → Blocked.
    let r = f
        .core
        .handle(DaemonCommand::PipelineAction {
            pipeline_id: spec.id.clone(),
            action: PipelineAction::ApprovePlan,
        })
        .await;
    assert!(r.is_err(), "spawn must fail without tmux");
    let p: PipelineSpec = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::GetPipeline {
                pipeline_id: spec.id.clone(),
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(p.stage(StageKind::Plan).unwrap().status, StageStatus::Done);
    assert_eq!(
        p.stage(StageKind::Implement).unwrap().status,
        StageStatus::Blocked
    );
    assert_eq!(p.status, PipelineStatus::Blocked);
    let wt = Path::new(&f.repo)
        .join(".starfane/worktrees")
        .join(spec.id.as_str());
    assert!(
        wt.join(".git").exists(),
        "worktree created at {}",
        wt.display()
    );
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        format!("starfane/{}", spec.id)
    );
    // The pipeline is persisted as TOML in the project.
    assert!(Path::new(&f.repo)
        .join(format!(".starfane/pipelines/{}.toml", spec.id))
        .exists());

    // Cancel releases the stage sessions.
    call(
        &f.core,
        DaemonCommand::PipelineAction {
            pipeline_id: spec.id.clone(),
            action: PipelineAction::Cancel,
        },
    )
    .await;
    let sessions: Vec<SessionRecord> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ListSessions {
                project: None,
                include_subagents: false,
                include_ended: false,
            },
        )
        .await,
    )
    .unwrap();
    assert!(sessions.iter().all(|s| s.stage.is_none()));
}

#[tokio::test]
async fn standalone_review_gates_and_precision() {
    let f = fixture();
    seed_sessions(&f).await;
    let head = git(Path::new(&f.repo), &["rev-parse", "HEAD"]);
    let report: ReviewReport = serde_json::from_value(json!({
        "depth": "D1", "base": head, "head": head,
        "findings": [
            {
                "category": "correctness", "severity": "major", "confidence": 0.8,
                "claim": "parse_header panics on empty input",
                "location": {"file": "src/lib.rs", "line_start": 2, "line_end": 2, "symbol": "parse_header"},
                "evidence": [{"file": "src/lib.rs", "line_start": 2, "line_end": 2, "quote": "let first = buf[0];"}],
                "trigger_path": [{"file": "src/lib.rs", "line": 1}, {"file": "src/lib.rs", "line": 2}],
                "checked_guards": [{"file": "src/lib.rs", "line": 1, "note": "no length check"}],
                "observable_impact": "panic", "preconditions": "empty buffer",
                "verification_method": "code_trace"
            },
            {
                "category": "security", "severity": "major", "confidence": 0.9,
                "claim": "invented issue",
                "location": {"file": "src/lib.rs", "line_start": 5, "line_end": 5},
                "evidence": [{"file": "src/lib.rs", "line_start": 5, "line_end": 5, "quote": "unsafe { transmute(ptr) }"}],
                "trigger_path": [{"file": "src/lib.rs", "line": 5}],
                "checked_guards": [{"file": "src/lib.rs", "line": 1}],
                "observable_impact": "memory corruption",
                "verification_method": "code_trace"
            },
            {
                "category": "style", "severity": "nit", "confidence": 0.5,
                "claim": "helper could be const",
                "location": {"file": "src/lib.rs", "line_start": 6, "line_end": 6},
                "evidence": [{"file": "src/lib.rs", "line_start": 6, "line_end": 6, "quote": "pub fn helper() -> usize { 42 }"}],
                "trigger_path": [{"file": "src/lib.rs", "line": 6}],
                "checked_guards": [{"file": "src/lib.rs", "line": 6}],
                "observable_impact": "none",
                "verification_method": "static_check"
            }
        ]
    }))
    .unwrap();
    let out = call(
        &f.core,
        DaemonCommand::SubmitArtifact {
            session_id: SessionId::new("x1"),
            artifact: ArtifactPayload::Review(report),
        },
    )
    .await;
    assert_eq!(out["accepted"], true, "{out}");
    let rid = ReviewId(out["review_id"].as_str().unwrap().to_string());
    let rec: ReviewRecord = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::GetReview {
                review_id: rid.clone(),
            },
        )
        .await,
    )
    .unwrap();
    let disp: Vec<Disposition> = rec.findings.iter().map(|f| f.disposition).collect();
    assert_eq!(
        disp,
        vec![
            Disposition::Delivered,
            Disposition::Fabricated,
            Disposition::Folded
        ],
        "{rec:#?}"
    );
    assert!(rec.complete);

    let fid = rec.findings[0].finding.id.clone();
    call(
        &f.core,
        DaemonCommand::ReviewFeedback {
            review_id: rid.clone(),
            finding_id: fid,
            label: FeedbackLabel::Valid,
            note: None,
            by: "t".into(),
        },
    )
    .await;
    let stats: Vec<PrecisionStat> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::GetPrecision {
                agent: Some(AgentKind::Codex),
                model: None,
            },
        )
        .await,
    )
    .unwrap();
    let correctness = stats
        .iter()
        .find(|s| s.category == Category::Correctness)
        .unwrap();
    assert_eq!((correctness.delivered, correctness.valid), (1, 1));
    assert_eq!(correctness.precision, Some(1.0));
    let security = stats
        .iter()
        .find(|s| s.category == Category::Security)
        .unwrap();
    assert_eq!(security.fabricated, 1);
    assert!(Path::new(&f.repo).join(".starfane/feedback.jsonl").exists());
}

#[tokio::test]
async fn dispatch_queues_in_inbox_when_gate_unavailable() {
    let f = fixture();
    seed_sessions(&f).await;
    let r = call(
        &f.core,
        DaemonCommand::Dispatch {
            target: DispatchTarget::Pane {
                id: PaneId::new("%1"),
            },
            text: "请继续".into(),
            attachments: Vec::new(),
            allow: vec![DeliveryLevel::L2Inject],
            interactive: false,
        },
    )
    .await;
    assert_eq!(r["status"], "queued");
    let inbox: Vec<InboxItem> = serde_json::from_value(
        call(
            &f.core,
            DaemonCommand::ReadInbox {
                session_id: SessionId::new("c1"),
                mark_delivered: true,
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].body_md, "请继续");
    let err = f
        .core
        .handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Pane {
                id: PaneId::new("%99"),
            },
            text: "x".into(),
            attachments: Vec::new(),
            allow: vec![],
            interactive: false,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
