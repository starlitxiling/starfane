//! Human CLI (contracts §8). Exit codes: 0 ok, 1 error, 2 usage,
//! 3 daemon unavailable, 4 gate rejected.

use clap::{Args, Parser, Subcommand};
use starfane_daemon::client::{ClientError, DaemonClient};
use starfane_daemon::{DaemonConfig, StarfaneConfig};
use starfane_proto::*;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const EXIT_ERR: i32 = 1;
const EXIT_UNAVAILABLE: i32 = 3;
const EXIT_GATE: i32 = 4;

#[derive(Debug, Parser)]
#[command(
    name = "starfane",
    version,
    about = "Interactive multi-agent coordinator for tmux (claude / codex / agy)"
)]
struct Cli {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Daemon socket (default: $STARFANE_SOCKET or ~/.local/state/starfane/starfaned.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Hook hot path (handled before clap; listed for --help only).
    Emit {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        event: Option<String>,
    },
    /// Run the control-plane daemon.
    Daemon(DaemonArgs),
    /// Read-only environment / hook / trust checks.
    Doctor,
    /// Install starfane hooks + MCP registration (dry-run first, backups always).
    Install(InstallArgs),
    /// Remove only starfane-marked hook entries and the MCP registration.
    Uninstall(InstallArgs),
    /// List sessions and pipelines.
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        project: Option<String>,
    },
    /// stdio MCP server (launched by the agent).
    Mcp {
        #[arg(long)]
        agent: String,
    },
    /// Hand off context from one session to another.
    Handoff(HandoffArgs),
    /// Feature / generic pipelines.
    Pipeline {
        #[command(subcommand)]
        cmd: PipelineCmd,
    },
    /// Two-speed debug: `starfane debug "<symptom; bindings>"`, `starfane debug continue|stop <id>`.
    Debug(DebugArgs),
    /// review.v1 records, feedback and precision.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Open the Web console.
    Web {
        #[arg(long)]
        print_url: bool,
        #[arg(long)]
        no_open: bool,
    },
    /// Backup and restore tmux sessions, windows, split layouts, and agent sessions.
    Tmux {
        #[command(subcommand)]
        cmd: TmuxCmd,
    },
}

#[derive(Debug, Subcommand)]
enum TmuxCmd {
    /// Capture a tmux layout snapshot (uses live daemon state if running, else inspects tmux directly).
    Backup {
        /// Write a snapshot even when layout/sessions match the latest saved snapshot.
        #[arg(long)]
        force: bool,
    },
    /// Restore tmux sessions from the latest (or specified) snapshot without touching existing sessions.
    Restore {
        #[arg(long)]
        file: Option<PathBuf>,
        /// Recreate sessions/windows/panes without re-launching `claude` / `codex` resume commands.
        #[arg(long)]
        no_agents: bool,
        /// Show what would be restored without creating any tmux sessions.
        #[arg(long)]
        dry_run: bool,
    },
    /// List saved tmux layout snapshots.
    Backups,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    /// Fork to background (log to <state>/starfaned.log).
    #[arg(long)]
    detach: bool,
    /// Stay in the foreground (default).
    #[arg(long)]
    foreground: bool,
    /// Serve web assets from this directory (dev hot reload).
    #[arg(long)]
    web_dir: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Debug, Args)]
struct InstallArgs {
    #[arg(long = "agent")]
    agents: Vec<String>,
    #[arg(long)]
    all: bool,
    /// user | project
    #[arg(long, default_value = "user")]
    scope: String,
    #[arg(long)]
    dry_run: bool,
    /// agy: also hook side-effecting PreToolUse/PostToolUse (permission heuristic).
    #[arg(long)]
    agy_permission_heuristic: bool,
    #[arg(long)]
    no_mcp: bool,
}

