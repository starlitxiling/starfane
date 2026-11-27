//! Gate / resolution / focus matrix tests (pure) + a sandboxed real-tmux integration test.
//!
//! The integration test (`sandbox_*`) is `#[ignore]`d per contracts §10; run it with
//! `cargo test -p starfane-tmux -- --ignored`. It uses its own `tmux -L starfane-test-gate-<pid>`
//! server (never the user's) and kills it on drop.

use starfane_proto::{AgentKind, GateCheck, PaneId, PaneResolution, SessionState, StageKind};
use starfane_tmux::{
    adaptive_paste_settle_ms, check_injection_gate, check_injection_gate_with_screen,
    detect_blocking_startup_prompt, detect_codex_update_prompt, detect_folder_trust_prompt,
    is_bare_slash_command, is_paste_stuck_in_prompt, is_user_watching, parse_client_line,
    parse_clients, parse_pane_line, parse_panes, pid_ancestors, resolve_pane,
    validate_pointer_text, ClientInfo, GateConfig, PaneHint, PaneInfo, PaneStatus, SpawnWindow,
    Tmux, TmuxError, FIELD_SEP,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEST_INSTANCE_SEQ: AtomicU64 = AtomicU64::new(1);

const NOW: u64 = 1_000_000;

fn pane(id: &str) -> PaneInfo {
    PaneInfo {
        pane_id: PaneId::new(id),
        session_name: "work".into(),
        window_id: "@1".into(),
        window_index: 0,
        pane_index: 0,
        pane_pid: 4242,
        current_command: "claude".into(),
        current_path: "/repo".into(),
        pane_active: true,
        window_active: true,
        session_attached: 1,
        in_mode: false,
        dead: false,
        synchronized: false,
        alternate_on: false,
        cursor_x: 2,
        cursor_y: 10,
        window_name: "code".into(),
        session_group: String::new(),
        history_size: 0,
        pane_height: 24,
    }
}

fn client(session: &str, activity: u64) -> ClientInfo {
    ClientInfo {
        tty: "/dev/ttys001".into(),
        session_name: session.into(),
        activity_s: activity,
        flags: "attached,focused".into(),
        session_group: String::new(),
        window_id: String::new(),
    }
}

fn gate(
    p: &PaneInfo,
    clients: &[ClientInfo],
    state: SessionState,
    agent: AgentKind,
    line: &str,
) -> starfane_proto::GateReport {
    check_injection_gate(p, clients, state, agent, line, &GateConfig::default(), NOW)
}

fn ok_gate(p: &PaneInfo) -> starfane_proto::GateReport {
    gate(p, &[], SessionState::Idle, AgentKind::Claude, "> ")
}

// ---------------------------------------------------------------- gate: individual checks

#[test]
fn gate_all_pass() {
    let r = ok_gate(&pane("%1"));
    assert!(r.passed, "{r:?}");
    assert_eq!(r.checks.len(), 6);
    assert!(r.checks.iter().all(|(_, ok)| *ok));
    assert!(r.detail.is_none());
    let order: Vec<GateCheck> = r.checks.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        order,
        vec![
            GateCheck::PaneAlive,
            GateCheck::NotInMode,
            GateCheck::NotSynchronized,
            GateCheck::AgentIdle,
            GateCheck::UserInactive,
            GateCheck::InputEmpty
        ]
    );
}

#[test]
fn gate_never_reports_ack() {
    let r = ok_gate(&pane("%1"));
    assert!(!r.checks.iter().any(|(c, _)| *c == GateCheck::Ack));
}

#[test]
fn gate_pane_dead() {
    let mut p = pane("%1");
    p.dead = true;
    let r = ok_gate(&p);
    assert!(!r.passed);
    assert_eq!(r.failed_checks(), vec![GateCheck::PaneAlive]);
}

#[test]
fn gate_pane_in_mode() {
    let mut p = pane("%1");
    p.in_mode = true;
    assert_eq!(ok_gate(&p).failed_checks(), vec![GateCheck::NotInMode]);
}

#[test]
fn gate_pane_synchronized() {
    let mut p = pane("%1");
    p.synchronized = true;
    assert_eq!(
        ok_gate(&p).failed_checks(),
        vec![GateCheck::NotSynchronized]
    );
}

#[test]
fn gate_only_idle_state_passes() {
    let p = pane("%1");
    let states = [
        (SessionState::Unknown, false),
        (SessionState::Starting, false),
        (SessionState::Working, false),
        (SessionState::Idle, true),
        (SessionState::NeedsPermission, false),
        (SessionState::NeedsInput, false),
        (SessionState::Error, false),
        (SessionState::Suspended, false),
        (SessionState::Ended, false),
    ];
    for (st, expect) in states {
        let r = gate(&p, &[], st, AgentKind::Claude, "> ");
        assert_eq!(r.passed, expect, "state {st:?}");
        if !expect {
            assert_eq!(r.failed_checks(), vec![GateCheck::AgentIdle], "{st:?}");
            assert!(r.detail.as_deref().unwrap().contains("not Idle"));
        }
    }
}

#[test]
fn gate_user_activity_threshold_is_strict() {
    let p = pane("%1");
    // default user_inactive_secs = 15: need quiet > 15s.
    for (age, expect) in [
        (0, false),
        (1, false),
        (14, false),
        (15, false),
        (16, true),
        (3600, true),
    ] {
        let r = gate(
            &p,
            &[client("work", NOW - age)],
            SessionState::Idle,
            AgentKind::Claude,
            "> ",
        );
        assert_eq!(r.passed, expect, "age {age}s");
        if !expect {
            assert_eq!(r.failed_checks(), vec![GateCheck::UserInactive]);
        }
    }
}

#[test]
fn gate_user_activity_future_timestamp_counts_as_active() {
    let r = gate(
        &pane("%1"),
        &[client("work", NOW + 500)],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
    );
    assert_eq!(r.failed_checks(), vec![GateCheck::UserInactive]);
}

#[test]
fn gate_user_activity_uses_newest_client_of_same_session_only() {
    let p = pane("%1");
    // Stale client + fresh client in the same session: the fresh one wins → blocked.
    let r = gate(
        &p,
        &[client("work", NOW - 9999), client("work", NOW - 2)],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
    );
    assert_eq!(r.failed_checks(), vec![GateCheck::UserInactive]);
    // Busy client attached to a *different* session must not block this pane.
    let r = gate(
        &p,
        &[client("other", NOW - 1), client("work", NOW - 300)],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
    );
    assert!(r.passed, "{r:?}");
}

#[test]
fn gate_custom_user_inactive_secs() {
    let cfg = GateConfig {
        user_inactive_secs: 60,
        ..GateConfig::default()
    };
    let r = check_injection_gate(
        &pane("%1"),
        &[client("work", NOW - 30)],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
        &cfg,
        NOW,
    );
    assert_eq!(r.failed_checks(), vec![GateCheck::UserInactive]);
}

#[test]
fn gate_input_empty_matrix() {
    let p = pane("%1");
    let cases: &[(AgentKind, &str, bool)] = &[
        // claude
        (AgentKind::Claude, "> ", true),
        (AgentKind::Claude, ">", true), // capture-pane trims trailing blanks
        (AgentKind::Claude, ">    ", true),
        (AgentKind::Claude, "❯\u{a0}", true),
        (AgentKind::Claude, "❯ ", true),
        (AgentKind::Claude, "❯", true),
        (AgentKind::Claude, "│ >                    │", true),
        (AgentKind::Claude, "│ > ", true),
        (AgentKind::Claude, "> fix the bug", false),
        (AgentKind::Claude, "❯ fix the bug", false),
        (AgentKind::Claude, "│ > half typed │", false),
        (AgentKind::Claude, "$ ls", false),
        (AgentKind::Claude, "", false),   // cannot tell → fail closed
        (AgentKind::Claude, "› ", false), // codex prompt is not claude's
        // codex
        (AgentKind::Codex, "› ", true),
        (AgentKind::Codex, "›", true),
        (AgentKind::Codex, "› Ask Codex to do anything", true),
        (AgentKind::Codex, "› draft", false),
        (AgentKind::Codex, "> ", false),
        // agy
        (AgentKind::Agy, "> ", true),
        (AgentKind::Agy, ">", true),
        (AgentKind::Agy, "> /sw", false),
        (
            AgentKind::Agy,
            "Do you trust the contents of this project?",
            false,
        ),
    ];
    for (agent, line, expect) in cases {
        let r = gate(&p, &[], SessionState::Idle, *agent, line);
        assert_eq!(r.passed, *expect, "{agent:?} {line:?}");
        if !expect {
            assert_eq!(r.failed_checks(), vec![GateCheck::InputEmpty], "{line:?}");
        }
    }
    // If cursor_x > 4 on Codex placeholder text, user actually typed it as a draft.
    let mut moved = pane("%1");
    moved.cursor_x = 15;
    let r_typed = gate(
        &moved,
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "› Ask Codex to do anything",
    );
    assert!(!r_typed.passed);
    assert_eq!(r_typed.failed_checks(), vec![GateCheck::InputEmpty]);
}

