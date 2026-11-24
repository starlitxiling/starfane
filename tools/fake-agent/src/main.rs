//! Sandbox fake interactive agent for isolated `tmux -L starfane-test` E2E tests (contracts §9).
//!
//! Usage:
//!   `fake-agent --agent <claude|codex|agy> --script <file.toml> [--emit-bin <path>] [--session-id <id>]`

use serde::Deserialize;
use serde_json::Value;
use starfane_proto::{id::new_id, AgentKind};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Default)]
struct ScriptMeta {
    session_id: Option<String>,
    cwd: Option<String>,
    idle_prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ScriptStep {
    /// Trigger: `"start"`, `"input"`, or `"after_ms:<n>"`.
    on: String,
    /// Hook event name passed to `starfane emit --event <event>`.
    event: String,
    /// Fixture path (relative to script dir, current dir, or absolute).
    payload: String,
    /// Optional top-level JSON field overrides applied after template substitution.
    #[serde(default)]
    set: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeScript {
    #[serde(default)]
    meta: ScriptMeta,
    #[serde(default)]
    step: Vec<ScriptStep>,
}

#[derive(Debug)]
struct CliArgs {
    agent: AgentKind,
    script: PathBuf,
    emit_bin: Option<PathBuf>,
    session_id: Option<String>,
}

fn default_idle_prompt(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude | AgentKind::Agy => "> ",
        AgentKind::Codex => "› ",
        AgentKind::Shell => "$ ",
    }
}

fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Result<CliArgs, String> {
    let mut args = iter.into_iter().skip(1);
    let mut agent: Option<AgentKind> = None;
    let mut script: Option<PathBuf> = None;
    let mut emit_bin: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent" => {
                let v = args
                    .next()
                    .ok_or_else(|| "missing value for --agent".to_string())?;
                agent =
                    Some(AgentKind::parse(&v).ok_or_else(|| {
                        format!("unknown --agent `{v}` (expected claude|codex|agy)")
                    })?);
            }
            "--script" => {
                let v = args
                    .next()
                    .ok_or_else(|| "missing value for --script".to_string())?;
                script = Some(PathBuf::from(v));
            }
            "--emit-bin" => {
                let v = args
                    .next()
                    .ok_or_else(|| "missing value for --emit-bin".to_string())?;
                emit_bin = Some(PathBuf::from(v));
            }
            "--session-id" => {
                let v = args
                    .next()
                    .ok_or_else(|| "missing value for --session-id".to_string())?;
                session_id = Some(v);
            }
            "-h" | "--help" => {
                return Err(
                    "Usage: fake-agent --agent <claude|codex|agy> --script <file.toml> [--emit-bin <path>] [--session-id <id>]"
                        .to_string(),
                );
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    Ok(CliArgs {
        agent: agent.ok_or_else(|| "missing required flag --agent".to_string())?,
        script: script.ok_or_else(|| "missing required flag --script".to_string())?,
        emit_bin,
        session_id,
    })
}

fn resolve_payload_path(script_path: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() || p.exists() {
        return p.to_path_buf();
    }
    if let Some(parent) = script_path.parent() {
        let candidate = parent.join(p);
        if candidate.exists() {
            return candidate;
        }
    }
    p.to_path_buf()
}

fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(tbl) => {
            let mut m = serde_json::Map::new();
            for (k, val) in tbl {
                m.insert(k.clone(), toml_to_json(val));
            }
            Value::Object(m)
        }
    }
}

