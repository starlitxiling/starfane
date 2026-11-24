//! Read-only environment, CLI, hook, Codex trust-hash, and disk doctor checks (contracts §3).

use crate::install::{hook_config_path, is_starfane_grouped_entry, starfane_command_prefix, Scope};
use serde::Serialize;
use serde_json::Value;
use starfane_proto::AgentKind;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorFinding {
    pub id: String,
    pub agent: Option<AgentKind>,
    pub level: Level,
    pub message: String,
    pub fix_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DoctorEnv {
    pub home: PathBuf,
    pub path_var: String,
    pub starfane_bin: PathBuf,
}

/// Resolve an agent CLI's absolute path from `env.path_var` (and common user locations).
pub fn resolve_agent_bin(agent: AgentKind, env: &DoctorEnv) -> Option<PathBuf> {
    resolve_bin(agent.as_str(), env)
}

/// Run all read-only environment & integration diagnostics.
pub fn doctor_checks(env: &DoctorEnv) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();

    // 1. Agent binaries + hook status
    for agent in AgentKind::ALL {
        match resolve_agent_bin(agent, env) {
            Some(bin) => findings.push(DoctorFinding {
                id: format!("bin.{}", agent.as_str()),
                agent: Some(agent),
                level: Level::Ok,
                message: format!("found {} at {}", agent.as_str(), bin.display()),
                fix_hint: None,
            }),
            None => findings.push(DoctorFinding {
                id: format!("bin.{}", agent.as_str()),
                agent: Some(agent),
                level: Level::Warn,
                message: format!("{} CLI not found in PATH", agent.as_str()),
                fix_hint: Some(format!(
                    "install {} or ensure it is on PATH",
                    agent.as_str()
                )),
            }),
        }

        check_agent_hooks(agent, env, &mut findings);
    }

    // 2. Codex config.toml notify coexistence & [hooks.state] trust hash check
    check_codex_trust_and_notify(env, &mut findings);

    // 3. tmux (+ scrollback history-limit advisory)
    match resolve_bin("tmux", env) {
        Some(bin) => {
            findings.push(DoctorFinding {
                id: "bin.tmux".into(),
                agent: None,
                level: Level::Ok,
                message: format!("found tmux at {}", bin.display()),
                fix_hint: None,
            });
            check_tmux_history_limit(&bin, &env.home, &mut findings);
        }
        None => findings.push(DoctorFinding {
            id: "bin.tmux".into(),
            agent: None,
            level: Level::Error,
            message: "tmux not found in PATH".into(),
            fix_hint: Some("install tmux (e.g. `brew install tmux`)".into()),
        }),
    }

    // 3b. zellij (optional >= 0.44 multiplexer backend)
    check_zellij_version(env, &mut findings);

    // 4. lark-cli (optional Feishu channel)
    match resolve_bin("lark-cli", env) {
        Some(bin) => findings.push(DoctorFinding {
            id: "bin.lark-cli".into(),
            agent: None,
            level: Level::Ok,
            message: format!("found lark-cli at {}", bin.display()),
            fix_hint: None,
        }),
        None => findings.push(DoctorFinding {
            id: "bin.lark-cli".into(),
            agent: None,
            level: Level::Warn,
            message: "lark-cli not found (Feishu two-way channel unavailable)".into(),
            fix_hint: Some("install lark-cli if you want Feishu notifications/commands".into()),
        }),
    }

    // 5. Rust toolchain & sccache (helpful for contributor/dev check)
    let rustc = resolve_bin("rustc", env);
    let cargo = resolve_bin("cargo", env);
    let sccache = resolve_bin("sccache", env);
    match (rustc, cargo) {
        (Some(r), Some(_)) => findings.push(DoctorFinding {
            id: "env.rust".into(),
            agent: None,
            level: Level::Ok,
            message: format!(
                "Rust toolchain ready ({}; sccache={})",
                r.display(),
                if sccache.is_some() {
                    "present"
                } else {
                    "absent"
                }
            ),
            fix_hint: if sccache.is_none() {
                Some("optional: `brew install sccache` to speed up rebuilds".into())
            } else {
                None
            },
        }),
        _ => findings.push(DoctorFinding {
            id: "env.rust".into(),
            agent: None,
            level: Level::Warn,
            message: "rustc/cargo not detected on PATH".into(),
            fix_hint: Some(
                "install minimal toolchain: `rustup --profile minimal --component clippy,rustfmt`"
                    .into(),
            ),
        }),
    }

    // 6. Disk free space check on HOME
    check_disk_space(&env.home, &mut findings);

    findings
}