#[test]
fn gate_draft_vs_unrecognised_detail() {
    let p = pane("%1");
    let d = gate(&p, &[], SessionState::Idle, AgentKind::Claude, "> hi");
    assert!(d.detail.unwrap().contains("draft"));
    let u = gate(&p, &[], SessionState::Idle, AgentKind::Claude, "~/repo $");
    assert!(u.detail.unwrap().contains("not a recognised idle prompt"));
}

#[test]
fn gate_blocks_folder_trust_prompts_for_codex_claude_and_agy() {
    let p = pane("%1");
    let cfg = GateConfig::default();

    let codex_screen = "\
  Folder access
  /Users/wujie/Work/workflow

  Trust this folder? Codex can read, edit, and run files here, subject to your permission
  settings. Folder settings can run code automatically, even without a model request. Continue
  only if you trust these files. Your trust decision will be saved.

› 1. Trust and continue
  2. Back to Agent Command Center

  enter continue · esc back
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Codex, codex_screen),
        Some("codex waiting for folder trust approval")
    );
    // Even if cursor sits on a blank row below the Codex trust menu, screen check catches it!
    let r_codex = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "",
        Some(codex_screen),
        &cfg,
        NOW,
    );
    assert!(!r_codex.passed);
    assert_eq!(r_codex.failed_checks(), vec![GateCheck::InputEmpty]);
    assert!(r_codex
        .detail
        .as_deref()
        .unwrap()
        .contains("codex waiting for folder trust approval"));

    // Single-line cursor on Codex trust option or older prompt text also fails with trust reason.
    for line in [
        "› 1. Trust and continue",
        "Trust this folder?",
        "Do you trust the contents of this directory?",
        "Folder access",
    ] {
        let r = gate(&p, &[], SessionState::Idle, AgentKind::Codex, line);
        assert!(!r.passed, "{line:?}");
        assert_eq!(r.failed_checks(), vec![GateCheck::InputEmpty], "{line:?}");
        assert!(
            r.detail
                .as_deref()
                .unwrap()
                .contains("codex waiting for folder trust approval"),
            "{line:?}: {:?}",
            r.detail
        );
    }

    let claude_screen = "\
────────────────────────────────────────────────────────────────────────────
 Accessing workspace:
 /Users/wujie/Work/new_repo
 Quick safety check: Is this a project you created or one you trust?
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 ❯ No, exit
   Yes, I trust this folder
 Enter to confirm · Esc to cancel
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Claude, claude_screen),
        Some("claude waiting for folder trust approval")
    );
    let r_claude = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "❯ No, exit",
        Some(claude_screen),
        &cfg,
        NOW,
    );
    assert!(!r_claude.passed);
    assert_eq!(r_claude.failed_checks(), vec![GateCheck::InputEmpty]);
    assert!(r_claude
        .detail
        .as_deref()
        .unwrap()
        .contains("claude waiting for folder trust approval"));

    for line in [
        "Do you trust the files in this folder?",
        "Quick safety check: Is this a project you created or one you trust?",
        "❯ Yes, I trust this folder",
    ] {
        let r = gate(&p, &[], SessionState::Idle, AgentKind::Claude, line);
        assert!(!r.passed, "{line:?}");
        assert!(r
            .detail
            .as_deref()
            .unwrap()
            .contains("claude waiting for folder trust approval"));
    }

    let agy_screen = "\
Accessing workspace:
/private/tmp/untrusted
Do you trust the contents of this project?
Antigravity CLI requires permission to read, edit, and execute files here.
> Yes, I trust this folder
  No, exit
  ↑/↓ Navigate · enter Confirm
                                                        Gemini 4 Neon · xhigh
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Agy, agy_screen),
        Some("agy waiting for folder trust approval")
    );
    let r_agy = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Agy,
        "Gemini 4 Neon · xhigh",
        Some(agy_screen),
        &cfg,
        NOW,
    );
    assert!(!r_agy.passed);
    assert_eq!(r_agy.failed_checks(), vec![GateCheck::InputEmpty]);
    assert!(r_agy
        .detail
        .as_deref()
        .unwrap()
        .contains("agy waiting for folder trust approval"));

    // False-positive guard: normal chat output discussing trust prompts alongside active chat chrome
    // MUST NOT be misclassified as a trust dialog!
    let normal_codex_chat = "\
• Fixed detection of `Folder access` / `Trust this folder?` / `Trust and continue`.

› Ask Codex to do anything

  GPT-6.2-Sol xhigh · ~/Work/workflow
  ← for agents · ? for shortcuts
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Codex, normal_codex_chat),
        None
    );
    let r_normal = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "› Ask Codex to do anything",
        Some(normal_codex_chat),
        &cfg,
        NOW,
    );
    assert!(r_normal.passed, "{r_normal:?}");

    let normal_agy_chat = "\
Implemented trust detection for 'Do you trust the contents of this project?' and 'Yes, I trust this folder'.
────────────────────────────────────────
> 
────────────────────────────────────────
? for shortcuts    auto · Gemini 4 Neon · xhigh
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Agy, normal_agy_chat),
        None
    );
}

#[test]
fn gate_blocks_claude_project_mcp_server_confirmation_prompt() {
    let p = pane("%31");
    let cfg = GateConfig::default();

    // Exact live `%31` (`openkernel-claude`) capture including previous session footer in scrollback above!
    let claude_mcp_screen = "\
────────────────────────────────────────────────────────────────────────────────
❯ 
────────────────────────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents · ? for shortcuts
❯ claude --allow-dangerously-skip-permissions

────────────────────────────────────────────────────────────────────────────────
  New MCP server found in this project: ida-pro

  MCP servers may execute code or access system resources. All tool calls require approval. Learn more in the MCP documentation.

    Use this MCP server
    Use this and all future MCP servers in this project
  ❯ Continue without using this MCP server

  Enter to confirm · Esc to cancel
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Claude, claude_mcp_screen),
        Some("claude waiting for mcp server approval")
    );
    assert_eq!(
        detect_blocking_startup_prompt(AgentKind::Claude, claude_mcp_screen),
        Some("claude waiting for mcp server approval")
    );
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Codex, claude_mcp_screen),
        None
    );

    let r = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "❯ Continue without using this MCP server",
        Some(claude_mcp_screen),
        &cfg,
        NOW,
    );
    assert!(!r.passed);
    assert_eq!(r.failed_checks(), vec![GateCheck::InputEmpty]);
    assert!(r
        .detail
        .as_deref()
        .unwrap()
        .contains("claude waiting for mcp server approval"));

    // Plural MCP servers variant and single-line cursor checks.
    for line in [
        "❯ Continue without using this MCP server",
        "❯ Use this and all future MCP servers in this project",
        "❯ Use this MCP server",
        "New MCP servers found in this project: ida-pro, playwright",
        "❯ Continue without using these MCP servers",
    ] {
        let r_line = gate(&p, &[], SessionState::Idle, AgentKind::Claude, line);
        assert!(!r_line.passed, "{line:?}");
        assert_eq!(
            r_line.failed_checks(),
            vec![GateCheck::InputEmpty],
            "{line:?}"
        );
        assert!(
            r_line
                .detail
                .as_deref()
                .unwrap()
                .contains("claude waiting for mcp server approval"),
            "{line:?}: {:?}",
            r_line.detail
        );
    }

    // False-positive guard: normal Claude chat discussing MCP server approval alongside live chat chrome.
    let normal_claude_chat = "\
Added detection for 'Continue without using this MCP server' and 'Use this and all future MCP servers in this project'.
────────────────────────────────────────
❯ 
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";
    assert_eq!(
        detect_folder_trust_prompt(AgentKind::Claude, normal_claude_chat),
        None
    );
    let r_ok = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "❯ ",
        Some(normal_claude_chat),
        &cfg,
        NOW,
    );
    assert!(r_ok.passed, "{r_ok:?}");
}

