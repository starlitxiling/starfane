//! `starfane` — single binary: hook emitter, daemon, MCP server and human CLI.
//!
//! `starfane emit` is matched before anything else so the hook hot path never
//! builds a tokio runtime or parses clap (contracts §2, §8).

mod cli;
mod logger;

use starfane_proto::AgentKind;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("emit") {
        std::process::exit(emit_main(&args[2..]));
    }
    std::process::exit(cli::run());
}

/// `starfane emit --agent <a> [--event <E>]` — never fails the host agent.
fn emit_main(args: &[String]) -> i32 {
    let mut agent: Option<AgentKind> = None;
    let mut event: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--agent" => agent = it.next().and_then(|v| AgentKind::parse_coding_agent(v)),
            "--event" => event = it.next().cloned(),
            other => {
                if let Some(v) = other.strip_prefix("--agent=") {
                    agent = AgentKind::parse_coding_agent(v);
                } else if let Some(v) = other.strip_prefix("--event=") {
                    event = Some(v.to_string());
                }
            }
        }
    }
    match agent {
        Some(agent) => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            starfane_emit::run_emit(agent, event.as_deref(), stdin.lock(), stdout.lock())
        }
        None => {
            // Unknown agent: we cannot know the reply contract; print `{}`
            // (harmless for claude/codex, required by agy) and report on stderr.
            let mut out = std::io::stdout();
            let _best_effort = out.write_all(b"{}\n");
            eprintln!("starfane emit: missing or unknown --agent (expected claude|codex|agy)");
            0
        }
    }
}