fn render_payload(
    raw: &str,
    agent: AgentKind,
    session_id: &str,
    cwd: &str,
    input: Option<&str>,
    overrides: &BTreeMap<String, toml::Value>,
) -> Result<Value, String> {
    let mut rendered = raw
        .replace("{{session_id}}", session_id)
        .replace("{{cwd}}", cwd);
    if let Some(inp) = input {
        rendered = rendered.replace("{{input}}", inp);
    }

    let mut val: Value =
        serde_json::from_str(&rendered).map_err(|e| format!("invalid fixture JSON: {e}"))?;

    if let Some(obj) = val.as_object_mut() {
        // Always stamp session_id / conversationId and cwd so sandbox runs isolate cleanly.
        match agent {
            AgentKind::Claude | AgentKind::Codex => {
                obj.insert("session_id".into(), Value::String(session_id.to_string()));
                if let Some(inp) = input {
                    if obj.contains_key("prompt") {
                        obj.insert("prompt".into(), Value::String(inp.to_string()));
                    }
                }
            }
            AgentKind::Agy => {
                obj.insert(
                    "conversationId".into(),
                    Value::String(session_id.to_string()),
                );
                if let Some(inp) = input {
                    obj.insert("lastUserInput".into(), Value::String(inp.to_string()));
                }
            }
            AgentKind::Shell => {}
        }
        obj.insert("cwd".into(), Value::String(cwd.to_string()));

        for (k, v) in overrides {
            obj.insert(k.clone(), toml_to_json(v));
        }
    }

    Ok(val)
}