#[test]
fn gate_blocks_codex_update_available_startup_modal() {
    let p = pane("%1");
    let cfg = GateConfig::default();

    let update_screen = "\
  Update available · 0.167.2 → 0.172.0
  Release notes: https://github.com/openai/codex/releases/latest

› 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version

  enter continue · esc skip
";
    assert_eq!(
        detect_codex_update_prompt(AgentKind::Codex, update_screen),
        Some("codex waiting on update prompt")
    );
    assert_eq!(
        detect_blocking_startup_prompt(AgentKind::Codex, update_screen),
        Some("codex waiting on update prompt")
    );
    assert_eq!(
        detect_codex_update_prompt(AgentKind::Claude, update_screen),
        None
    );

    // Even when cursor sits on a blank row below the update modal, screen check blocks injection.
    let r = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "",
        Some(update_screen),
        &cfg,
        NOW,
    );
    assert!(!r.passed);
    assert_eq!(r.failed_checks(), vec![GateCheck::InputEmpty]);
    assert!(r
        .detail
        .as_deref()
        .unwrap()
        .contains("codex waiting on update prompt"));

    // Single-line cursor on the `› 1. Update now` option is also blocked cleanly.
    for line in [
        "› 1. Update now (runs `npm install -g @openai/codex`)",
        "Update available · 0.167.2 → 0.172.0",
        "enter continue · esc skip",
    ] {
        let r_line = gate(&p, &[], SessionState::Idle, AgentKind::Codex, line);
        assert!(!r_line.passed, "{line:?}");
        assert_eq!(
            r_line.failed_checks(),
            vec![GateCheck::InputEmpty],
            "{line:?}"
        );
        assert!(
            r_line
                .detail
                .as_deref()
                .unwrap()
                .contains("codex waiting on update prompt"),
            "{line:?}: {:?}",
            r_line.detail
        );
    }

    // False-positive guard: normal Codex chat mentioning Update available alongside chat chrome.
    let normal_chat = "\
• Fixed handling of `Update available · 0.167.2 → 0.172.0` and `› 1. Update now`.

› Ask Codex to do anything

  GPT-6.2-Sol xhigh · ~/Work/workflow
  ← for agents · ? for shortcuts
";
    assert_eq!(
        detect_codex_update_prompt(AgentKind::Codex, normal_chat),
        None
    );
    let r_ok = check_injection_gate_with_screen(
        &p,
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "› Ask Codex to do anything",
        Some(normal_chat),
        &cfg,
        NOW,
    );
    assert!(r_ok.passed, "{r_ok:?}");
}

#[test]
fn gate_agent_without_configured_prompts_fails_closed() {
    let mut cfg = GateConfig::default();
    cfg.idle_prompts.remove(&AgentKind::Codex);
    let r = check_injection_gate(
        &pane("%1"),
        &[],
        SessionState::Idle,
        AgentKind::Codex,
        "› ",
        &cfg,
        NOW,
    );
    assert_eq!(r.failed_checks(), vec![GateCheck::InputEmpty]);
}

#[test]
fn gate_custom_prompt_prefix_is_honoured() {
    let mut cfg = GateConfig::default();
    cfg.idle_prompts
        .insert(AgentKind::Agy, vec!["agy> ".to_string()]);
    let f = |line: &str| {
        check_injection_gate(
            &pane("%1"),
            &[],
            SessionState::Idle,
            AgentKind::Agy,
            line,
            &cfg,
            NOW,
        )
        .passed
    };
    assert!(f("agy> "));
    assert!(f("agy>"));
    assert!(!f("> "));
}

#[test]
fn gate_multiple_failures_are_all_reported() {
    let mut p = pane("%1");
    p.dead = true;
    p.in_mode = true;
    p.synchronized = true;
    let r = gate(
        &p,
        &[client("work", NOW)],
        SessionState::Working,
        AgentKind::Claude,
        "> draft",
    );
    assert!(!r.passed);
    assert_eq!(r.failed_checks().len(), 6);
    assert!(r.detail.is_some());
}

// ---------------------------------------------------------------- gate point 7: slash text

#[test]
fn bare_slash_command_detection() {
    for t in ["/swarm", "/help", "  /boost  ", "/teamwork\t"] {
        assert!(is_bare_slash_command(t), "{t:?}");
    }
    for t in [
        "/swarm starfane: new task T-1, call starfane.read_inbox",
        "starfane: /swarm",
        "starfane: check inbox",
        "/path/to/file is broken",
    ] {
        assert!(!is_bare_slash_command(t), "{t:?}");
    }
}

#[test]
fn pointer_text_validation() {
    let cfg = GateConfig::default();
    assert!(validate_pointer_text(
        "starfane: new task P-1, please call starfane.read_inbox",
        &cfg
    )
    .is_ok());
    assert!(validate_pointer_text("/swarm starfane: task P-1", &cfg).is_ok());
    assert!(
        validate_pointer_text("starfane: 新任务 P-1，请调用 starfane.read_inbox", &cfg).is_ok()
    );

    let bad = [
        "",
        "   ",
        "line1\nline2",
        "line1\rline2",
        "trail\n",
        "esc\x1b[201~injected",
        "nul\0byte",
        "tab\there",
        "/swarm",
        "/boost  ",
    ];
    for t in bad {
        assert!(
            matches!(
                validate_pointer_text(t, &cfg),
                Err(TmuxError::InvalidText(_))
            ),
            "{t:?}"
        );
    }
}

#[test]
fn pointer_text_length_counts_chars_not_bytes() {
    let cfg = GateConfig::default();
    let exact = "字".repeat(cfg.max_pointer_len); // 900 bytes, 300 chars
    assert!(validate_pointer_text(&exact, &cfg).is_ok());
    let over = "字".repeat(cfg.max_pointer_len + 1);
    assert!(matches!(
        validate_pointer_text(&over, &cfg),
        Err(TmuxError::InvalidText(_))
    ));
}

// ---------------------------------------------------------------- is_user_watching

#[test]
fn watching_requires_visibility() {
    let fresh = [client("work", NOW - 1)];
    let base = pane("%1");
    assert!(is_user_watching(&base, &fresh, NOW, 30));

    let mut p = base.clone();
    p.session_attached = 0;
    assert!(!is_user_watching(&p, &fresh, NOW, 30));
    let mut p = base.clone();
    p.window_active = false;
    assert!(!is_user_watching(&p, &fresh, NOW, 30));
    let mut p = base;
    p.pane_active = false;
    assert!(!is_user_watching(&p, &fresh, NOW, 30));
}

#[test]
fn watching_activity_recency_threshold() {
    let p = pane("%1");
    for (age, expect) in [
        (0, true),
        (29, true),
        (30, false),
        (31, false),
        (86_400, false),
    ] {
        assert_eq!(
            is_user_watching(&p, &[client("work", NOW - age)], NOW, 30),
            expect,
            "age {age}"
        );
    }
}

#[test]
fn watching_multi_client_focus_events_off() {
    // focus-events off: tmux still reports every attached client as visible. A stale second
    // client must not make the pane look watched, and a fresh one must.
    let p = pane("%1");
    let stale_only = [client("work", NOW - 4000), client("work", NOW - 9000)];
    assert!(!is_user_watching(&p, &stale_only, NOW, 30));
    let one_fresh = [client("work", NOW - 4000), client("work", NOW - 3)];
    assert!(is_user_watching(&p, &one_fresh, NOW, 30));
    // A fresh client on a different session says nothing about this pane.
    let other_session = [client("other", NOW - 1), client("work", NOW - 4000)];
    assert!(!is_user_watching(&p, &other_session, NOW, 30));
}