#[derive(Debug, Args)]
struct HandoffArgs {
    #[arg(long)]
    from: String,
    #[arg(long)]
    to: String,
    #[arg(long, default_value = "review")]
    intent: String,
    #[arg(long, default_value = "read_only")]
    mode: String,
    #[arg(short = 'm', long)]
    summary: Option<String>,
    #[arg(long)]
    body_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum PipelineCmd {
    New {
        utterance: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        playbook: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    List {
        #[arg(long)]
        all: bool,
    },
    Show {
        id: String,
    },
    Confirm {
        id: String,
    },
    Approve {
        id: String,
    },
    Reject {
        id: String,
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    Cancel {
        id: String,
    },
}

#[derive(Debug, Args)]
struct DebugArgs {
    /// `"<symptom; bindings>"` or `continue <id>` / `stop <id>`.
    #[arg(required = true, num_args = 1..)]
    words: Vec<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum ReviewCmd {
    Show {
        id: String,
    },
    Feedback {
        review: String,
        finding: String,
        /// valid | false-positive | wontfix
        label: String,
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    Verdict {
        review: String,
        finding: String,
        /// confirmed | needs_human | downgraded | disproved
        verdict: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    Precision {
        #[arg(long)]
        agent: Option<String>,
    },
}

struct Ctx {
    json: bool,
    socket: PathBuf,
}

#[derive(Debug)]
struct CliError {
    code: i32,
    msg: String,
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        let code = match &e {
            ClientError::Unavailable(_) => EXIT_UNAVAILABLE,
            ClientError::Api(a) if a.code == ErrorCode::GateRejected => EXIT_GATE,
            _ => EXIT_ERR,
        };
        CliError {
            code,
            msg: e.to_string(),
        }
    }
}

fn err(msg: impl Into<String>) -> CliError {
    CliError {
        code: EXIT_ERR,
        msg: msg.into(),
    }
}

type R<T> = Result<T, CliError>;

pub fn run() -> i32 {
    let cli = Cli::parse();
    let ctx = Ctx {
        json: cli.json,
        socket: cli.socket.clone().unwrap_or_else(paths::socket_path),
    };
    let result = match cli.command {
        Cmd::Emit { .. } => Ok(()), // unreachable: handled in main before clap
        Cmd::Daemon(a) => daemon(&ctx, a),
        Cmd::Doctor => doctor(&ctx),
        Cmd::Install(a) => install(&ctx, a, false),
        Cmd::Uninstall(a) => install(&ctx, a, true),
        Cmd::Ls { all, project } => ls(&ctx, all, project),
        Cmd::Mcp { agent } => {
            let agent = match AgentKind::parse_coding_agent(&agent) {
                Some(a) => a,
                None => return usage(&format!("unknown agent {agent}")),
            };
            return starfane_daemon::mcp::run_stdio(agent, &ctx.socket);
        }
        Cmd::Handoff(a) => handoff(&ctx, a),
        Cmd::Pipeline { cmd } => pipeline(&ctx, cmd),
        Cmd::Debug(a) => debug(&ctx, a),
        Cmd::Review { cmd } => review(&ctx, cmd),
        Cmd::Web { print_url, no_open } => web(&ctx, print_url, no_open),
        Cmd::Tmux { cmd } => tmux_cmd(&ctx, cmd),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("starfane: {}", e.msg);
            e.code
        }
    }
}

fn usage(msg: &str) -> i32 {
    eprintln!("starfane: {msg}");
    2
}

fn client(ctx: &Ctx) -> R<DaemonClient> {
    Ok(DaemonClient::connect(&ctx.socket, true)?)
}

fn print_json(v: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

fn load_config() -> R<StarfaneConfig> {
    StarfaneConfig::load(&paths::config_path()).map_err(err)
}

fn cwd_root() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn project_root(arg: Option<String>) -> R<String> {
    match arg {
        Some(p) => {
            let path = Path::new(&p);
            if path.is_dir() {
                std::fs::canonicalize(path)
                    .map(|c| c.to_string_lossy().into_owned())
                    .map_err(|e| err(format!("{p}: {e}")))
            } else {
                Ok(p) // a project name; the daemon resolves it
            }
        }
        None => cwd_root().ok_or_else(|| err("not inside a git repository; pass --project")),
    }
}

fn confirm_prompt(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _flush_best_effort = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------------
// daemon / doctor / install
// ---------------------------------------------------------------------------

fn daemon(_ctx: &Ctx, a: DaemonArgs) -> R<()> {
    if a.detach && !a.foreground {
        let state = paths::state_dir();
        std::fs::create_dir_all(&state).map_err(|e| err(format!("{}: {e}", state.display())))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths::log_path())
            .map_err(|e| err(format!("open log: {e}")))?;
        let log2 = log.try_clone().map_err(|e| err(e.to_string()))?;
        let exe = std::env::current_exe().map_err(|e| err(e.to_string()))?;
        let mut cmd = Command::new(exe);
        cmd.arg("daemon").arg("--foreground");
        if let Some(d) = &a.web_dir {
            cmd.arg("--web-dir").arg(d);
        }
        if let Some(p) = a.port {
            cmd.arg("--port").arg(p.to_string());
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(log2);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            extern "C" {
                fn setsid() -> std::ffi::c_int;
            }
            // Start a fresh POSIX session detached from the caller's controlling TTY so closing
            // the launching shell never sends SIGHUP to `starfaned` or its child processes (e.g. `lark-cli`).
            // SAFETY: `setsid()` is async-signal-safe per POSIX.1-2008.
            unsafe {
                cmd.pre_exec(|| {
                    if setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        cmd.spawn().map_err(|e| err(format!("spawn daemon: {e}")))?;
        return Ok(());
    }
    crate::logger::init();
    let mut config = load_config()?;
    if let Some(p) = a.port {
        config.web.port = p;
    }
    let cfg = DaemonConfig {
        state_dir: paths::state_dir(),
        socket: paths::socket_path(),
        config,
        web_dir: a.web_dir,
        foreground: true,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| err(format!("tokio runtime: {e}")))?;
    rt.block_on(starfane_daemon::run(cfg)).map_err(|e| match e {
        starfane_daemon::DaemonError::AlreadyRunning(m) => CliError {
            code: EXIT_ERR,
            msg: format!("daemon already running at {m}"),
        },
        other => err(other.to_string()),
    })
}

fn home() -> R<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| err("$HOME is not set"))
}

fn starfane_bin() -> R<PathBuf> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| err(format!("cannot resolve starfane binary: {e}")))
}

fn doctor(ctx: &Ctx) -> R<()> {
    let env = starfane_adapters::DoctorEnv {
        home: home()?,
        path_var: std::env::var("PATH").unwrap_or_default(),
        starfane_bin: starfane_bin()?,
    };
    let mut findings = starfane_adapters::doctor_checks(&env);
    let reachable = DaemonClient::connect(&ctx.socket, false)
        .and_then(|mut c| c.call(DaemonCommand::Ping))
        .is_ok();
    findings.push(starfane_adapters::DoctorFinding {
        id: "daemon".into(),
        agent: None,
        level: if reachable {
            starfane_adapters::Level::Ok
        } else {
            starfane_adapters::Level::Warn
        },
        message: if reachable {
            format!("daemon reachable at {}", ctx.socket.display())
        } else {
            format!(
                "daemon not running at {} (it starts on the first hook)",
                ctx.socket.display()
            )
        },
        fix_hint: (!reachable).then(|| "starfane daemon --detach".to_string()),
    });
    match load_config() {
        Ok(c) => {
            findings.push(starfane_adapters::DoctorFinding {
                id: "config".into(),
                agent: None,
                level: starfane_adapters::Level::Ok,
                message: format!(
                    "config {} (web {}:{}, easytier {}, tailscale {}, feishu {}, shell_panes {})",
                    paths::config_path().display(),
                    c.web.bind,
                    c.web.port,
                    if c.web.allow_easytier { "on" } else { "off" },
                    if c.web.allow_tailscale { "on" } else { "off" },
                    if c.feishu.enabled { "on" } else { "off" },
                    if c.discovery.include_shell_panes {
                        "on"
                    } else {
                        "off"
                    }
                ),
                fix_hint: None,
            });
            let backups =
                starfane_tmux::list_snapshots(&paths::tmux_backups_dir()).unwrap_or_default();
            findings.push(starfane_adapters::DoctorFinding {
                id: "tmux.backup".into(),
                agent: None,
                level: starfane_adapters::Level::Ok,
                message: format!(
                    "tmux.backup {} (interval {}s, auto_restore {}, snapshots {})",
                    if c.tmux.backup.enabled { "on" } else { "off" },
                    c.tmux.backup.interval_secs,
                    if c.tmux.backup.auto_restore_on_start {
                        "on"
                    } else {
                        "off"
                    },
                    backups.len()
                ),
                fix_hint: (!c.tmux.backup.enabled).then(|| {
                    "enable `[tmux.backup] enabled = true` in config.toml if desired".into()
                }),
            });
        }
        Err(e) => findings.push(starfane_adapters::DoctorFinding {
            id: "config".into(),
            agent: None,
            level: starfane_adapters::Level::Error,
            message: e.msg,
            fix_hint: Some("fix the TOML syntax".into()),
        }),
    }
    if ctx.json {
        print_json(&serde_json::to_value(&findings).unwrap_or_default());
    } else {
        for f in &findings {
            let mark = match f.level {
                starfane_adapters::Level::Ok => "✓",
                starfane_adapters::Level::Warn => "!",
                starfane_adapters::Level::Error => "✗",
            };
            let agent = f.agent.map(|a| format!("[{a}] ")).unwrap_or_default();
            println!("{mark} {agent}{}", f.message);
            if let Some(h) = &f.fix_hint {
                println!("    → {h}");
            }
        }
    }
    if findings
        .iter()
        .any(|f| f.level == starfane_adapters::Level::Error)
    {
        return Err(err("doctor found errors"));
    }
    Ok(())
}

fn line_diff(before: &str, after: &str) -> String {
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    let mut out = String::new();
    for l in &b {
        if !a.contains(l) {
            out.push_str(&format!("-{l}\n"));
        }
    }
    for l in &a {
        if !b.contains(l) {
            out.push_str(&format!("+{l}\n"));
        }
    }
    out
}

fn install(ctx: &Ctx, a: InstallArgs, uninstall: bool) -> R<()> {
    let agents: Vec<AgentKind> = if a.all || a.agents.is_empty() {
        AgentKind::ALL.to_vec()
    } else {
        a.agents
            .iter()
            .map(|s| {
                AgentKind::parse_coding_agent(s).ok_or_else(|| err(format!("unknown agent {s}")))
            })
            .collect::<R<_>>()?
    };
    let scope = match a.scope.as_str() {
        "user" => starfane_adapters::Scope::User,
        "project" => starfane_adapters::Scope::Project(PathBuf::from(
            cwd_root().ok_or_else(|| err("--scope project needs a git repository"))?,
        )),
        other => return Err(err(format!("unknown scope {other} (user|project)"))),
    };
    let home = home()?;
    let bin = starfane_bin()?;
    let mut summary = Vec::new();
    for agent in agents {
        let plan = if uninstall {
            starfane_adapters::uninstall_plan(agent, &home, &scope, &bin)
        } else {
            starfane_adapters::install_plan(
                agent,
                &home,
                &starfane_adapters::InstallOptions {
                    scope: scope.clone(),
                    dry_run: a.dry_run,
                    starfane_bin: bin.clone(),
                    agy_permission_heuristic: a.agy_permission_heuristic,
                    register_mcp: !a.no_mcp,
                },
            )
        }
        .map_err(|e| err(format!("{agent}: {e}")))?;
        if !ctx.json {
            println!("== {agent} ==");
            for fc in &plan.file_changes {
                println!("--- {}", fc.path.display());
                print!(
                    "{}",
                    line_diff(
                        fc.before.as_deref().unwrap_or(""),
                        fc.after.as_deref().unwrap_or("")
                    )
                );
            }
            for c in &plan.commands {
                println!("$ {}", c.join(" "));
            }
            for n in &plan.notes {
                println!("note: {n}");
            }
        }
        let backup = if a.dry_run {
            None
        } else {
            Some(
                starfane_adapters::apply_plan(&plan, &paths::backups_dir())
                    .map_err(|e| err(format!("{agent}: {e}")))?,
            )
        };
        summary.push(serde_json::json!({"agent": agent, "plan": plan, "backup": backup}));
    }
    if ctx.json {
        print_json(&serde_json::Value::Array(summary));
    } else if a.dry_run {
        println!("(dry run — nothing written)");
    } else {
        println!("done; backups under {}", paths::backups_dir().display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ls / handoff
// ---------------------------------------------------------------------------

fn format_compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}K", (n as f64) / 1_000.0)
    } else {
        n.to_string()
    }
}

fn ls(ctx: &Ctx, all: bool, project: Option<String>) -> R<()> {
    let mut c = client(ctx)?;
    let sessions: Vec<SessionRecord> = c.call_as(DaemonCommand::ListSessions {
        project: project.clone(),
        include_subagents: all,
        include_ended: all,
    })?;
    let pipelines: Vec<PipelineSpec> = c.call_as(DaemonCommand::ListPipelines {
        project,
        include_done: all,
    })?;
    if ctx.json {
        print_json(&serde_json::json!({"sessions": sessions, "pipelines": pipelines}));
        return Ok(());
    }
    println!(
        "{:<8} {:<18} {:<7} {:<16} {:<16} {:<22} {:<7} {:<8} STAGE",
        "PANE", "LABEL", "AGENT", "STATE", "PROJECT", "MODEL", "CTX", "TPS"
    );
    for s in &sessions {
        let tag = if s.is_subagent {
            " (sub)"
        } else if s.discovered_only {
            " (tmux)"
        } else {
            ""
        };
        let model_col = match (
            s.model.as_deref().map(str::trim).filter(|v| !v.is_empty()),
            s.effort.as_deref().map(str::trim).filter(|v| !v.is_empty()),
        ) {
            (Some(m), Some(e)) => format!("{m} ({e})"),
            (Some(m), None) => m.to_string(),
            (None, Some(e)) => format!("- ({e})"),
            (None, None) => "-".to_string(),
        };
        let ctx_col = s
            .metrics
            .as_ref()
            .and_then(|m| {
                if let Some(pct) = m.context_used_pct {
                    Some(format!("{pct:.0}%"))
                } else {
                    m.context_used_tokens.map(format_compact_tokens)
                }
            })
            .unwrap_or_else(|| "-".into());
        let tps_col = s
            .metrics
            .as_ref()
            .and_then(|m| m.tps)
            .map(|v| format!("{v:.1}/s"))
            .unwrap_or_else(|| "-".into());
        let label = s
            .tmux_label
            .as_deref()
            .or(s.tmux_target.as_deref())
            .unwrap_or("-");
        println!(
            "{:<8} {:<18} {:<7} {:<16} {:<16} {:<22} {:<7} {:<8} {}{}",
            s.pane.as_ref().map(|p| p.as_str()).unwrap_or("-"),
            label,
            s.agent.as_str(),
            format!("{:?}", s.state),
            s.git_root
                .as_deref()
                .or(s.cwd.as_deref())
                .and_then(|r| Path::new(r).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "-".into()),
            model_col,
            ctx_col,
            tps_col,
            s.stage
                .as_ref()
                .map(|r| format!("{}/{}", r.pipeline_id, r.stage))
                .unwrap_or_default(),
            tag,
        );
    }
    if !pipelines.is_empty() {
        println!();
        for p in &pipelines {
            let stages: Vec<String> = p
                .stages
                .iter()
                .map(|s| format!("{}:{:?}", s.id, s.status))
                .collect();
            println!(
                "{} [{:?}] {} — {}",
                p.id,
                p.status,
                p.goal,
                stages.join(" → ")
            );
        }
    }
    Ok(())
}

/// `%12` | `s:<id>` | `P-…/<stage>` | agent name (unique in the current project).
fn resolve_selector(c: &mut DaemonClient, sel: &str) -> R<HandoffTarget> {
    if sel.starts_with('%') {
        return Ok(HandoffTarget {
            pane: Some(PaneId(sel.to_string())),
            ..Default::default()
        });
    }
    if let Some(id) = sel.strip_prefix("s:") {
        return Ok(HandoffTarget {
            session_id: Some(SessionId(id.to_string())),
            ..Default::default()
        });
    }
    if let Some((pid, stage)) = sel.split_once('/') {
        let stage = StageKind::parse(stage).ok_or_else(|| err(format!("unknown stage {stage}")))?;
        return Ok(HandoffTarget {
            stage: Some(StageRef {
                pipeline_id: PipelineId(pid.to_string()),
                stage,
            }),
            ..Default::default()
        });
    }
    let agent = AgentKind::parse(sel).ok_or_else(|| err(format!("bad selector {sel}")))?;
    let root = cwd_root();
    let sessions: Vec<SessionRecord> = c.call_as(DaemonCommand::ListSessions {
        project: root,
        include_subagents: false,
        include_ended: false,
    })?;
    let live: Vec<&SessionRecord> = sessions
        .iter()
        .filter(|s| s.agent == agent && s.state != SessionState::Ended)
        .collect();
    match live.len() {
        1 => Ok(HandoffTarget {
            session_id: Some(live[0].session_id.clone()),
            ..Default::default()
        }),
        0 => Err(err(format!("no live {agent} session in this project"))),
        _ => Err(err(format!(
            "several {agent} sessions; use %pane or s:<id>"
        ))),
    }
}

fn session_of(c: &mut DaemonClient, t: &HandoffTarget) -> R<SessionId> {
    if let Some(s) = &t.session_id {
        return Ok(s.clone());
    }
    let sessions: Vec<SessionRecord> = c.call_as(DaemonCommand::ListSessions {
        project: None,
        include_subagents: false,
        include_ended: false,
    })?;
    if let Some(p) = &t.pane {
        return sessions
            .iter()
            .filter(|s| s.pane.as_ref() == Some(p) && s.state != SessionState::Ended)
            .max_by_key(|s| s.last_event_ms)
            .map(|s| s.session_id.clone())
            .ok_or_else(|| err(format!("no session in pane {p}")));
    }
    if let Some(r) = &t.stage {
        let p: PipelineSpec = c.call_as(DaemonCommand::GetPipeline {
            pipeline_id: r.pipeline_id.clone(),
        })?;
        return p
            .stage(r.stage)
            .and_then(|s| s.session_id.clone())
            .ok_or_else(|| err(format!("stage {} has no session", r.stage)));
    }
    Err(err("cannot resolve the source session"))
}

fn handoff(ctx: &Ctx, a: HandoffArgs) -> R<()> {
    let mut c = client(ctx)?;
    let from_t = resolve_selector(&mut c, &a.from)?;
    let from = session_of(&mut c, &from_t)?;
    let to = resolve_selector(&mut c, &a.to)?;
    let intent: HandoffIntent = serde_json::from_value(serde_json::json!(a.intent))
        .map_err(|_| err("intent: review|implement|continue|ask|fyi"))?;
    let mode: AccessMode = serde_json::from_value(serde_json::json!(a.mode.replace('-', "_")))
        .map_err(|_| err("mode: read_only|write"))?;
    let body = match &a.body_file {
        Some(f) => std::fs::read_to_string(f).map_err(|e| err(format!("{}: {e}", f.display())))?,
        None => {
            let out = Command::new("git")
                .args(["diff", "--stat", "HEAD"])
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    format!("```\n{}\n```", String::from_utf8_lossy(&o.stdout).trim())
                }
                _ => "(no body)".into(),
            }
        }
    };
    let summary = a
        .summary
        .unwrap_or_else(|| format!("handoff from {}", a.from));
    let v = c.call(DaemonCommand::CreateHandoff {
        draft: HandoffDraft {
            from_session: from,
            to,
            intent,
            mode,
            summary,
            body_md: body,
            todo: Vec::new(),
            refs: Vec::new(),
            self_test: None,
        },
    })?;
    if ctx.json {
        print_json(&v);
    } else {
        println!(
            "handoff {} → {}",
            v["handoff_id"].as_str().unwrap_or("?"),
            v["delivery"]["status"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pipelines / debug / review
// ---------------------------------------------------------------------------

fn print_parsed(parsed: &serde_json::Value) {
    println!("playbook: {}", parsed["playbook"].as_str().unwrap_or("?"));
    if let Some(g) = parsed["goal"].as_str() {
        println!("goal:     {g}");
    }
    if let Some(map) = parsed["resolved"].as_object() {
        for (stage, b) in map {
            let shorthand = serde_json::from_value::<StageBinding>(b.clone())
                .map(|b| b.to_shorthand())
                .unwrap_or_else(|_| b.to_string());
            let explicit = parsed["bindings"].get(stage).is_some();
            println!(
                "  {stage:<12} → {shorthand}{}",
                if explicit { "" } else { "  (default)" }
            );
        }
    }
    if let Some(ws) = parsed["warnings"].as_array() {
        for w in ws {
            println!("  ⚠ {w}");
        }
    }
}

fn new_pipeline(
    ctx: &Ctx,
    text: &str,
    project: Option<String>,
    playbook: Option<PlaybookKind>,
    yes: bool,
) -> R<()> {
    let root = project_root(project)?;
    let mut c = client(ctx)?;
    let parsed = c.call(DaemonCommand::ParseUtterance {
        text: text.to_string(),
        playbook,
        project: Some(root.clone()),
    })?;
    let playbook: PlaybookKind = serde_json::from_value(parsed["playbook"].clone())
        .map_err(|e| err(format!("bad parse result: {e}")))?;
    let bindings: BTreeMap<StageKind, StageBinding> =
        serde_json::from_value(parsed["bindings"].clone()).unwrap_or_default();
    let goal = parsed["goal"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| text.to_string());
    if !ctx.json {
        print_parsed(&parsed);
    }
    let spec: PipelineSpec = c.call_as(DaemonCommand::CreatePipeline {
        req: CreatePipelineReq {
            playbook,
            project_root: root,
            goal,
            bindings,
            options: None,
            source: PipelineSource {
                via: Via::Cli,
                utterance: Some(text.to_string()),
                operator: std::env::var("USER").ok(),
            },
        },
    })?;
    if !yes && !confirm_prompt(&format!("start pipeline {}?", spec.id)) {
        println!(
            "left awaiting confirmation: starfane pipeline confirm {}",
            spec.id
        );
        return Ok(());
    }
    let v = c.call(DaemonCommand::ConfirmPipeline {
        pipeline_id: spec.id.clone(),
        overrides: None,
    })?;
    if ctx.json {
        print_json(&v);
    } else {
        println!(
            "started {} ({})",
            spec.id,
            v["status"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

fn pipeline_cmd(ctx: &Ctx, id: &str, action: PipelineAction) -> R<()> {
    let v = client(ctx)?.call(DaemonCommand::PipelineAction {
        pipeline_id: PipelineId(id.to_string()),
        action,
    })?;
    if ctx.json {
        print_json(&v);
    } else {
        println!("{} → {}", id, v["status"].as_str().unwrap_or("?"));
    }
    Ok(())
}

fn pipeline(ctx: &Ctx, cmd: PipelineCmd) -> R<()> {
    match cmd {
        PipelineCmd::New {
            utterance,
            project,
            playbook,
            yes,
        } => {
            let pb = match playbook {
                Some(p) => {
                    Some(PlaybookKind::parse(&p).ok_or_else(|| err("playbook: feature|debug"))?)
                }
                None => None,
            };
            new_pipeline(ctx, &utterance, project, pb, yes)
        }
        PipelineCmd::List { all } => {
            let v = client(ctx)?.call(DaemonCommand::ListPipelines {
                project: None,
                include_done: all,
            })?;
            print_json(&v);
            Ok(())
        }
        PipelineCmd::Show { id } => {
            let v = client(ctx)?.call(DaemonCommand::GetPipeline {
                pipeline_id: PipelineId(id),
            })?;
            print_json(&v);
            Ok(())
        }
        PipelineCmd::Confirm { id } => {
            let v = client(ctx)?.call(DaemonCommand::ConfirmPipeline {
                pipeline_id: PipelineId(id.clone()),
                overrides: None,
            })?;
            if ctx.json {
                print_json(&v);
            } else {
                println!("{id} → {}", v["status"].as_str().unwrap_or("?"));
            }
            Ok(())
        }
        PipelineCmd::Approve { id } => pipeline_cmd(ctx, &id, PipelineAction::ApprovePlan),
        PipelineCmd::Reject { id, message } => pipeline_cmd(
            ctx,
            &id,
            match message {
                Some(note) => PipelineAction::RequestChanges { note },
                None => PipelineAction::Cancel,
            },
        ),
        PipelineCmd::Cancel { id } => pipeline_cmd(ctx, &id, PipelineAction::Cancel),
    }
}

fn debug(ctx: &Ctx, a: DebugArgs) -> R<()> {
    match a.words.as_slice() {
        [verb, id] if verb == "continue" => pipeline_cmd(ctx, id, PipelineAction::ContinueDeep),
        [verb, id] if verb == "stop" => pipeline_cmd(ctx, id, PipelineAction::StopDeep),
        words => {
            let text = words.join(" ");
            new_pipeline(ctx, &text, a.project, Some(PlaybookKind::Debug), a.yes)
        }
    }
}

fn review(ctx: &Ctx, cmd: ReviewCmd) -> R<()> {
    let mut c = client(ctx)?;
    let v = match cmd {
        ReviewCmd::Show { id } => c.call(DaemonCommand::GetReview {
            review_id: ReviewId(id),
        })?,
        ReviewCmd::Feedback {
            review,
            finding,
            label,
            message,
        } => {
            let label: FeedbackLabel =
                serde_json::from_value(serde_json::json!(label.replace('-', "_")))
                    .map_err(|_| err("label: valid|false-positive|wontfix"))?;
            c.call(DaemonCommand::ReviewFeedback {
                review_id: ReviewId(review),
                finding_id: FindingId(finding),
                label,
                note: message,
                by: std::env::var("USER").unwrap_or_else(|_| "cli".into()),
            })?
        }
        ReviewCmd::Verdict {
            review,
            finding,
            verdict,
            to,
            message,
        } => {
            let mut j = serde_json::json!({"verdict": verdict.replace('-', "_")});
            if let Some(t) = to {
                j["to"] = serde_json::json!(t);
            }
            let verdict: FalsifierVerdict =
                serde_json::from_value(j).map_err(|e| err(format!("verdict: {e}")))?;
            c.call(DaemonCommand::HumanVerdict {
                review_id: ReviewId(review),
                finding_id: FindingId(finding),
                verdict,
                note: message,
                by: std::env::var("USER").unwrap_or_else(|_| "cli".into()),
            })?
        }
        ReviewCmd::Precision { agent } => {
            let agent = match agent {
                Some(a) => Some(
                    AgentKind::parse_coding_agent(&a)
                        .ok_or_else(|| err(format!("unknown agent {a}")))?,
                ),
                None => None,
            };
            c.call(DaemonCommand::GetPrecision { agent, model: None })?
        }
    };
    print_json(&v);
    Ok(())
}

fn extract_matching_ipv4(
    out_text: &str,
    mut pred: impl FnMut(std::net::Ipv4Addr) -> bool,
) -> Option<String> {
    for line in out_text.lines() {
        let Some((_, after_inet)) = line.split_once("inet ") else {
            continue;
        };
        let Some(token) = after_inet.split_whitespace().next() else {
            continue;
        };
        let ip_str = token.split('/').next().unwrap_or(token);
        if let Ok(v4) = ip_str.parse::<std::net::Ipv4Addr>() {
            if !v4.is_loopback() && pred(v4) {
                return Some(ip_str.to_string());
            }
        }
    }
    None
}

fn detect_easytier_ipv4(cidrs: &[String]) -> Option<String> {
    for (bin, args) in [
        ("ifconfig", &[][..]),
        ("ip", &["-4", "-o", "addr", "show"][..]),
    ] {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(ip) = extract_matching_ipv4(&s, |v4| {
                    starfane_daemon::config::ipv4_in_cidrs(v4, cidrs)
                }) {
                    return Some(ip);
                }
            }
        }
    }
    None
}

fn detect_tailscale_ipv4() -> Option<String> {
    if let Ok(out) = Command::new("tailscale").args(["ip", "-4"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(ip) = s.lines().map(str::trim).find(|l| !l.is_empty()) {
                return Some(ip.to_string());
            }
        }
    }
    if let Ok(out) = Command::new("ifconfig").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(ip) = extract_matching_ipv4(&s, |v4| {
                let o = v4.octets();
                o[0] == 100 && (o[1] & 0b1100_0000) == 64
            }) {
                return Some(ip);
            }
        }
    }
    None
}

fn web(ctx: &Ctx, print_url: bool, no_open: bool) -> R<()> {
    // Make sure the daemon (and therefore the token) exists.
    client(ctx)?.call(DaemonCommand::Ping)?;
    let cfg = load_config()?;
    let url = if !cfg.web.auth_enabled {
        let host = if cfg.web.bind == "0.0.0.0" {
            let et = cfg
                .web
                .allow_easytier
                .then(|| detect_easytier_ipv4(&cfg.web.easytier_cidrs))
                .flatten();
            let ts = cfg
                .web
                .allow_tailscale
                .then(detect_tailscale_ipv4)
                .flatten();
            et.or(ts).unwrap_or_else(|| "127.0.0.1".into())
        } else {
            cfg.web.bind.clone()
        };
        format!("http://{host}:{}/", cfg.web.port)
    } else {
        let token = std::fs::read_to_string(paths::web_token_path())
            .map_err(|e| err(format!("read {}: {e}", paths::web_token_path().display())))?;
        format!(
            "http://127.0.0.1:{}/auth?token={}",
            cfg.web.port,
            token.trim()
        )
    };
    if print_url || no_open {
        println!("{url}");
    }
    if !no_open {
        Command::new("open")
            .arg(&url)
            .status()
            .map_err(|e| err(format!("open browser: {e}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tmux backup / restore (daemon-preferred, offline-capable)
// ---------------------------------------------------------------------------

fn offline_tmux(cfg: &StarfaneConfig) -> starfane_tmux::Tmux {
    starfane_tmux::Tmux::new(PathBuf::from(cfg.tmux_bin()), cfg.tmux_socket())
}

fn tmux_cmd(ctx: &Ctx, cmd: TmuxCmd) -> R<()> {
    match cmd {
        TmuxCmd::Backup { force } => {
            if let Ok(mut c) = DaemonClient::connect(&ctx.socket, false) {
                let v = c.call(DaemonCommand::TmuxBackup { force })?;
                if ctx.json {
                    print_json(&v);
                } else {
                    let out: starfane_tmux::BackupSaveOutcome = serde_json::from_value(v)
                        .map_err(|e| err(format!("parse backup response: {e}")))?;
                    print_backup_outcome(&out);
                }
                return Ok(());
            }
            let cfg = load_config()?;
            let tmux = offline_tmux(&cfg);
            let windows = tmux.list_windows().map_err(|e| err(e.to_string()))?;
            let panes = tmux.list_panes().map_err(|e| err(e.to_string()))?;
            let clients = tmux.list_clients().map_err(|e| err(e.to_string()))?;
            let discovered = tmux.discover_panes_from(
                &panes,
                starfane_tmux::DiscoverOptions {
                    include_shell_panes: false,
                },
            );
            let mut hints = std::collections::HashMap::new();
            for dp in discovered {
                if !dp.pane_dead && !dp.agent.is_shell() {
                    hints.insert(
                        dp.pane_id,
                        starfane_tmux::PaneAgentHint {
                            agent: dp.agent,
                            resume_id: dp.codex_resume_id,
                            model: dp.model,
                        },
                    );
                }
            }
            let snap = starfane_tmux::build_snapshot(
                &windows,
                &panes,
                &clients,
                &hints,
                &cfg.tmux.backup.exclude_sessions,
                now_ms_cli(),
            );
            let dir = paths::tmux_backups_dir();
            let out = starfane_tmux::save_snapshot_to_dir(
                &dir,
                &snap,
                cfg.tmux.backup.max_snapshots,
                force,
            )
            .map_err(|e| err(format!("save snapshot: {e}")))?;
            if ctx.json {
                print_json(&serde_json::to_value(&out).unwrap_or_default());
            } else {
                print_backup_outcome(&out);
            }
            Ok(())
        }
        TmuxCmd::Restore {
            file,
            no_agents,
            dry_run,
        } => {
            let restore_agents_override = no_agents.then_some(false);
            if !no_agents && !dry_run {
                if let Ok(mut c) = DaemonClient::connect(&ctx.socket, false) {
                    let v = c.call(DaemonCommand::TmuxRestore {
                        file: file.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        restore_agents: restore_agents_override,
                        dry_run,
                    })?;
                    if ctx.json {
                        print_json(&v);
                    } else {
                        let rep: starfane_tmux::RestoreReport = serde_json::from_value(v)
                            .map_err(|e| err(format!("parse restore response: {e}")))?;
                        print_restore_report(&rep);
                    }
                    return Ok(());
                }
            }
            let cfg = load_config()?;
            let dir = paths::tmux_backups_dir();
            let (snap_path, snap) = match file {
                Some(p) => {
                    let resolved = if p.is_absolute() || p.components().count() > 1 {
                        p
                    } else {
                        dir.join(p)
                    };
                    let s = starfane_tmux::load_snapshot_file(&resolved)
                        .map_err(|e| err(format!("load {}: {e}", resolved.display())))?;
                    (resolved, s)
                }
                None => starfane_tmux::load_latest_snapshot(&dir)
                    .map_err(|e| err(format!("load latest snapshot: {e}")))?
                    .ok_or_else(|| {
                        err(format!(
                            "no tmux backup snapshot found under {}",
                            dir.display()
                        ))
                    })?,
            };
            let tmux = offline_tmux(&cfg);
            let opts = starfane_tmux::RestoreOptions {
                restore_agents: restore_agents_override.unwrap_or(cfg.tmux.backup.restore_agents),
                restore_bypass_permissions: cfg.tmux.backup.restore_bypass_permissions,
                dry_run,
                claude_bin: cfg.agent_bin(AgentKind::Claude),
                codex_bin: cfg.agent_bin(AgentKind::Codex),
            };
            let mut rep = tmux
                .restore_snapshot(&snap, &opts)
                .map_err(|e| err(format!("restore tmux: {e}")))?;
            rep.snapshot_path = Some(snap_path);
            if ctx.json {
                print_json(&serde_json::to_value(&rep).unwrap_or_default());
            } else {
                print_restore_report(&rep);
            }
            Ok(())
        }
        TmuxCmd::Backups => {
            let dir = paths::tmux_backups_dir();
            let list = starfane_tmux::list_snapshots(&dir)
                .map_err(|e| err(format!("list snapshots: {e}")))?;
            if ctx.json {
                print_json(&serde_json::to_value(&list).unwrap_or_default());
                return Ok(());
            }
            if list.is_empty() {
                println!("no tmux snapshots under {}", dir.display());
                return Ok(());
            }
            println!(
                "{:<28} {:<15} {:<8} {:<8} {:<6} RESUMABLE",
                "FILE", "CREATED_MS", "SESS", "WIN", "PANE"
            );
            for item in list {
                println!(
                    "{:<28} {:<15} {:<8} {:<8} {:<6} {}",
                    item.file_name,
                    item.created_ms,
                    item.session_count,
                    item.window_count,
                    item.pane_count,
                    item.resumable_agent_count
                );
            }
            Ok(())
        }
    }
}

fn now_ms_cli() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn print_backup_outcome(out: &starfane_tmux::BackupSaveOutcome) {
    if out.saved {
        println!(
            "saved snapshot {} (sessions={}, windows={}, panes={}, resumable_agents={})",
            out.path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| out.latest_path.display().to_string()),
            out.session_count,
            out.window_count,
            out.pane_count,
            out.resumable_agent_count
        );
    } else {
        println!(
            "skipped snapshot ({}): sessions={}, windows={}, panes={}, resumable_agents={}",
            out.skipped_reason.as_deref().unwrap_or("skipped"),
            out.session_count,
            out.window_count,
            out.pane_count,
            out.resumable_agent_count
        );
    }
}

fn print_restore_report(rep: &starfane_tmux::RestoreReport) {
    println!(
        "{}restore from {}:",
        if rep.dry_run { "[dry-run] " } else { "" },
        rep.snapshot_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "latest.json".into())
    );
    println!(
        "  restored_sessions: {}",
        if rep.restored_sessions.is_empty() {
            "-".into()
        } else {
            rep.restored_sessions.join(", ")
        }
    );
    if !rep.skipped_sessions.is_empty() {
        println!("  skipped_sessions:  {}", rep.skipped_sessions.join(", "));
    }
    println!(
        "  windows={}, panes={}, resumed_agents={}",
        rep.restored_windows,
        rep.restored_panes,
        rep.resumed_agents.len()
    );
    for a in &rep.resumed_agents {
        println!(
            "    - {}:{} ({}) -> {}",
            a.session_name, a.window_name, a.agent, a.command
        );
    }
    for w in &rep.warnings {
        println!("  ⚠ {w}");
    }
}
