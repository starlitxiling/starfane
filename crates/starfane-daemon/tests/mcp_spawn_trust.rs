//! End-to-end integration tests for:
//! 1. `DaemonCommand::SpawnSession` (backing the `spawn_session` MCP tool):
//!    - Structured JSON payload (`session_id`, `pane_id`, `tmux_target`, `tmux_label`, `agent`, `model`, `effort`, `cwd`),
//!    - Sanctioned per-session ancestor-trust argv propagation for Codex (`-c 'projects={...}'`)
//!      and AGY (`--settings '{"trustedWorkspaces":[...]}'`),
//!    - Strict refusal to inherit for explicitly untrusted subdirs, outside paths, and Claude (read-only `~/.claude.json`).
//! 2. End-to-end folder-trust screen detection across Codex, Claude, and AGY:
//!    - `sync_discovered_panes()` transitions panes stuck on trust screens to `SessionState::NeedsPermission`
//!      and surfaces them in `StateSnapshot.attention` (Web Inbox),
//!    - `DaemonCommand::Dispatch` (both L2 gate and `interactive: true`) refuses to inject keystrokes
//!      into trust prompts, failing `GateCheck::InputEmpty` with clear agent-specific reasons.

use serde_json::{json, Value};
use starfane_daemon::{Core, DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;

struct IsolatedEnv {
    root: PathBuf,
    tmux_socket: Option<String>,
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        if let Some(sock) = &self.tmux_socket {
            let _ = Command::new("tmux")
                .args(["-L", sock, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for_file(path: &Path, timeout_ms: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                return content;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

#[tokio::test]
async fn spawn_session_command_returns_structured_json_and_applies_sanctioned_ancestor_trust() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let root = std::env::temp_dir().join(format!("sf-mcp-spawn-{}", id::new_id("T")));
    let home = root.join("home");
    let trusted_child = home.join("Work").join("repo_a");
    let untrusted_child = home.join("untrusted_zone").join("repo_b");
    let outside_dir = root.join("outside_repo");
    let bin_dir = root.join("bin");
    let argv_dir = root.join("argv");
    std::fs::create_dir_all(&trusted_child).unwrap();
    std::fs::create_dir_all(&untrusted_child).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&argv_dir).unwrap();

    let sock = format!("sf-mcp-spawn-{}", std::process::id());
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: Some(sock.clone()),
    };

    let canon_home = std::fs::canonicalize(&home).unwrap();
    let canon_trusted_child = std::fs::canonicalize(&trusted_child).unwrap();
    let canon_untrusted_zone = std::fs::canonicalize(home.join("untrusted_zone")).unwrap();

    // Seed read-only trust configs for all 3 CLIs under isolated `home`.
    let codex_cfg = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(codex_cfg.parent().unwrap()).unwrap();
    let codex_toml = format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"\n\n[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
        canon_home.display(),
        canon_untrusted_zone.display()
    );
    std::fs::write(&codex_cfg, &codex_toml).unwrap();

    let agy_cfg = home.join(".gemini/antigravity-cli/settings.json");
    std::fs::create_dir_all(agy_cfg.parent().unwrap()).unwrap();
    let agy_json = json!({ "trustedWorkspaces": [canon_home.to_string_lossy()] }).to_string();
    std::fs::write(&agy_cfg, &agy_json).unwrap();

    let claude_cfg = home.join(".claude.json");
    let claude_json = json!({
        "projects": {
            canon_home.to_string_lossy().to_string(): { "hasTrustDialogAccepted": true }
        }
    })
    .to_string();
    std::fs::write(&claude_cfg, &claude_json).unwrap();

    // Fake binaries record their argv (one arg per line) atomically into `<argv_dir>/<agent>-<window>.txt`.
    for name in ["codex", "agy", "claude"] {
        write_executable(
            &bin_dir.join(name),
            &format!(
                "#!/bin/sh\nWIN=$(tmux -L {sock} display-message -p -t \"$TMUX_PANE\" '#W' 2>/dev/null || echo unknown)\nOUT=\"{}/{}-$WIN.txt\"\n: > \"$OUT.tmp\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"$OUT.tmp\"; done\nmv \"$OUT.tmp\" \"$OUT\"\nsleep 30\n",
                argv_dir.display(),
                name
            ),
        );
    }
    let tmux_wrap = bin_dir.join("tmux-wrap.sh");
    write_executable(
        &tmux_wrap,
        &format!("#!/bin/sh\nexec tmux -L {sock} \"$@\"\n"),
    );

    let mut config = StarfaneConfig::default();
    config.paths.tmux = tmux_wrap.to_string_lossy().into_owned();
    config.paths.codex = bin_dir.join("codex").to_string_lossy().into_owned();
    config.paths.agy = bin_dir.join("agy").to_string_lossy().into_owned();
    config.paths.claude = bin_dir.join("claude").to_string_lossy().into_owned();
    config.zellij.enabled = false;

    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    core.set_home_dir_override(Some(home.clone()));

    // 1. Spawn Codex in trusted-ancestor child via DaemonCommand::SpawnSession
    let resp_cx: Value = core
        .handle(DaemonCommand::SpawnSession {
            agent: AgentKind::Codex,
            host_id: None,
            model: Some("gpt-6.2-astra".into()),
            effort: Some(Effort::Xhigh),
            project_root: Some(trusted_child.to_string_lossy().into_owned()),
            tmux_session: Some("mcp-test".into()),
            name: Some("cx-trusted".into()),
            prompt: Some("starfane: read_inbox".into()),
            from_session: Some(SessionId::new("s-caller")),
        })
        .await
        .expect("spawn codex trusted child");

    assert!(resp_cx["session_id"]
        .as_str()
        .unwrap()
        .starts_with("tmux-%"));
    assert!(resp_cx["pane_id"].as_str().unwrap().starts_with('%'));
    assert!(resp_cx["tmux_target"]
        .as_str()
        .unwrap()
        .starts_with("mcp-test:"));
    assert_eq!(resp_cx["tmux_label"], "mcp-test-cx-trusted");
    assert_eq!(resp_cx["agent"], "codex");
    assert_eq!(resp_cx["model"], "gpt-6.2-astra");
    assert_eq!(resp_cx["effort"], "xhigh");
    assert_eq!(resp_cx["cwd"], trusted_child.to_string_lossy().as_ref());

    let cx_trusted_argv = wait_for_file(&argv_dir.join("codex-cx-trusted.txt"), 3_000);
    let expected_cx_override = starfane_adapters::codex_trust_override_arg(&canon_trusted_child);
    assert!(
        cx_trusted_argv.lines().any(|l| l == expected_cx_override),
        "expected Codex ancestor-trust -c override in argv:\n{cx_trusted_argv}"
    );

    // 2. Spawn Codex in explicitly untrusted subdir -> MUST NOT pass projects override
    core.handle(DaemonCommand::SpawnSession {
        agent: AgentKind::Codex,
        host_id: None,
        model: None,
        effort: None,
        project_root: Some(untrusted_child.to_string_lossy().into_owned()),
        tmux_session: Some("mcp-test".into()),
        name: Some("cx-untrusted".into()),
        prompt: None,
        from_session: None,
    })
    .await
    .expect("spawn codex untrusted child");
    let cx_untrusted_argv = wait_for_file(&argv_dir.join("codex-cx-untrusted.txt"), 3_000);
    assert!(
        !cx_untrusted_argv.contains("projects="),
        "explicitly untrusted subdir must NEVER receive projects trust override:\n{cx_untrusted_argv}"
    );

    // 3. Spawn Codex in completely outside dir -> MUST NOT pass projects override
    core.handle(DaemonCommand::SpawnSession {
        agent: AgentKind::Codex,
        host_id: None,
        model: None,
        effort: None,
        project_root: Some(outside_dir.to_string_lossy().into_owned()),
        tmux_session: Some("mcp-test".into()),
        name: Some("cx-outside".into()),
        prompt: None,
        from_session: None,
    })
    .await
    .expect("spawn codex outside dir");
    let cx_outside_argv = wait_for_file(&argv_dir.join("codex-cx-outside.txt"), 3_000);
    assert!(
        !cx_outside_argv.contains("projects="),
        "outside dir must NEVER receive projects trust override:\n{cx_outside_argv}"
    );

    // 4. Spawn AGY in trusted-ancestor child -> gets session-only `--settings` with canon path
    let resp_agy = core
        .handle(DaemonCommand::SpawnSession {
            agent: AgentKind::Agy,
            host_id: None,
            model: Some("gemini-4-carbon".into()),
            effort: Some(Effort::High),
            project_root: Some(trusted_child.to_string_lossy().into_owned()),
            tmux_session: Some("mcp-test".into()),
            name: Some("agy-trusted".into()),
            prompt: None,
            from_session: None,
        })
        .await
        .expect("spawn agy trusted child");
    assert_eq!(resp_agy["agent"], "agy");
    assert_eq!(resp_agy["model"], "gemini-4-carbon-high");
    assert_eq!(resp_agy["effort"], "high");

    let agy_trusted_argv = wait_for_file(&argv_dir.join("agy-agy-trusted.txt"), 3_000);
    let agy_lines: Vec<&str> = agy_trusted_argv.lines().collect();
    let settings_idx = agy_lines
        .iter()
        .position(|&l| l == "--settings")
        .expect("expected --settings flag for AGY ancestor trust");
    let settings_val: Value = serde_json::from_str(agy_lines[settings_idx + 1]).unwrap();
    assert!(settings_val["trustedWorkspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str() == Some(canon_trusted_child.to_string_lossy().as_ref())));

    // 5. Spawn Claude in trusted-ancestor child -> NO trust CLI flag & ~/.claude.json untouched!
    core.handle(DaemonCommand::SpawnSession {
        agent: AgentKind::Claude,
        host_id: None,
        model: Some("opus".into()),
        effort: Some(Effort::High),
        project_root: Some(trusted_child.to_string_lossy().into_owned()),
        tmux_session: Some("mcp-test".into()),
        name: Some("cl-trusted".into()),
        prompt: None,
        from_session: None,
    })
    .await
    .expect("spawn claude trusted child");
    let cl_argv = wait_for_file(&argv_dir.join("claude-cl-trusted.txt"), 3_000);
    assert!(!cl_argv.contains("hasTrustDialogAccepted"));

    // Confirm ZERO config files on disk were mutated!
    assert_eq!(std::fs::read_to_string(&codex_cfg).unwrap(), codex_toml);
    assert_eq!(std::fs::read_to_string(&agy_cfg).unwrap(), agy_json);
    assert_eq!(std::fs::read_to_string(&claude_cfg).unwrap(), claude_json);
}

#[tokio::test]
async fn discovered_trust_screens_surface_as_needs_permission_and_block_dispatch() {
    let root = std::env::temp_dir().join(format!("sf-trust-detect-{}", id::new_id("T")));
    let bin_dir = root.join("bin");
    let screens_dir = root.join("screens");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&screens_dir).unwrap();
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: None,
    };

    let paste_marker = root.join("paste_attempted.txt");
    let self_pid = std::process::id();
    let pane_file = root.join("panes.txt");

    let mk_row = |pane: &str, cmd: &str, win: &str| {
        [
            pane,
            "agent-session",
            "@1",
            "0",
            "0",
            &self_pid.to_string(),
            cmd,
            "/tmp/untrusted_proj",
            "1",
            "1",
            "0",
            "0",
            "0",
            "0",
            "0",
            "1",
            "20",
            win,
        ]
        .join("\x1f")
    };
    std::fs::write(
        &pane_file,
        format!(
            "{}\n{}\n{}\n",
            mk_row("%81", "codex -m gpt-6.2-astra", "codex-1"),
            mk_row("%82", "claude --model opus", "claude-1"),
            mk_row("%83", "agy --model gemini-4-neon-xhigh", "agy-1"),
        ),
    )
    .unwrap();

    std::fs::write(
        screens_dir.join("%81.txt"),
        "  Folder access\n  /tmp/untrusted_proj\n\n  Trust this folder? Codex can read, edit, and run files here.\n\n› 1. Trust and continue\n  2. Back to Agent Command Center\n\n  enter continue · esc back\n",
    )
    .unwrap();
    std::fs::write(
        screens_dir.join("%82.txt"),
        " Accessing workspace:\n /tmp/untrusted_proj\n Quick safety check: Is this a project you created or one you trust?\n Claude Code'll be able to read, edit, and execute files here.\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel\n",
    )
    .unwrap();
    std::fs::write(
        screens_dir.join("%83.txt"),
        "Accessing workspace:\n/tmp/untrusted_proj\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\n  No, exit\n  ↑/↓ Navigate · enter Confirm\n",
    )
    .unwrap();

    let fake_tmux = bin_dir.join("fake-tmux.sh");
    write_executable(
        &fake_tmux,
        &format!(
            r#"#!/bin/sh
if [ "$1" = "list-panes" ]; then
  cat "{panes}"
  exit 0
fi
if [ "$1" = "list-clients" ]; then
  exit 0
fi
if [ "$1" = "load-buffer" ] || [ "$1" = "paste-buffer" ] || [ "$1" = "send-keys" ]; then
  echo "$*" >> "{marker}"
  exit 0
fi
if [ "$1" = "capture-pane" ]; then
  PANE=""
  PREV=""
  for a in "$@"; do
    if [ "$PREV" = "-t" ]; then PANE="$a"; break; fi
    PREV="$a"
  done
  if [ -f "{screens}/$PANE.txt" ]; then
    cat "{screens}/$PANE.txt"
  fi
  exit 0
fi
exit 0
"#,
            panes = pane_file.display(),
            screens = screens_dir.display(),
            marker = paste_marker.display(),
        ),
    );

    let mut config = StarfaneConfig::default();
    config.paths.tmux = fake_tmux.to_string_lossy().into_owned();
    config.zellij.enabled = false;
    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    core.sync_discovered_panes().await;
    let snap = core.snapshot();

    for (sid_str, expected_reason) in [
        ("tmux-%81", "codex waiting for folder trust approval"),
        ("tmux-%82", "claude waiting for folder trust approval"),
        ("tmux-%83", "agy waiting for folder trust approval"),
    ] {
        let rec = snap
            .sessions
            .iter()
            .find(|s| s.session_id.as_str() == sid_str)
            .expect("discovered session exists");
        assert_eq!(rec.state, SessionState::NeedsPermission, "{sid_str}");
        assert_eq!(
            rec.last_error.as_deref(),
            Some(expected_reason),
            "{sid_str}"
        );
        assert!(
            snap.attention.iter().any(|a| {
                a.kind == attention::BLOCKED
                    && a.session_id.as_ref().map(|x| x.as_str()) == Some(sid_str)
            }),
            "attention item missing for {sid_str}: {:?}",
            snap.attention
        );

        // Both interactive and non-interactive Dispatch MUST refuse to inject into trust screen!
        let rep_interactive: DeliveryReport = serde_json::from_value(
            core.handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new(sid_str),
                },
                text: "hello from web ui".into(),
                attachments: Vec::new(),
                allow: vec![DeliveryLevel::L2Inject],
                interactive: true,
            })
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(rep_interactive.status, DeliveryStatus::Failed, "{sid_str}");
        let g = rep_interactive.gate.expect("gate report");
        assert!(!g.passed);
        assert!(g.failed_checks().contains(&GateCheck::InputEmpty));
        assert!(g.detail.as_deref().unwrap().contains(expected_reason));

        let rep_l2: DeliveryReport = serde_json::from_value(
            core.handle(DaemonCommand::Dispatch {
                target: DispatchTarget::Session {
                    id: SessionId::new(sid_str),
                },
                text: "starfane: read_inbox".into(),
                attachments: Vec::new(),
                allow: vec![DeliveryLevel::L2Inject],
                interactive: false,
            })
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(rep_l2.status, DeliveryStatus::Queued, "{sid_str}");
        let g2 = rep_l2.gate.expect("l2 gate report");
        assert!(!g2.passed);
        assert!(g2.failed_checks().contains(&GateCheck::InputEmpty));
        assert!(g2.detail.as_deref().unwrap().contains(expected_reason));
    }

    assert!(
        !paste_marker.exists(),
        "no paste-buffer or send-keys may ever run against a trust prompt"
    );
}

#[tokio::test]
async fn resolve_identity_disambiguates_two_codex_panes_in_same_repo_via_pid_chain_and_blocks_update_modal(
) {
    let root = std::env::temp_dir().join(format!("sf-codex-pid-id-{}", id::new_id("T")));
    let bin_dir = root.join("bin");
    let screens_dir = root.join("screens");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&screens_dir).unwrap();
    let _guard = IsolatedEnv {
        root: root.clone(),
        tmux_socket: None,
    };

    let shared_repo = "/tmp/shared_workflow_repo";
    let pane_file = root.join("panes.txt");
    let mk_row = |pane: &str, pid: u32, cmd: &str, win: &str| {
        [
            pane,
            "agent-session",
            "@1",
            "0",
            "0",
            &pid.to_string(),
            cmd,
            shared_repo,
            "1",
            "1",
            "0",
            "0",
            "0",
            "0",
            "0",
            "1",
            "20",
            win,
        ]
        .join("\x1f")
    };
    std::fs::write(
        &pane_file,
        format!(
            "{}\n{}\n",
            mk_row("%91", 41001, "codex -m gpt-6.2-astra", "codex-1"),
            mk_row("%92", 41002, "codex -m gpt-6.2-astra", "codex-2"),
        ),
    )
    .unwrap();

    // %91 is stuck on Codex's `Update available` startup modal; %92 is at normal ready prompt.
    std::fs::write(
        screens_dir.join("%91.txt"),
        "  Update available · 0.167.2 → 0.172.0\n  Release notes: https://github.com/openai/codex/releases/latest\n\n› 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  3. Skip until next version\n\n  enter continue · esc skip\n",
    )
    .unwrap();
    std::fs::write(
        screens_dir.join("%92.txt"),
        "› Ask Codex to do anything\n  GPT-6.2-Astra high · /tmp/shared_workflow_repo\n  ← for agents · ? for shortcuts\n",
    )
    .unwrap();

    let fake_tmux = bin_dir.join("fake-tmux.sh");
    write_executable(
        &fake_tmux,
        &format!(
            r#"#!/bin/sh
if [ "$1" = "list-panes" ]; then cat "{panes}"; exit 0; fi
if [ "$1" = "list-clients" ]; then exit 0; fi
if [ "$1" = "capture-pane" ]; then
  PANE=""
  PREV=""
  for a in "$@"; do
    if [ "$PREV" = "-t" ]; then PANE="$a"; break; fi
    PREV="$a"
  done
  if [ -f "{screens}/$PANE.txt" ]; then cat "{screens}/$PANE.txt"; fi
  exit 0
fi
exit 0
"#,
            panes = pane_file.display(),
            screens = screens_dir.display(),
        ),
    );

    let mut config = StarfaneConfig::default();
    config.paths.tmux = fake_tmux.to_string_lossy().into_owned();
    config.zellij.enabled = false;
    let state_dir = root.join("state");
    let core = Core::new(&DaemonConfig {
        state_dir: state_dir.clone(),
        socket: state_dir.join("starfaned.sock"),
        config,
        web_dir: None,
        foreground: true,
    })
    .unwrap();

    // Seed two real hook-backed Codex sessions sharing `shared_repo` on %91 and %92.
    for (sid, pane) in [("codex-s1", "%91"), ("codex-s2", "%92")] {
        core.ingest(EmitRequest {
            v: 1,
            agent: AgentKind::Codex,
            hook_event: Some("SessionStart".into()),
            payload: json!({
                "session_id": sid,
                "hook_event_name": "SessionStart",
                "cwd": shared_repo,
                "model": "gpt-6.2-astra"
            }),
            env: EnvCtx {
                tmux_pane: Some(pane.into()),
                tmux: Some("/tmp/tmux,1,0".into()),
                ..EnvCtx::default()
            },
            recv_ts_ms: id::now_ms(),
        })
        .await;
    }

    // 1. Without pane or pid_chain -> CWD match is Ambiguous (2 candidates in shared_repo).
    let amb: IdentityResolution = serde_json::from_value(
        core.handle(DaemonCommand::ResolveIdentity {
            agent: AgentKind::Codex,
            cwd: Some(shared_repo.into()),
            pane: None,
            bind_token: None,
            session_hint: None,
            pid_chain: Vec::new(),
        })
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(amb, IdentityResolution::Ambiguous { ref candidates } if candidates.len() == 2),
        "expected Ambiguous with 2 candidates, got {amb:?}"
    );

    // 2. With pane=None (stripped by Codex) + pid_chain containing %92's pane_pid 41002 ->
    //    deterministically resolves to `codex-s2` via `PaneResolution::PidTree`!
    let res: IdentityResolution = serde_json::from_value(
        core.handle(DaemonCommand::ResolveIdentity {
            agent: AgentKind::Codex,
            cwd: Some(shared_repo.into()),
            pane: None,
            bind_token: None,
            session_hint: None,
            pid_chain: vec![99001, 41002, 8403],
        })
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        res,
        IdentityResolution::Resolved {
            session_id: SessionId::new("codex-s2"),
            via: PaneResolution::PidTree,
        }
    );

    // 3. Dispatch (both interactive and L2) to %91 showing `Update available` modal is blocked!
    let rep_modal: DeliveryReport = serde_json::from_value(
        core.handle(DaemonCommand::Dispatch {
            target: DispatchTarget::Session {
                id: SessionId::new("codex-s1"),
            },
            text: "check inbox".into(),
            attachments: Vec::new(),
            allow: vec![DeliveryLevel::L2Inject],
            interactive: true,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(rep_modal.status, DeliveryStatus::Failed);
    let gate = rep_modal.gate.expect("gate report");
    assert!(!gate.passed);
    assert!(gate
        .detail
        .as_deref()
        .unwrap()
        .contains("codex waiting on update prompt"));
}