#[test]
fn watching_attached_without_client_records_is_conservative() {
    assert!(is_user_watching(&pane("%1"), &[], NOW, 30));
    let mut p = pane("%1");
    p.session_attached = 0;
    assert!(!is_user_watching(&p, &[], NOW, 30));
}

#[test]
fn watching_future_activity_is_recent() {
    assert!(is_user_watching(
        &pane("%1"),
        &[client("work", NOW + 9)],
        NOW,
        30
    ));
}

// ---------------------------------------------------------------- resolve_pane

fn hint(agent: AgentKind) -> PaneHint {
    PaneHint {
        agent,
        env_pane: None,
        pid_chain: vec![],
        cwd: None,
    }
}

fn panes3() -> Vec<PaneInfo> {
    let mut a = pane("%1");
    a.pane_pid = 100;
    a.current_path = "/a".into();
    let mut b = pane("%2");
    b.pane_pid = 200;
    b.current_path = "/b".into();
    b.current_command = "codex".into();
    let mut c = pane("%3");
    c.pane_pid = 300;
    c.current_path = "/c".into();
    c.current_command = "zsh".into();
    vec![a, b, c]
}

#[test]
fn resolve_env_wins_over_everything() {
    let mut h = hint(AgentKind::Claude);
    h.env_pane = Some(PaneId::new("%3"));
    h.pid_chain = vec![999, 200];
    h.cwd = Some("/a".into());
    assert_eq!(
        resolve_pane(&panes3(), &h),
        (Some(PaneId::new("%3")), PaneResolution::Env)
    );
}

#[test]
fn resolve_stale_env_falls_through_to_pid_tree() {
    let mut h = hint(AgentKind::Codex);
    h.env_pane = Some(PaneId::new("%99"));
    h.pid_chain = vec![5555, 200, 1];
    assert_eq!(
        resolve_pane(&panes3(), &h),
        (Some(PaneId::new("%2")), PaneResolution::PidTree)
    );
}

#[test]
fn resolve_pid_chain_nearest_ancestor_wins() {
    let mut h = hint(AgentKind::Claude);
    h.pid_chain = vec![7, 300, 100]; // 300 is nearer than 100
    assert_eq!(
        resolve_pane(&panes3(), &h),
        (Some(PaneId::new("%3")), PaneResolution::PidTree)
    );
}

#[test]
fn resolve_pid_beats_cwd() {
    let mut h = hint(AgentKind::Claude);
    h.pid_chain = vec![300];
    h.cwd = Some("/a".into());
    assert_eq!(resolve_pane(&panes3(), &h).1, PaneResolution::PidTree);
}

#[test]
fn resolve_cwd_unique_same_agent() {
    let mut h = hint(AgentKind::Codex);
    h.cwd = Some("/b/".into()); // trailing slash tolerated
    assert_eq!(
        resolve_pane(&panes3(), &h),
        (Some(PaneId::new("%2")), PaneResolution::CwdMatch)
    );
}

#[test]
fn resolve_cwd_requires_same_agent_class() {
    let mut h = hint(AgentKind::Agy);
    h.cwd = Some("/b".into()); // pane %2 runs codex
    assert_eq!(resolve_pane(&panes3(), &h), (None, PaneResolution::Unknown));
    let mut h = hint(AgentKind::Claude);
    h.cwd = Some("/c".into()); // pane %3 runs zsh
    assert_eq!(resolve_pane(&panes3(), &h), (None, PaneResolution::Unknown));
}

#[test]
fn resolve_cwd_ambiguous_is_unknown() {
    let mut ps = panes3();
    let mut dup = pane("%4");
    dup.current_path = "/a".into();
    ps.push(dup);
    let mut h = hint(AgentKind::Claude);
    h.cwd = Some("/a".into());
    assert_eq!(resolve_pane(&ps, &h), (None, PaneResolution::Unknown));
}

#[test]
fn resolve_cwd_ignores_dead_panes() {
    let mut ps = panes3();
    let mut dead = pane("%4");
    dead.current_path = "/a".into();
    dead.dead = true;
    ps.push(dead);
    let mut h = hint(AgentKind::Claude);
    h.cwd = Some("/a".into());
    assert_eq!(
        resolve_pane(&ps, &h),
        (Some(PaneId::new("%1")), PaneResolution::CwdMatch)
    );
}

#[test]
fn resolve_claude_version_style_command() {
    let mut p = pane("%1");
    p.current_command = "2.1.45".into();
    p.current_path = "/r".into();
    let mut h = hint(AgentKind::Claude);
    h.cwd = Some("/r".into());
    assert_eq!(resolve_pane(&[p.clone()], &h).1, PaneResolution::CwdMatch);
    let mut h = hint(AgentKind::Codex);
    h.cwd = Some("/r".into());
    assert_eq!(resolve_pane(&[p], &h).1, PaneResolution::Unknown);
}

#[test]
fn resolve_nothing_known_and_empty_list() {
    assert_eq!(
        resolve_pane(&panes3(), &hint(AgentKind::Claude)),
        (None, PaneResolution::Unknown)
    );
    let mut h = hint(AgentKind::Claude);
    h.env_pane = Some(PaneId::new("%1"));
    h.pid_chain = vec![100];
    h.cwd = Some("/a".into());
    assert_eq!(resolve_pane(&[], &h), (None, PaneResolution::Unknown));
}

#[test]
fn pid_ancestors_walks_up_from_self() {
    let me = std::process::id();
    let chain = pid_ancestors(me, 16);
    assert_eq!(chain.first(), Some(&me));
    assert!(chain.len() <= 16);
    let mut dedup = chain.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), chain.len(), "no cycles");
    assert!(!chain.contains(&1) && !chain.contains(&0));
    assert_eq!(pid_ancestors(me, 1), vec![me]);
    assert!(pid_ancestors(me, 0).is_empty());
    assert!(pid_ancestors(1, 8).is_empty());
}

// ---------------------------------------------------------------- parsing

fn pane_line(fields: &[&str]) -> String {
    fields.join(&FIELD_SEP.to_string())
}

const GOOD_PANE: [&str; 18] = [
    "%12",
    "my sess",
    "@3",
    "2",
    "1",
    "4242",
    "node",
    "/Users/me/my repo",
    "1",
    "0",
    "2",
    "0",
    "0",
    "1",
    "1",
    "17",
    "9",
    "workflow",
];

#[test]
fn parse_pane_line_ok() {
    let p = parse_pane_line(&pane_line(&GOOD_PANE)).unwrap();
    assert_eq!(p.pane_id, PaneId::new("%12"));
    assert_eq!(p.session_name, "my sess");
    assert_eq!(p.window_id, "@3");
    assert_eq!((p.window_index, p.pane_index, p.pane_pid), (2, 1, 4242));
    assert_eq!(p.current_command, "node");
    assert_eq!(p.current_path, "/Users/me/my repo");
    assert!(p.pane_active && !p.window_active);
    assert_eq!(p.session_attached, 2);
    assert!(!p.in_mode && !p.dead && p.synchronized && p.alternate_on);
    assert_eq!((p.cursor_x, p.cursor_y), (17, 9));
    assert_eq!(p.window_name, "workflow");
    assert_eq!(p.tmux_label(), "my sess-workflow");
}

#[test]
fn parse_pane_line_rejects_malformed() {
    let mut short = GOOD_PANE.to_vec();
    short.pop();
    let mut bad_id = GOOD_PANE;
    bad_id[0] = "12";
    let mut bad_num = GOOD_PANE;
    bad_num[5] = "abc";
    let mut bad_flag = GOOD_PANE;
    bad_flag[8] = "2";
    for line in [
        pane_line(&short),
        pane_line(&bad_id),
        pane_line(&bad_num),
        pane_line(&bad_flag),
        "garbage".to_string(),
    ] {
        assert!(
            matches!(parse_pane_line(&line), Err(TmuxError::Parse(_))),
            "{line:?}"
        );
    }
}