fn check_agent_hooks(agent: AgentKind, env: &DoctorEnv, findings: &mut Vec<DoctorFinding>) {
    let path = hook_config_path(agent, &env.home, &Scope::User);
    let id = format!("hooks.{}", agent.as_str());

    if !path.exists() {
        findings.push(DoctorFinding {
            id,
            agent: Some(agent),
            level: Level::Warn,
            message: format!(
                "{} hook config not found ({})",
                agent.as_str(),
                path.display()
            ),
            fix_hint: Some(format!("run `starfane install --agent {}`", agent.as_str())),
        });
        return;
    }

    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            findings.push(DoctorFinding {
                id,
                agent: Some(agent),
                level: Level::Error,
                message: format!("cannot read {}: {e}", path.display()),
                fix_hint: None,
            });
            return;
        }
    };

    let val: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            findings.push(DoctorFinding {
                id,
                agent: Some(agent),
                level: Level::Error,
                message: format!("invalid JSON in {}: {e}", path.display()),
                fix_hint: None,
            });
            return;
        }
    };

    let prefix = starfane_command_prefix(&env.starfane_bin);
    let installed = match agent {
        AgentKind::Claude | AgentKind::Codex => val
            .get("hooks")
            .and_then(Value::as_object)
            .map(|m| {
                m.values().any(|arr| {
                    arr.as_array().is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            is_starfane_grouped_entry(entry, &prefix)
                                || is_starfane_grouped_entry(entry, "starfane emit --agent ")
                        })
                    })
                })
            })
            .unwrap_or(false),
        AgentKind::Agy => val.get("starfane").is_some(),
        AgentKind::Shell => false,
    };

    if installed {
        findings.push(DoctorFinding {
            id,
            agent: Some(agent),
            level: Level::Ok,
            message: format!("starfane hooks installed in {}", path.display()),
            fix_hint: None,
        });
    } else {
        findings.push(DoctorFinding {
            id,
            agent: Some(agent),
            level: Level::Warn,
            message: format!("starfane hooks not installed in {}", path.display()),
            fix_hint: Some(format!("run `starfane install --agent {}`", agent.as_str())),
        });
    }
}

fn check_codex_trust_and_notify(env: &DoctorEnv, findings: &mut Vec<DoctorFinding>) {
    let config_path = env.home.join(".codex").join("config.toml");
    let hooks_path = hook_config_path(AgentKind::Codex, &env.home, &Scope::User);

    let Ok(raw_cfg) = fs::read_to_string(&config_path) else {
        return;
    };

    let Ok(cfg) = raw_cfg.parse::<toml::Table>() else {
        findings.push(DoctorFinding {
            id: "codex.config".into(),
            agent: Some(AgentKind::Codex),
            level: Level::Warn,
            message: format!("could not parse {}", config_path.display()),
            fix_hint: None,
        });
        return;
    };

    if cfg.contains_key("notify") {
        findings.push(DoctorFinding {
            id: "codex.notify".into(),
            agent: Some(AgentKind::Codex),
            level: Level::Ok,
            message: "existing Codex `notify` config detected; starfane leaves it untouched and uses hooks.json".into(),
            fix_hint: None,
        });
    }

    // Count starfane hook entries in hooks.json and compare with trusted hashes in [hooks.state].
    let starfane_hook_count = count_codex_starfane_hooks(&hooks_path, &env.starfane_bin);
    if starfane_hook_count == 0 {
        return;
    }

    let hooks_path_str = hooks_path.to_string_lossy();
    let trusted_entries = cfg
        .get("hooks")
        .and_then(toml::Value::as_table)
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .map(|state| {
            state
                .iter()
                .filter(|(k, v)| {
                    k.starts_with(hooks_path_str.as_ref())
                        && v.as_table()
                            .and_then(|t| t.get("trusted_hash"))
                            .and_then(toml::Value::as_str)
                            .is_some()
                })
                .count()
        })
        .unwrap_or(0);

    if trusted_entries >= starfane_hook_count {
        findings.push(DoctorFinding {
            id: "codex.hook_trust".into(),
            agent: Some(AgentKind::Codex),
            level: Level::Ok,
            message: format!(
                "Codex hook trust hashes present ({trusted_entries} trusted entries in config.toml)"
            ),
            fix_hint: None,
        });
    } else {
        findings.push(DoctorFinding {
            id: "codex.hook_trust".into(),
            agent: Some(AgentKind::Codex),
            level: Level::Warn,
            message: format!(
                "Codex has {starfane_hook_count} starfane hooks in hooks.json but only {trusted_entries} trusted entries in [hooks.state]"
            ),
            fix_hint: Some(
                "launch `codex` interactively once and approve the new hooks.json entries".into(),
            ),
        });
    }
}

fn count_codex_starfane_hooks(hooks_path: &Path, starfane_bin: &Path) -> usize {
    let Ok(raw) = fs::read_to_string(hooks_path) else {
        return 0;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return 0;
    };
    let prefix = starfane_command_prefix(starfane_bin);
    val.get("hooks")
        .and_then(Value::as_object)
        .map(|events| {
            events
                .values()
                .filter_map(Value::as_array)
                .flat_map(|arr| arr.iter())
                .filter(|entry| {
                    is_starfane_grouped_entry(entry, &prefix)
                        || is_starfane_grouped_entry(entry, "starfane emit --agent ")
                })
                .count()
        })
        .unwrap_or(0)
}

