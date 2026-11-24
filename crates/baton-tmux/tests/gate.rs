//! Gate / resolution / focus matrix tests (pure) + a sandboxed real-tmux integration test.
//!
//! The integration test (`sandbox_*`) is `#[ignore]`d per contracts §10; run it with
//! `cargo test -p baton-tmux -- --ignored`. It uses its own `tmux -L baton-test-gate-<pid>`
//! server (never the user's) and kills it on drop.

use baton_proto::{AgentKind, GateCheck, PaneId, PaneResolution, SessionState, StageKind};
use baton_tmux::{
    check_injection_gate, is_bare_slash_command, is_user_watching, parse_client_line,
    parse_clients, parse_pane_line, parse_panes, pid_ancestors, resolve_pane,
    validate_pointer_text, ClientInfo, GateConfig, PaneHint, PaneInfo, PaneStatus, SpawnWindow,
    Tmux, TmuxError, FIELD_SEP,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    }
}

fn client(session: &str, activity: u64) -> ClientInfo {
    ClientInfo {
        tty: "/dev/ttys001".into(),
        session_name: session.into(),
        activity_s: activity,
        flags: "attached,focused".into(),
    }
}

fn gate(
    p: &PaneInfo,
    clients: &[ClientInfo],
    state: SessionState,
    agent: AgentKind,
    line: &str,
) -> baton_proto::GateReport {
    check_injection_gate(p, clients, state, agent, line, &GateConfig::default(), NOW)
}

fn ok_gate(p: &PaneInfo) -> baton_proto::GateReport {
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
        "/swarm baton: new task T-1, call baton.read_inbox",
        "baton: /swarm",
        "baton: check inbox",
        "/path/to/file is broken",
    ] {
        assert!(!is_bare_slash_command(t), "{t:?}");
    }
}

#[test]
fn pointer_text_validation() {
    let cfg = GateConfig::default();
    assert!(
        validate_pointer_text("baton: new task P-1, please call baton.read_inbox", &cfg).is_ok()
    );
    assert!(validate_pointer_text("/swarm baton: task P-1", &cfg).is_ok());
    assert!(validate_pointer_text("baton: 新任务 P-1，请调用 baton.read_inbox", &cfg).is_ok());

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
    format!("baton-test-{tag}-{}", std::process::id())
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
        t.inject_pointer(&PaneId::new("bogus"), "baton: hi", &cfg),
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
        let tmux = Tmux::new(PathBuf::from("tmux"), Some(unique_socket("gate")));
        if !tmux.available() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!("baton-tmux-gate-{}", std::process::id()));
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
            env: vec![("BATON_BIND".into(), bind.into())],
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        })
        .expect("spawn_window")
}

#[test]
#[ignore = "needs tmux; run with `cargo test -p baton-tmux -- --ignored`"]
fn sandbox_spawn_list_inject_status() {
    let Some(sb) = Sandbox::new() else {
        panic!("tmux is required for this ignored test");
    };
    let ready = sb.file("ready_line");
    let got_line = sb.file("got_line");
    let bind_out = sb.file("bind_out");

    // Pane A: cooked tty, prints the line it receives after a paste + Enter.
    let script_a = format!(
        "printf '%s' \"$BATON_BIND\" > '{bind}'; : > '{ready}'; IFS= read -r line; printf '%s' \"$line\" > '{got}'; sleep 30",
        bind = bind_out.display(),
        ready = ready.display(),
        got = got_line.display()
    );
    let pane_a = spawn_sh(&sb, "agent-a", &script_a, "B-TESTTOKEN");
    wait_exists(&ready, "pane A ready");

    // BATON_BIND reached the child; default detached session "agent-session" was created directly on window 0.
    assert_eq!(wait_for(&bind_out, "BATON_BIND"), b"B-TESTTOKEN");
    let panes = sb.tmux.list_panes().expect("list_panes");
    let a = panes
        .iter()
        .find(|p| p.pane_id == pane_a)
        .expect("pane A listed");
    assert_eq!(a.session_name, baton_tmux::DEFAULT_DETACHED_SESSION);
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
    let text = "baton: 新任务 P-1，请调用 baton.read_inbox ($HOME `x` 'q' \"d\")";
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
    let text_b = "baton: raw check /swarm P-2";
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
    assert_eq!(show("@baton_state"), "needs_permission");
    assert_eq!(show("@agent_state"), "needs_permission");
    assert_eq!(show("@baton_role"), "implement");
    assert_eq!(show("@baton_pipeline"), "P-ABC");
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
    assert_eq!(show("@baton_state"), "idle");
    assert_eq!(show("@baton_role"), "");
    sb.tmux.clear_status(&pane_a).unwrap();
    assert_eq!(show("@baton_state"), "");
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
            baton_tmux::DEFAULT_DETACHED_SESSION.to_string(),
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
            target_session: Some(baton_tmux::DEFAULT_DETACHED_SESSION.into()),
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