#[test]
fn parse_panes_skips_blank_lines_and_fails_on_bad_line() {
    let l = pane_line(&GOOD_PANE);
    let out = format!("{l}\n\n{l}\n");
    assert_eq!(parse_panes(&out).unwrap().len(), 2);
    assert!(parse_panes("").unwrap().is_empty());
    assert!(parse_panes(&format!("{l}\nbroken\n")).is_err());
}

#[test]
fn parse_clients_ok_and_bad() {
    let line = pane_line(&[
        "/dev/ttys004",
        "work",
        "1700000123",
        "attached,focused,UTF-8",
    ]);
    let c = parse_client_line(&line).unwrap();
    assert_eq!(c.tty, "/dev/ttys004");
    assert_eq!(c.session_name, "work");
    assert_eq!(c.activity_s, 1_700_000_123);
    assert_eq!(c.flags, "attached,focused,UTF-8");
    assert_eq!(
        parse_clients(&format!("{line}\n{line}\n")).unwrap().len(),
        2
    );
    assert!(parse_clients("").unwrap().is_empty());
    assert!(parse_client_line("a\x1fb").is_err());
    assert!(parse_client_line(&pane_line(&["t", "s", "-5", "f"])).is_err());
}

// ---------------------------------------------------------------- no tmux server / binary

fn unique_socket(tag: &str) -> String {
    let seq = TEST_INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("starfane-test-{tag}-{}-{seq}", std::process::id())
}

#[test]
fn missing_server_is_graceful() {
    let t = Tmux::new(PathBuf::from("tmux"), Some(unique_socket("absent")));
    if !t.available() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    assert!(t.list_panes().unwrap().is_empty());
    assert!(t.list_clients().unwrap().is_empty());
    t.kill_server().unwrap();
}

#[test]
fn missing_binary_is_reported() {
    let t = Tmux::new(PathBuf::from("/nonexistent/tmux-bin"), None);
    assert!(!t.available());
    assert!(matches!(t.list_panes(), Err(TmuxError::Io { .. })));
}

#[test]
fn invalid_requests_are_rejected_before_spawning() {
    let t = Tmux::new(PathBuf::from("/nonexistent/tmux-bin"), None);
    let cfg = GateConfig::default();
    // Validation errors must surface before any process is spawned (bin does not exist).
    assert!(matches!(
        t.inject_pointer(&PaneId::new("%1"), "line1\nline2", &cfg),
        Err(TmuxError::InvalidText(_))
    ));
    assert!(matches!(
        t.inject_pointer(&PaneId::new("bogus"), "starfane: hi", &cfg),
        Err(TmuxError::InvalidRequest(_))
    ));
    let mk = |argv: Vec<&str>, env: Vec<(&str, &str)>, session: Option<&str>| SpawnWindow {
        target_session: session.map(String::from),
        create_session: false,
        window_name: "w".into(),
        cwd: PathBuf::from("/tmp"),
        env: env.into_iter().map(|(a, b)| (a.into(), b.into())).collect(),
        argv: argv.into_iter().map(String::from).collect(),
    };
    for req in [
        mk(vec![], vec![], None),
        mk(vec!["/bin/true"], vec![("BAD KEY", "x")], None),
        mk(vec!["/bin/true"], vec![("1X", "x")], None),
        mk(vec!["/bin/true"], vec![], Some("a:b")),
        mk(vec!["/bin/true"], vec![], Some("")),
    ] {
        assert!(
            matches!(t.spawn_window(&req), Err(TmuxError::InvalidRequest(_))),
            "{req:?}"
        );
    }
    assert!(matches!(
        t.set_status(
            &PaneId::new("x"),
            &PaneStatus {
                state: SessionState::Idle,
                role: None,
                pipeline: None
            }
        ),
        Err(TmuxError::InvalidRequest(_))
    ));
}

// ---------------------------------------------------------------- real tmux sandbox

/// Owns a private tmux server; kills it (and removes the scratch dir) on drop, even on panic.
struct Sandbox {
    tmux: Tmux,
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Option<Sandbox> {
        let seq = TEST_INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let socket = format!("starfane-test-gate-{}-{seq}", std::process::id());
        let tmux = Tmux::new(PathBuf::from("tmux"), Some(socket));
        if !tmux.available() {
            return None;
        }
        let dir =
            std::env::temp_dir().join(format!("starfane-tmux-gate-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox dir");
        let dir = dir.canonicalize().expect("canonicalize sandbox dir");
        Some(Sandbox { tmux, dir })
    }
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Err(e) = self.tmux.kill_server() {
            eprintln!("sandbox kill-server failed: {e}");
        }
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            eprintln!("sandbox cleanup failed: {e}");
        }
    }
}