fn dispatch_emit(
    agent: AgentKind,
    event: &str,
    payload: &Value,
    emit_bin: Option<&Path>,
) -> Result<(), String> {
    let body = serde_json::to_string(payload).map_err(|e| format!("json encode failed: {e}"))?;
    if let Some(bin) = emit_bin {
        let mut child = Command::new(bin)
            .args(["emit", "--agent", agent.as_str(), "--event", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed spawning {}: {e}", bin.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let _ = child.wait();
    } else {
        let mut sink = io::sink();
        let _ =
            starfane_emit::run_emit(agent, Some(event), Cursor::new(body.as_bytes()), &mut sink);
    }
    Ok(())
}

fn parse_after_ms(trigger: &str) -> Option<u64> {
    trigger
        .strip_prefix("after_ms:")
        .and_then(|n| n.trim().parse::<u64>().ok())
}

fn run_step(
    step: &ScriptStep,
    script_path: &Path,
    agent: AgentKind,
    session_id: &str,
    cwd: &str,
    input: Option<&str>,
    emit_bin: Option<&Path>,
) -> Result<(), String> {
    let payload_path = resolve_payload_path(script_path, &step.payload);
    let raw = fs::read_to_string(&payload_path)
        .map_err(|e| format!("failed reading fixture {}: {e}", payload_path.display()))?;
    let payload = render_payload(&raw, agent, session_id, cwd, input, &step.set)?;
    dispatch_emit(agent, &step.event, &payload, emit_bin)
}

fn main() {
    let cli = match parse_args(std::env::args()) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let script_raw = match fs::read_to_string(&cli.script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed reading script {}: {e}", cli.script.display());
            std::process::exit(1);
        }
    };
    let script: FakeScript = match toml::from_str(&script_raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("invalid TOML script {}: {e}", cli.script.display());
            std::process::exit(1);
        }
    };

    let session_id = cli
        .session_id
        .or(script.meta.session_id)
        .unwrap_or_else(|| new_id("FAKE"));
    let cwd = script.meta.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    });
    let prompt = script
        .meta
        .idle_prompt
        .unwrap_or_else(|| default_idle_prompt(cli.agent).to_string());

    let mut idx = 0usize;

    // 1. Run initial `on = "start"` and any immediately following `after_ms:<n>` steps.
    while idx < script.step.len() {
        let step = &script.step[idx];
        if step.on == "start" {
            if let Err(e) = run_step(
                step,
                &cli.script,
                cli.agent,
                &session_id,
                &cwd,
                None,
                cli.emit_bin.as_deref(),
            ) {
                eprintln!("step {idx} error: {e}");
            }
            idx += 1;
        } else if let Some(ms) = parse_after_ms(&step.on) {
            thread::sleep(Duration::from_millis(ms));
            if let Err(e) = run_step(
                step,
                &cli.script,
                cli.agent,
                &session_id,
                &cwd,
                None,
                cli.emit_bin.as_deref(),
            ) {
                eprintln!("step {idx} error: {e}");
            }
            idx += 1;
        } else {
            break;
        }
    }

    // 2. Interactive loop: print idle prompt, read lines from stdin, fire scripted steps.
    let mut stdout = io::stdout();
    let _ = write!(stdout, "{prompt}");
    let _ = stdout.flush();

    let stdin = io::stdin();
    for line_res in stdin.lock().lines() {
        let Ok(line) = line_res else {
            break;
        };
        let trimmed = line.trim();
        if trimmed == "/exit" || trimmed == "exit" {
            break;
        }
        if trimmed.is_empty() {
            let _ = write!(stdout, "{prompt}");
            let _ = stdout.flush();
            continue;
        }

        // Fire the next `on = "input"` step, plus any chained `after_ms:<n>` steps.
        if idx < script.step.len() && script.step[idx].on == "input" {
            if let Err(e) = run_step(
                &script.step[idx],
                &cli.script,
                cli.agent,
                &session_id,
                &cwd,
                Some(trimmed),
                cli.emit_bin.as_deref(),
            ) {
                eprintln!("step {idx} error: {e}");
            }
            idx += 1;

            while idx < script.step.len() {
                if let Some(ms) = parse_after_ms(&script.step[idx].on) {
                    thread::sleep(Duration::from_millis(ms));
                    if let Err(e) = run_step(
                        &script.step[idx],
                        &cli.script,
                        cli.agent,
                        &session_id,
                        &cwd,
                        Some(trimmed),
                        cli.emit_bin.as_deref(),
                    ) {
                        eprintln!("step {idx} error: {e}");
                    }
                    idx += 1;
                } else {
                    break;
                }
            }
        }

        let _ = write!(stdout, "{prompt}");
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_and_applies_template_and_set_overrides() {
        let toml_str = r#"
[meta]
session_id = "sess-42"
cwd = "/tmp/demo"

[[step]]
on = "start"
event = "SessionStart"
payload = "tests/fixtures/agy/SessionStart.json"

[[step]]
on = "input"
event = "Stop"
payload = "tests/fixtures/agy/Stop__not_idle.json"
[step.set]
fullyIdle = true
"#;
        let script: FakeScript = toml::from_str(toml_str).unwrap();
        assert_eq!(script.step.len(), 2);
        assert_eq!(script.meta.session_id.as_deref(), Some("sess-42"));

        let base_fixture =
            r#"{"conversationId":"old","cwd":"/old","fullyIdle":false,"lastUserInput":"old"}"#;
        let rendered = render_payload(
            base_fixture,
            AgentKind::Agy,
            "sess-42",
            "/tmp/demo",
            Some("hello starfane"),
            &script.step[1].set,
        )
        .unwrap();

        assert_eq!(rendered["conversationId"], "sess-42");
        assert_eq!(rendered["cwd"], "/tmp/demo");
        assert_eq!(rendered["lastUserInput"], "hello starfane");
        assert_eq!(rendered["fullyIdle"], true);
    }

    #[test]
    fn parses_cli_flags_and_after_ms() {
        let args = vec![
            "fake-agent".to_string(),
            "--agent".to_string(),
            "codex".to_string(),
            "--script".to_string(),
            "demo.toml".to_string(),
            "--session-id".to_string(),
            "s-99".to_string(),
        ];
        let parsed = parse_args(args).unwrap();
        assert_eq!(parsed.agent, AgentKind::Codex);
        assert_eq!(parsed.script, PathBuf::from("demo.toml"));
        assert_eq!(parsed.session_id.as_deref(), Some("s-99"));
        assert_eq!(parse_after_ms("after_ms:250"), Some(250));
        assert_eq!(parse_after_ms("input"), None);
    }
}