fn check_tmux_history_limit(tmux_bin: &Path, home: &Path, findings: &mut Vec<DoctorFinding>) {
    let live_limit = Command::new(tmux_bin)
        .args(["show-options", "-gv", "history-limit"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        });

    let conf_limit = [home.join(".tmux.conf"), home.join(".config/tmux/tmux.conf")]
        .into_iter()
        .find_map(|p| fs::read_to_string(p).ok())
        .and_then(|text| {
            text.lines().rev().find_map(|line| {
                let trimmed = line.trim();
                if trimmed.starts_with('#') || !trimmed.contains("history-limit") {
                    return None;
                }
                trimmed
                    .split_whitespace()
                    .last()
                    .and_then(|tok| tok.parse::<u64>().ok())
            })
        });

    let effective = live_limit.or(conf_limit);
    match effective {
        Some(n) if n >= 10_000 => findings.push(DoctorFinding {
            id: "tmux.history_limit".into(),
            agent: None,
            level: Level::Ok,
            message: format!("tmux history-limit is {n} lines"),
            fix_hint: None,
        }),
        Some(n) => findings.push(DoctorFinding {
            id: "tmux.history_limit".into(),
            agent: None,
            level: Level::Warn,
            message: format!(
                "tmux history-limit is only {n} lines (older terminal scrollback is discarded by tmux)"
            ),
            fix_hint: Some("add `set -g history-limit 50000` to ~/.tmux.conf for deep Web terminal history".into()),
        }),
        None => {}
    }
}

fn parse_semver_minor(version_out: &str) -> Option<(u32, u32, u32)> {
    let tok = version_out
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = tok.trim_start_matches('v').split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts
        .next()
        .and_then(|p| p.split('-').next())
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

fn check_zellij_version(env: &DoctorEnv, findings: &mut Vec<DoctorFinding>) {
    let Some(bin) = resolve_bin("zellij", env) else {
        findings.push(DoctorFinding {
            id: "bin.zellij".into(),
            agent: None,
            level: Level::Ok,
            message: "zellij not installed (optional; tmux backend active)".into(),
            fix_hint: Some(
                "optional: `brew install zellij` (>= 0.44) to enable Zellij panes".into(),
            ),
        });
        return;
    };
    let ver_str = Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    match parse_semver_minor(&ver_str) {
        Some((maj, min, _)) if maj > 0 || min >= 44 => findings.push(DoctorFinding {
            id: "bin.zellij".into(),
            agent: None,
            level: Level::Ok,
            message: format!("found {ver_str} at {} (>= 0.44 supported)", bin.display()),
            fix_hint: None,
        }),
        Some((maj, min, patch)) => findings.push(DoctorFinding {
            id: "bin.zellij".into(),
            agent: None,
            level: Level::Warn,
            message: format!(
                "zellij {maj}.{min}.{patch} at {} is older than 0.44 (Zellij backend will skip gracefully)",
                bin.display()
            ),
            fix_hint: Some("upgrade to Zellij >= 0.44 (`brew upgrade zellij`) for CLI JSON/pane-id support".into()),
        }),
        None => findings.push(DoctorFinding {
            id: "bin.zellij".into(),
            agent: None,
            level: Level::Warn,
            message: format!("found zellij at {} but could not parse version ({ver_str:?})", bin.display()),
            fix_hint: Some("ensure zellij >= 0.44 is installed".into()),
        }),
    }
}

fn check_disk_space(path: &Path, findings: &mut Vec<DoctorFinding>) {
    let target = if path.exists() { path } else { Path::new("/") };
    let Ok(output) = Command::new("df")
        .args(["-k", &target.to_string_lossy()])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(data_line) = text.lines().nth(1) else {
        return;
    };
    let cols: Vec<&str> = data_line.split_whitespace().collect();
    // POSIX df -k columns: Filesystem 1024-blocks Used Available Capacity ...
    let Some(avail_kb) = cols.get(3).and_then(|s| s.parse::<u64>().ok()) else {
        return;
    };
    let avail_gib = avail_kb as f64 / (1024.0 * 1024.0);
    let level = if avail_gib < 5.0 {
        Level::Warn
    } else {
        Level::Ok
    };
    findings.push(DoctorFinding {
        id: "env.disk".into(),
        agent: None,
        level,
        message: format!("{avail_gib:.1} GiB available on {}", target.display()),
        fix_hint: if level == Level::Warn {
            Some(
                "low disk space; run `cargo clean` or `cargo sweep` to free Cargo artifacts".into(),
            )
        } else {
            None
        },
    });
}

fn resolve_bin(name: &str, env: &DoctorEnv) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = env
        .path_var
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    // Standard user locations when tmux/daemon PATH is minimal.
    dirs.push(env.home.join(".local").join("bin"));
    dirs.push(env.home.join(".cargo").join("bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));

    for dir in dirs {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}