fn wait_for(path: &Path, what: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if !bytes.is_empty() {
                return bytes;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exists(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_sh(sb: &Sandbox, name: &str, script: &str, bind: &str) -> PaneId {
    sb.tmux
        .spawn_window(&SpawnWindow {
            target_session: None,
            create_session: true,
            window_name: name.into(),
            cwd: sb.dir.clone(),
            env: vec![("STARFANE_BIND".into(), bind.into())],
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        })
        .expect("spawn_window")
}

#[test]
#[ignore = "needs tmux; run with `cargo test -p starfane-tmux -- --ignored`"]
fn sandbox_spawn_list_inject_status() {
    let Some(sb) = Sandbox::new() else {
        panic!("tmux is required for this ignored test");
    };
    let ready = sb.file("ready_line");
    let got_line = sb.file("got_line");
    let bind_out = sb.file("bind_out");

    // Pane A: cooked tty, prints the line it receives after a paste + Enter.
    let script_a = format!(
        "printf '%s' \"$STARFANE_BIND\" > '{bind}'; : > '{ready}'; IFS= read -r line; printf '%s' \"$line\" > '{got}'; sleep 30",
        bind = bind_out.display(),
        ready = ready.display(),
        got = got_line.display()
    );
    let pane_a = spawn_sh(&sb, "agent-a", &script_a, "B-TESTTOKEN");
    wait_exists(&ready, "pane A ready");

    // STARFANE_BIND reached the child; default detached session "agent-session" was created directly on window 0.
    assert_eq!(wait_for(&bind_out, "STARFANE_BIND"), b"B-TESTTOKEN");
    let panes = sb.tmux.list_panes().expect("list_panes");
    let a = panes
        .iter()
        .find(|p| p.pane_id == pane_a)
        .expect("pane A listed");
    assert_eq!(a.session_name, starfane_tmux::DEFAULT_DETACHED_SESSION);
    assert_eq!(a.window_name, "agent-a");
    assert_eq!(panes.len(), 1, "no stray shell window created: {panes:?}");
    assert!(!a.dead && !a.in_mode && !a.synchronized);
    assert!(a.pane_pid > 1);
    assert_eq!(
        Path::new(&a.current_path).canonicalize().unwrap(),
        sb.dir,
        "cwd is honoured"
    );
    // Detached sandbox: nobody is watching, no clients.
    assert_eq!(a.session_attached, 0);
    assert!(sb.tmux.list_clients().unwrap().is_empty());
    assert!(!is_user_watching(a, &[], 0, 30));

    // PID-tree resolution against the real pane pid.
    let (resolved, via) = resolve_pane(
        &panes,
        &PaneHint {
            agent: AgentKind::Claude,
            env_pane: None,
            pid_chain: vec![999_999, a.pane_pid],
            cwd: None,
        },
    );
    assert_eq!(
        (resolved, via),
        (Some(pane_a.clone()), PaneResolution::PidTree)
    );

    // L2 injection: text must arrive intact as one line, submitted by C-m.
    let cfg = GateConfig::default();
    let text = "starfane: 新任务 P-1，请调用 starfane.read_inbox ($HOME `x` 'q' \"d\")";
    sb.tmux
        .inject_pointer(&pane_a, text, &cfg)
        .expect("inject_pointer");
    assert_eq!(
        String::from_utf8(wait_for(&got_line, "pane A line")).unwrap(),
        text
    );

    // The paste buffer must not leak (paste-buffer -d).
    let bufs = std::process::Command::new("tmux")
        .args([
            "-L",
            sb.tmux.socket_name.as_deref().unwrap(),
            "list-buffers",
        ])
        .output()
        .expect("list-buffers");
    assert!(
        String::from_utf8_lossy(&bufs.stdout).trim().is_empty(),
        "leaked buffers"
    );

    // Pane B: raw mode + bracketed-paste enabled; verify exact bytes: ESC[200~ text ESC[201~ CR.
    let ready_b = sb.file("ready_raw");
    let got_raw = sb.file("got_raw");
    let text_b = "starfane: raw check /swarm P-2";
    let expected_len = 6 + text_b.len() + 6 + 1;
    let script_b = format!(
        "printf '\\033[?2004h'; stty raw -echo; : > '{ready}'; dd bs=1 count={n} of='{got}' 2>/dev/null; sleep 30",
        ready = ready_b.display(),
        n = expected_len,
        got = got_raw.display()
    );
    let pane_b = spawn_sh(&sb, "agent-b", &script_b, "B-RAW");
    wait_exists(&ready_b, "pane B ready");
    std::thread::sleep(Duration::from_millis(200)); // let tmux register mode 2004 + raw tty
    sb.tmux
        .inject_pointer(&pane_b, text_b, &cfg)
        .expect("inject raw");
    let raw = wait_for(&got_raw, "pane B bytes");
    assert_eq!(raw, format!("\x1b[200~{text_b}\x1b[201~\r").into_bytes());

    // Validation failures never touch the pane.
    assert!(matches!(
        sb.tmux.inject_pointer(&pane_b, "/swarm", &cfg),
        Err(TmuxError::InvalidText(_))
    ));

    // Status options set / overwritten / cleared.
    let status = PaneStatus {
        state: SessionState::NeedsPermission,
        role: Some(StageKind::Implement),
        pipeline: Some("P-ABC".into()),
    };
    sb.tmux.set_status(&pane_a, &status).expect("set_status");
    let show = |opt: &str| -> String {
        let o = std::process::Command::new("tmux")
            .args([
                "-L",
                sb.tmux.socket_name.as_deref().unwrap(),
                "show-options",
                "-pqv",
                "-t",
                pane_a.as_str(),
                opt,
            ])
            .output()
            .expect("show-options");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    assert_eq!(show("@starfane_state"), "needs_permission");
    assert_eq!(show("@agent_state"), "needs_permission");
    assert_eq!(show("@starfane_role"), "implement");
    assert_eq!(show("@starfane_pipeline"), "P-ABC");
    sb.tmux
        .set_status(
            &pane_a,
            &PaneStatus {
                state: SessionState::Idle,
                role: None,
                pipeline: None,
            },
        )
        .unwrap();
    assert_eq!(show("@starfane_state"), "idle");
    assert_eq!(show("@starfane_role"), "");
    sb.tmux.clear_status(&pane_a).unwrap();
    assert_eq!(show("@starfane_state"), "");
    assert_eq!(show("@agent_state"), "");

    // cursor_line reads a real screen line; bell write succeeds on a live pane tty.
    let cur = sb.tmux.cursor_line(&pane_a, 0).expect("cursor_line");
    assert!(!cur.contains('\n'));
    sb.tmux.ring_bell(&pane_a).expect("ring_bell");

    // remain-on-exit failed: a crashing command leaves a dead pane we can still list.
    let crash = spawn_sh(&sb, "crash", "sleep 0.5; exit 3", "B-CRASH");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ps = sb.tmux.list_panes().unwrap();
        if ps.iter().any(|p| p.pane_id == crash && p.dead) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "crashed pane did not stay as dead"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let dead = sb
        .tmux
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.pane_id == crash)
        .unwrap();
    let r = check_injection_gate(
        &dead,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
        &cfg,
        NOW,
    );
    assert_eq!(r.failed_checks(), vec![GateCheck::PaneAlive]);

    // Explicit, non-existent target session with create_session=false is an error.
    let err = sb.tmux.spawn_window(&SpawnWindow {
        target_session: Some("nope".into()),
        create_session: false,
        window_name: "x".into(),
        cwd: sb.dir.clone(),
        env: vec![],
        argv: vec!["/bin/true".into()],
    });
    assert!(matches!(err, Err(TmuxError::Command { .. })), "{err:?}");

    // Explicit non-existent target session with create_session=true auto-creates the session cleanly.
    let p_custom = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("custom-auto".into()),
            create_session: true,
            window_name: "win-0".into(),
            cwd: sb.dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();
    let custom_panes: Vec<_> = sb
        .tmux
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.session_name == "custom-auto")
        .collect();
    assert_eq!(custom_panes.len(), 1);
    assert_eq!(custom_panes[0].pane_id, p_custom);
    assert_eq!(custom_panes[0].window_name, "win-0");
    assert_eq!(
        sb.tmux.list_sessions().unwrap(),
        vec![
            starfane_tmux::DEFAULT_DETACHED_SESSION.to_string(),
            "custom-auto".to_string()
        ]
    );

    // Focusing a detached pane selects its window/pane without requiring an attached client.
    let focus_res = sb.tmux.focus_pane(&p_custom).unwrap();
    assert_eq!(focus_res.pane_id, p_custom);
    assert_eq!(focus_res.session_name, "custom-auto");
    assert!(focus_res.switched_client.is_none());

    // Explicit existing session works.
    let p2 = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some(starfane_tmux::DEFAULT_DETACHED_SESSION.into()),
            create_session: false,
            window_name: "again".into(),
            cwd: sb.dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();
    assert!(sb
        .tmux
        .list_panes()
        .unwrap()
        .iter()
        .any(|p| p.pane_id == p2));

    // Drop kills the server; verify via a second handle after explicit drop.
    let socket = sb.tmux.clone();
    drop(sb);
    assert!(
        socket.list_panes().unwrap().is_empty(),
        "server must be gone after drop"
    );
}

#[test]
#[ignore = "needs tmux; run with `cargo test -p starfane-tmux -- --ignored`"]
fn sandbox_backup_and_restore_layout_idempotently() {
    use starfane_tmux::{
        build_snapshot, load_latest_snapshot, save_snapshot_to_dir, PaneAgentHint, RestoreOptions,
    };
    use std::collections::HashMap;

    let Some(sb) = Sandbox::new() else {
        panic!("tmux is required for this ignored test");
    };
    let dir_a = sb.dir.join("proj-a");
    let dir_b = sb.dir.join("proj-b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    // Session 1 (`work`): window 0 (`main`) with 2 split panes + window 1 (`agent`).
    let p_w0 = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("work".into()),
            create_session: true,
            window_name: "main".into(),
            cwd: dir_a.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();
    let split_out = std::process::Command::new(&sb.tmux.bin)
        .args([
            "-L",
            sb.tmux.socket_name.as_deref().unwrap(),
            "split-window",
            "-h",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            p_w0.as_str(),
            "-c",
            &dir_b.to_string_lossy(),
            "--",
            "/bin/sh",
            "-c",
            "sleep 30",
        ])
        .output()
        .unwrap();
    assert!(split_out.status.success(), "{split_out:?}");

    let p_w1 = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("work".into()),
            create_session: false,
            window_name: "agent".into(),
            cwd: dir_b.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();

    // Session 2 (`spot`): single window (`zsh`).
    let _p_spot = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("spot".into()),
            create_session: true,
            window_name: "zsh".into(),
            cwd: sb.dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();

    let windows = sb.tmux.list_windows().unwrap();
    let panes = sb.tmux.list_panes().unwrap();
    assert_eq!(windows.len(), 3);
    assert_eq!(panes.len(), 4);

    let mut hints = HashMap::new();
    hints.insert(
        p_w1,
        PaneAgentHint {
            agent: AgentKind::Codex,
            resume_id: Some("11111111-2222-4333-8444-555555555555".into()),
            model: Some("gpt-5".into()),
        },
    );
    let snap = build_snapshot(&windows, &panes, &[], &hints, &[], 123_456);
    let backup_dir = sb.dir.join("tmux-backups");
    let save_out = save_snapshot_to_dir(&backup_dir, &snap, 5, false).unwrap();
    assert!(save_out.saved);

    // Simulate reboot: kill the entire sandbox tmux server.
    sb.tmux.kill_server().unwrap();
    assert!(sb.tmux.list_sessions().unwrap().is_empty());

    let (_, loaded) = load_latest_snapshot(&backup_dir).unwrap().unwrap();
    let opts_dry = RestoreOptions {
        restore_agents: true,
        restore_bypass_permissions: false,
        dry_run: true,
        claude_bin: "claude".into(),
        codex_bin: "echo codex-dry".into(),
    };
    let dry_rep = sb.tmux.restore_snapshot(&loaded, &opts_dry).unwrap();
    assert!(dry_rep.dry_run);
    assert_eq!(dry_rep.restored_sessions, vec!["spot", "work"]);
    assert_eq!(dry_rep.restored_windows, 3);
    assert_eq!(dry_rep.restored_panes, 4);
    assert_eq!(dry_rep.resumed_agents.len(), 1);
    assert!(
        sb.tmux.list_sessions().unwrap().is_empty(),
        "dry_run must not start tmux server"
    );

    // Real restore (without running external codex CLI in sandbox: disable agent resume on first assert,
    // or use a harmless no-op command — wait: test `echo` via `codex_bin = "/bin/echo"`).
    let opts_real = RestoreOptions {
        restore_agents: true,
        restore_bypass_permissions: false,
        dry_run: false,
        claude_bin: "/bin/echo".into(),
        codex_bin: "/bin/echo".into(),
    };
    let rep = sb.tmux.restore_snapshot(&loaded, &opts_real).unwrap();
    assert!(!rep.dry_run);
    assert_eq!(rep.restored_sessions, vec!["spot", "work"]);
    assert!(rep.skipped_sessions.is_empty());
    assert_eq!(rep.restored_windows, 3);
    assert_eq!(rep.restored_panes, 4);
    assert_eq!(rep.resumed_agents.len(), 1);
    assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);

    let restored_sessions = sb.tmux.list_sessions().unwrap();
    assert_eq!(restored_sessions, vec!["spot", "work"]);
    let restored_windows = sb.tmux.list_windows().unwrap();
    let restored_panes = sb.tmux.list_panes().unwrap();
    assert_eq!(restored_windows.len(), 3);
    assert_eq!(restored_panes.len(), 4);

    // Idempotent second restore skips both existing sessions without duplicating anything.
    let rep2 = sb.tmux.restore_snapshot(&loaded, &opts_real).unwrap();
    assert!(rep2.restored_sessions.is_empty());
    assert_eq!(rep2.skipped_sessions, vec!["spot", "work"]);
    assert_eq!(rep2.restored_windows, 0);
    assert_eq!(rep2.restored_panes, 0);
}

#[test]
fn gate_and_watching_ignore_background_window_and_respect_session_group() {
    // Active client viewing window @1 must NOT block injection into background window @2 of the same session.
    let mut bg = pane("%9");
    bg.window_id = "@2".into();
    bg.window_active = false;
    let mut active_client = client("work", NOW - 1);
    active_client.window_id = "@1".into();

    let r = gate(
        &bg,
        &[active_client.clone()],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
    );
    assert!(r.passed, "background window should not be blocked: {r:?}");
    assert!(!is_user_watching(&bg, &[active_client], NOW, 30));

    // Client in a grouped sibling session viewing window @2 MUST be detected as watching & active.
    bg.session_group = "grp-work".into();
    let grouped_client = ClientInfo {
        tty: "/dev/ttys009".into(),
        session_name: "work-mirror".into(),
        activity_s: NOW - 2,
        flags: "attached,focused".into(),
        session_group: "grp-work".into(),
        window_id: "@2".into(),
    };
    assert!(is_user_watching(
        &bg,
        std::slice::from_ref(&grouped_client),
        NOW,
        30
    ));
    let r_grp = gate(
        &bg,
        &[grouped_client],
        SessionState::Idle,
        AgentKind::Claude,
        "> ",
    );
    assert_eq!(r_grp.failed_checks(), vec![GateCheck::UserInactive]);
}

#[test]
fn parse_extended_pane_and_client_lines() {
    let mut ext_pane = GOOD_PANE.to_vec();
    ext_pane.extend(["grp-main", "1234", "48"]);
    let p = parse_pane_line(&pane_line(&ext_pane)).unwrap();
    assert_eq!(p.session_group, "grp-main");
    assert_eq!(p.history_size, 1234);
    assert_eq!(p.pane_height, 48);

    let ext_client = pane_line(&[
        "/dev/ttys004",
        "work",
        "1700000123",
        "attached,active-pane",
        "grp-main",
        "@7",
    ]);
    let c = parse_client_line(&ext_client).unwrap();
    assert_eq!(c.session_group, "grp-main");
    assert_eq!(c.window_id, "@7");
}

#[test]
#[ignore = "needs tmux; run with `cargo test -p starfane-tmux -- --ignored`"]
fn sandbox_batch_capture_and_paginated_scrollback() {
    use starfane_tmux::CaptureRequest;
    let Some(sb) = Sandbox::new() else {
        return;
    };
    let p1 = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("scroll".into()),
            create_session: true,
            window_name: "w1".into(),
            cwd: sb.dir.clone(),
            env: Vec::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "i=1; while [ $i -le 120 ]; do echo LINE_$i; i=$((i+1)); done; sleep 10".into(),
            ],
        })
        .unwrap();
    let p2 = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("scroll".into()),
            create_session: false,
            window_name: "w2".into(),
            cwd: sb.dir.clone(),
            env: Vec::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo PANE_TWO_MARKER; sleep 10".into(),
            ],
        })
        .unwrap();

    // Poll until LINE_120 appears in p1.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(t) = sb.tmux.capture_pane_tail(&p1, 20) {
            if t.contains("LINE_120") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for LINE_120");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 1. Batched capture captures both panes in one tmux invocation.
    let batch = sb.tmux.capture_panes_tail_batch(&[(&p1, 10), (&p2, 10)]);
    assert!(batch.get(&p1).is_some_and(|t| t.contains("LINE_120")));
    assert!(batch
        .get(&p2)
        .is_some_and(|t| t.contains("PANE_TWO_MARKER")));

    // 2. Tail capture reports has_more + next_before_line, and paging backwards reaches LINE_1 without gaps.
    let tail = sb
        .tmux
        .capture_pane(&p1, &CaptureRequest::tail(40))
        .unwrap();
    assert!(tail.text.contains("LINE_120"));
    assert!(tail.has_more, "expected scrollback history: {tail:?}");
    let cursor = tail.next_before_line.expect("next_before_line");
    let earlier = sb
        .tmux
        .capture_pane(&p1, &CaptureRequest::page_before(cursor, 200))
        .unwrap();
    assert!(earlier.text.contains("LINE_1"), "earlier: {}", earlier.text);
    assert!(!earlier.has_more);
    assert_eq!(earlier.start_line, 0);

    // 3. Full capture (`all = true`) returns from LINE_1 through LINE_120.
    let full = sb.tmux.capture_pane(&p1, &CaptureRequest::full()).unwrap();
    assert!(full.text.contains("LINE_1"));
    assert!(full.text.contains("LINE_120"));
    assert!(!full.has_more);
    assert_eq!(full.start_line, 0);
}

#[test]
fn claude_placeholder_prompt_and_adaptive_settle_and_stuck_detection() {
    // 1. Claude Code 2.1 ghost placeholder hint counts as empty only when cursor sits at prompt start.
    let mut claude_pane = pane("%23");
    claude_pane.cursor_x = 2;
    let r_empty = gate(
        &claude_pane,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "❯\u{a0}Try \"fix lint errors\"",
    );
    assert!(
        r_empty.passed,
        "placeholder should count as empty: {r_empty:?}"
    );

    claude_pane.cursor_x = 22;
    let r_typed = gate(
        &claude_pane,
        &[],
        SessionState::Idle,
        AgentKind::Claude,
        "❯\u{a0}Try \"fix lint errors\"",
    );
    assert_eq!(r_typed.failed_checks(), vec![GateCheck::InputEmpty]);

    // 2. Adaptive paste settle scales up for multiline/long payloads while keeping short lines fast.
    assert_eq!(adaptive_paste_settle_ms(0, "a\nb\nc"), 0);
    assert_eq!(adaptive_paste_settle_ms(250, "hello"), 250);
    let multiline = "第一行说明\n第二行说明\n第三行说明\n第四行很长的追问内容";
    let ml_settle = adaptive_paste_settle_ms(250, multiline);
    assert!(
        (400..=900).contains(&ml_settle),
        "unexpected multiline settle: {ml_settle}"
    );
    let huge = format!("{}\n{}", "x".repeat(2000), "y\n".repeat(20));
    assert_eq!(adaptive_paste_settle_ms(250, &huge), 900);
    assert_eq!(adaptive_paste_settle_ms(1200, &huge), 1200);

    // 3. Stuck-in-prompt detection catches unsubmitted text & paste badges without false positives.
    let pre_agy = "前一条助手回复\n────────────────────\n>\n────────────────────\n? for shortcuts";
    let stuck_agy = "前一条助手回复\n────────────────────\n> 当前接 Sharpa 时的两个注意项：需要改成4踏板\n────────────────────\n? for shortcuts";
    assert!(is_paste_stuck_in_prompt(
        AgentKind::Agy,
        pre_agy,
        stuck_agy,
        2,
        "当前接 Sharpa 时的两个注意项：需要改成4踏板"
    ));

    let pre_claude = "assistant reply\n────────────────────\n❯ \n────────────────────";
    let stuck_badge =
        "assistant reply\n────────────────────\n❯ [Pasted text #1 +4 lines]\n────────────────────";
    assert!(is_paste_stuck_in_prompt(
        AgentKind::Claude,
        pre_claude,
        stuck_badge,
        2,
        "line1\nline2\nline3\nline4"
    ));

    // Submitted message scrolled above divider while cursor sits on clean empty prompt -> NOT stuck.
    let submitted_agy = "> 当前接 Sharpa 时的两个注意项：需要改成4踏板\n────────────────────\n>\n────────────────────\n? for shortcuts";
    assert!(!is_paste_stuck_in_prompt(
        AgentKind::Agy,
        pre_agy,
        submitted_agy,
        2,
        "当前接 Sharpa 时的两个注意项：需要改成4踏板"
    ));

    // Active thinking/streaming state below submitted prompt -> NOT stuck.
    let thinking_agy =
        "> 当前接 Sharpa 时的两个注意项：需要改成4踏板\n⠋ Thinking... (esc to interrupt)";
    assert!(!is_paste_stuck_in_prompt(
        AgentKind::Agy,
        pre_agy,
        thinking_agy,
        1,
        "当前接 Sharpa 时的两个注意项：需要改成4踏板"
    ));

    // Shell panes are never flagged for retry-Enter.
    assert!(!is_paste_stuck_in_prompt(
        AgentKind::Shell,
        pre_agy,
        stuck_agy,
        2,
        "当前接 Sharpa 时的两个注意项：需要改成4踏板"
    ));
}

#[test]
fn exit_copy_mode_recovers_scrolled_tmux_pane() {
    let Some(sb) = Sandbox::new() else {
        return;
    };
    let ready = sb.file("ready_cm");
    let got = sb.file("got_cm");
    let script = format!(
        ": > '{ready}'; IFS= read -r line; printf '%s' \"$line\" > '{got}'; sleep 30",
        ready = ready.display(),
        got = got.display()
    );
    let p = spawn_sh(&sb, "agy-cm", &script, "B-CM");
    wait_exists(&ready, "copy-mode pane ready");

    // Enter tmux copy-mode (reproducing user scrolling up on an alt=0 AGY pane).
    let st = std::process::Command::new(&sb.tmux.bin)
        .args([
            "-L",
            sb.tmux.socket_name.as_deref().unwrap(),
            "copy-mode",
            "-t",
            p.as_str(),
        ])
        .status()
        .expect("tmux copy-mode");
    assert!(st.success());
    let before = sb
        .tmux
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|x| x.pane_id == p)
        .unwrap();
    assert!(before.in_mode, "pane must be in copy-mode");
    assert_eq!(sb.tmux.pane_mode(&p).unwrap(), "copy-mode");

    // Safely cancel copy-mode and inject message without leaking keys into PTY.
    assert!(sb.tmux.exit_copy_mode(&p).unwrap());
    let after = sb
        .tmux
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|x| x.pane_id == p)
        .unwrap();
    assert!(!after.in_mode, "pane must have exited copy-mode");

    let cfg = GateConfig::default();
    sb.tmux
        .inject_interactive(&p, "RECOVERED_AFTER_COPY_MODE", &cfg)
        .unwrap();
    assert_eq!(
        String::from_utf8(wait_for(&got, "delivered line after copy-mode")).unwrap(),
        "RECOVERED_AFTER_COPY_MODE"
    );
}

#[test]
fn kill_pane_validates_strict_pane_id_before_exec() {
    // Use `/usr/bin/false` so any unintended subprocess call would yield `TmuxError::Command`,
    // proving invalid targets fail strictly in `valid_pane` with `TmuxError::InvalidRequest`.
    let tmux = Tmux::new(PathBuf::from("/usr/bin/false"), None);
    for bad in [
        "",
        "%",
        "12",
        "teleop",
        "teleop:3",
        "teleop:3.0",
        "%12a",
        "%12; echo pwn",
        "zj:work/1",
        "local:%12",
    ] {
        let err = tmux.kill_pane(&PaneId::new(bad)).unwrap_err();
        assert!(
            matches!(err, TmuxError::InvalidRequest(_)),
            "expected InvalidRequest for {bad:?}, got {err:?}"
        );
    }
}

#[test]
fn kill_pane_closes_single_pane_window_and_preserves_sibling_split_pane() {
    let Some(sb) = Sandbox::new() else {
        return;
    };

    // Window 0 (`solo`): 1 pane (`p_solo`).
    let p_solo = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("close-test".into()),
            create_session: true,
            window_name: "solo".into(),
            cwd: sb.dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();

    // Window 1 (`split`): 2 split panes (`p_left` + `p_right`).
    let p_left = sb
        .tmux
        .spawn_window(&SpawnWindow {
            target_session: Some("close-test".into()),
            create_session: false,
            window_name: "split".into(),
            cwd: sb.dir.clone(),
            env: vec![],
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        })
        .unwrap();
    let split_out = std::process::Command::new(&sb.tmux.bin)
        .args([
            "-L",
            sb.tmux.socket_name.as_deref().unwrap(),
            "split-window",
            "-h",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            p_left.as_str(),
            "-c",
            &sb.dir.to_string_lossy(),
            "--",
            "/bin/sh",
            "-c",
            "sleep 30",
        ])
        .output()
        .unwrap();
    assert!(split_out.status.success(), "{split_out:?}");
    let p_right = PaneId::new(String::from_utf8_lossy(&split_out.stdout).trim());

    assert_eq!(sb.tmux.list_windows().unwrap().len(), 2);
    assert_eq!(sb.tmux.list_panes().unwrap().len(), 3);

    // 1. Killing `p_solo` closes the 1-pane `solo` window while keeping `split` alive.
    sb.tmux.kill_pane(&p_solo).unwrap();
    let wins_after_solo = sb.tmux.list_windows().unwrap();
    assert_eq!(wins_after_solo.len(), 1);
    assert_eq!(wins_after_solo[0].window_name, "split");
    let panes_after_solo = sb.tmux.list_panes().unwrap();
    assert!(!panes_after_solo.iter().any(|p| p.pane_id == p_solo));
    assert!(panes_after_solo.iter().any(|p| p.pane_id == p_left));
    assert!(panes_after_solo.iter().any(|p| p.pane_id == p_right));

    // 2. Killing an already-gone pane is idempotent.
    sb.tmux.kill_pane(&p_solo).unwrap();

    // 3. Killing `p_left` in the 2-pane split window preserves sibling `p_right`!
    sb.tmux.kill_pane(&p_left).unwrap();
    let panes_after_split = sb.tmux.list_panes().unwrap();
    assert_eq!(panes_after_split.len(), 1);
    assert_eq!(panes_after_split[0].pane_id, p_right);
    assert_eq!(sb.tmux.list_windows().unwrap().len(), 1);
}
