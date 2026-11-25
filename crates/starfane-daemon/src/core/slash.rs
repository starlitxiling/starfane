//! Native `/` slash command catalog and lightweight local skill/command scanner.
//!
//! Serves `GET /api/slash-commands?agent=<claude|codex|agy>` with:
//! 1. Curated verified native commands per agent (`group: "native"`).
//! 2. Lightweight read-only local skill/command scan (`group: "skill"`, 60s in-memory TTL cache):
//!    - Claude: `~/.claude/commands/*.md`, `~/.claude/skills/*/SKILL.md`
//!    - Codex: `~/.codex/skills/*/SKILL.md`
//!    - AGY: `~/.gemini/config/skills/*/SKILL.md`
//! 3. Built-in Starfane console shortcut commands (`group: "starfane"`).

use serde::{Deserialize, Serialize};
use starfane_proto::AgentKind;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_FRONTMATTER_READ_BYTES: u64 = 4096;
const MAX_DESC_CHARS: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub cmd: String,
    pub args_hint: String,
    pub desc: String,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_args: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandsResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub commands: Vec<SlashCommand>,
}

#[derive(Debug, Default)]
pub struct SlashCache {
    fetched_at: Option<Instant>,
    skills_by_agent: BTreeMap<AgentKind, Vec<SlashCommand>>,
}

impl SlashCache {
    /// Return cached local skills for all agents, refreshing if older than 60s.
    pub fn get_or_refresh(
        &mut self,
        home: Option<&Path>,
    ) -> &BTreeMap<AgentKind, Vec<SlashCommand>> {
        let now = Instant::now();
        let fresh = self
            .fetched_at
            .is_some_and(|ts| now.saturating_duration_since(ts) < CACHE_TTL);
        if !fresh {
            self.skills_by_agent = scan_all_local_skills(home);
            self.fetched_at = Some(now);
        }
        &self.skills_by_agent
    }
}

pub fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Build the response payload for `GET /api/slash-commands` using cached local skills.
pub fn build_catalog(
    agent: Option<AgentKind>,
    skills_by_agent: &BTreeMap<AgentKind, Vec<SlashCommand>>,
    is_en: bool,
) -> SlashCommandsResponse {
    let mut commands = Vec::new();
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();

    let agents: Vec<AgentKind> = match agent {
        Some(a) => vec![a],
        None => vec![AgentKind::Agy, AgentKind::Claude, AgentKind::Codex],
    };

    for a in agents {
        let tag = a.as_str().to_string();
        for cmd in native_commands_for_lang(a, is_en) {
            if seen.insert((Some(tag.clone()), cmd.cmd.clone())) {
                commands.push(cmd);
            }
        }
        if let Some(skills) = skills_by_agent.get(&a) {
            for sk in skills {
                if seen.insert((Some(tag.clone()), sk.cmd.clone())) {
                    let mut localized = sk.clone();
                    if is_en {
                        if localized.args_hint == "[参数]" {
                            localized.args_hint = "[args]".to_string();
                        }
                        if localized.desc == "本地自定义命令" {
                            localized.desc = "Local custom command".to_string();
                        } else if localized.desc == "本地技能" {
                            localized.desc = "Local skill".to_string();
                        }
                    }
                    commands.push(localized);
                }
            }
        }
    }

    for b in starfane_commands_for_lang(is_en) {
        if seen.insert((None, b.cmd.clone())) {
            commands.push(b);
        }
    }

    SlashCommandsResponse {
        agent: agent.map(|a| a.as_str().to_string()),
        commands,
    }
}

fn native(
    agent: AgentKind,
    cmd: &str,
    args_hint: &str,
    desc: &str,
    needs_args: bool,
) -> SlashCommand {
    SlashCommand {
        cmd: cmd.to_string(),
        args_hint: args_hint.to_string(),
        desc: desc.to_string(),
        group: "native".to_string(),
        needs_args: Some(needs_args),
        action: None,
        agent: Some(agent.as_str().to_string()),
    }
}

fn starfane_cmd(cmd: &str, desc: &str, action: &str) -> SlashCommand {
    SlashCommand {
        cmd: cmd.to_string(),
        args_hint: String::new(),
        desc: desc.to_string(),
        group: "starfane".to_string(),
        needs_args: None,
        action: Some(action.to_string()),
        agent: None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn native_commands(agent: AgentKind) -> Vec<SlashCommand> {
    native_commands_for_lang(agent, false)
}

pub fn native_commands_for_lang(agent: AgentKind, is_en: bool) -> Vec<SlashCommand> {
    if is_en {
        return match agent {
            AgentKind::Agy => vec![
                native(
                    agent,
                    "/swarm",
                    "<task>",
                    "Parallel multi-agent swarm collaboration",
                    true,
                ),
                native(
                    agent,
                    "/boost",
                    "[task]",
                    "High-effort deep reasoning and verification",
                    false,
                ),
                native(
                    agent,
                    "/teamwork",
                    "<task>",
                    "Multi-role team collaboration workflow",
                    true,
                ),
                native(
                    agent,
                    "/plan",
                    "[goal]",
                    "Enter step-by-step planning mode",
                    false,
                ),
                native(
                    agent,
                    "/compact",
                    "[note]",
                    "Compact context window to free tokens",
                    false,
                ),
                native(
                    agent,
                    "/model",
                    "[model]",
                    "Inspect or switch current model",
                    false,
                ),
                native(
                    agent,
                    "/effort",
                    "[level]",
                    "Adjust reasoning effort level",
                    false,
                ),
                native(
                    agent,
                    "/clear",
                    "",
                    "Clear current conversation context",
                    false,
                ),
                native(agent, "/help", "", "Show AGY help and keybindings", false),
            ],
            AgentKind::Claude => vec![
                native(
                    agent,
                    "/compact",
                    "[note]",
                    "Compact conversation history to free context",
                    false,
                ),
                native(agent, "/clear", "", "Clear current session context", false),
                native(agent, "/plan", "[goal]", "Switch to planning mode", false),
                native(
                    agent,
                    "/review",
                    "[goal]",
                    "Review current code changes",
                    false,
                ),
                native(agent, "/model", "[model]", "Switch Claude model", false),
                native(
                    agent,
                    "/effort",
                    "[level]",
                    "Adjust reasoning effort level",
                    false,
                ),
                native(
                    agent,
                    "/context",
                    "",
                    "Inspect context window breakdown",
                    false,
                ),
                native(
                    agent,
                    "/cost",
                    "",
                    "Show session token and cost usage",
                    false,
                ),
                native(
                    agent,
                    "/agents",
                    "",
                    "Inspect or manage subagent definitions",
                    false,
                ),
                native(
                    agent,
                    "/mcp",
                    "",
                    "Check connected MCP server status",
                    false,
                ),
                native(agent, "/help", "", "Show Claude Code help", false),
            ],
            AgentKind::Codex => vec![
                native(
                    agent,
                    "/compact",
                    "",
                    "Compact context to free window space",
                    false,
                ),
                native(agent, "/new", "", "Start a new conversation session", false),
                native(
                    agent,
                    "/review",
                    "[note]",
                    "Review workspace code changes",
                    false,
                ),
                native(
                    agent,
                    "/diff",
                    "",
                    "Show current Git working tree diff",
                    false,
                ),
                native(
                    agent,
                    "/model",
                    "[model]",
                    "Switch Codex model and reasoning effort",
                    false,
                ),
                native(
                    agent,
                    "/status",
                    "",
                    "Inspect session and quota status",
                    false,
                ),
                native(
                    agent,
                    "/permissions",
                    "",
                    "Inspect or adjust sandbox and approval policy",
                    false,
                ),
                native(agent, "/mcp", "", "Check MCP tool connections", false),
                native(agent, "/help", "", "Show Codex CLI help", false),
            ],
            AgentKind::Shell => Vec::new(),
        };
    }
    match agent {
        AgentKind::Agy => vec![
            native(agent, "/swarm", "<任务>", "群体多 Agent 并行协作", true),
            native(agent, "/boost", "[任务]", "增强推理与执行深度", false),
            native(agent, "/teamwork", "<任务>", "多角色协作执行任务", true),
            native(agent, "/plan", "[目标]", "进入分步规划模式", false),
            native(agent, "/compact", "[说明]", "压缩对话上下文释放窗口", false),
            native(agent, "/model", "[模型]", "查看或切换当前模型", false),
            native(agent, "/effort", "[级别]", "调整推理深度等级", false),
            native(agent, "/clear", "", "清空当前对话上下文", false),
            native(agent, "/help", "", "查看 AGY 帮助与快捷键", false),
        ],
        AgentKind::Claude => vec![
            native(agent, "/compact", "[说明]", "压缩对话历史释放上下文", false),
            native(agent, "/clear", "", "清空当前会话上下文", false),
            native(agent, "/plan", "[目标]", "切换至规划模式", false),
            native(agent, "/review", "[目标]", "审查当前代码变更", false),
            native(agent, "/model", "[模型]", "切换 Claude 模型", false),
            native(agent, "/effort", "[级别]", "调整推理思考强度", false),
            native(agent, "/context", "", "查看上下文窗口占用明细", false),
            native(agent, "/cost", "", "查看当前会话 Token 与费用", false),
            native(agent, "/agents", "", "查看或管理子 Agent 配置", false),
            native(agent, "/mcp", "", "查看已连接的 MCP 服务状态", false),
            native(agent, "/help", "", "查看 Claude Code 帮助", false),
        ],
        AgentKind::Codex => vec![
            native(agent, "/compact", "", "压缩上下文释放窗口", false),
            native(agent, "/new", "", "开启新对话会话", false),
            native(agent, "/review", "[说明]", "审查工作区代码变更", false),
            native(agent, "/diff", "", "查看当前 Git 变更差异", false),
            native(
                agent,
                "/model",
                "[模型]",
                "切换 Codex 模型与推理等级",
                false,
            ),
            native(agent, "/status", "", "查看会话与限额状态", false),
            native(agent, "/permissions", "", "查看或调整沙箱与审批权限", false),
            native(agent, "/mcp", "", "查看 MCP 工具连接状态", false),
            native(agent, "/help", "", "查看 Codex CLI 帮助", false),
        ],
        AgentKind::Shell => Vec::new(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn starfane_commands() -> Vec<SlashCommand> {
    starfane_commands_for_lang(false)
}

pub fn starfane_commands_for_lang(is_en: bool) -> Vec<SlashCommand> {
    if is_en {
        return vec![
            starfane_cmd(
                "/feature",
                "Switch to Feature Pipeline mode",
                "mode_feature",
            ),
            starfane_cmd("/debug", "Switch to Two-Speed Debug mode", "mode_debug"),
            starfane_cmd("/mute", "Open Feishu notification mute menu", "open_mute"),
            starfane_cmd(
                "/terminal",
                "Toggle reading view / raw terminal",
                "toggle_terminal",
            ),
            starfane_cmd(
                "/drawer",
                "Open current session detail drawer",
                "open_drawer",
            ),
        ];
    }
    vec![
        starfane_cmd("/feature", "切换为新功能协作模式", "mode_feature"),
        starfane_cmd("/debug", "切换为排查 Debug 协作模式", "mode_debug"),
        starfane_cmd("/mute", "打开飞书通知静音设置", "open_mute"),
        starfane_cmd("/terminal", "切换阅读视图 / 原始终端", "toggle_terminal"),
        starfane_cmd("/drawer", "展开当前会话详情抽屉", "open_drawer"),
    ]
}

/// Scan read-only local skill and command directories under `home` for all three agents.
pub fn scan_all_local_skills(home: Option<&Path>) -> BTreeMap<AgentKind, Vec<SlashCommand>> {
    let mut map = BTreeMap::new();
    let Some(home) = home else {
        return map;
    };

    let mut claude = Vec::new();
    scan_markdown_commands(
        &home.join(".claude").join("commands"),
        AgentKind::Claude,
        &mut claude,
    );
    scan_skill_dirs(
        &home.join(".claude").join("skills"),
        AgentKind::Claude,
        &mut claude,
    );
    sort_and_dedup(&mut claude);
    map.insert(AgentKind::Claude, claude);

    let mut codex = Vec::new();
    scan_skill_dirs(
        &home.join(".codex").join("skills"),
        AgentKind::Codex,
        &mut codex,
    );
    sort_and_dedup(&mut codex);
    map.insert(AgentKind::Codex, codex);

    let mut agy = Vec::new();
    scan_skill_dirs(
        &home.join(".gemini").join("config").join("skills"),
        AgentKind::Agy,
        &mut agy,
    );
    sort_and_dedup(&mut agy);
    map.insert(AgentKind::Agy, agy);

    map
}

fn sort_and_dedup(cmds: &mut Vec<SlashCommand>) {
    cmds.sort_by(|a, b| a.cmd.cmp(&b.cmd));
    let mut seen = HashSet::new();
    cmds.retain(|c| seen.insert(c.cmd.clone()));
}

fn scan_markdown_commands(dir: &Path, agent: AgentKind, out: &mut Vec<SlashCommand>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.starts_with('.') {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(slug) = sanitize_command_slug(stem) else {
            continue;
        };
        let head = read_head(&path);
        let (fm_name, fm_desc) = head
            .as_deref()
            .map(parse_frontmatter)
            .unwrap_or((None, None));
        let cmd_slug = fm_name
            .as_deref()
            .and_then(sanitize_command_slug)
            .unwrap_or(slug);
        let desc = fm_desc
            .filter(|d| !d.is_empty())
            .or_else(|| head.as_deref().and_then(first_body_summary))
            .unwrap_or_else(|| "本地自定义命令".to_string());
        out.push(SlashCommand {
            cmd: format!("/{cmd_slug}"),
            args_hint: "[参数]".to_string(),
            desc: truncate_chars(&desc, MAX_DESC_CHARS),
            group: "skill".to_string(),
            needs_args: Some(false),
            action: None,
            agent: Some(agent.as_str().to_string()),
        });
    }
}

fn scan_skill_dirs(dir: &Path, agent: AgentKind, out: &mut Vec<SlashCommand>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(dirname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if dirname.starts_with('.') {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let head = read_head(&skill_md);
        let (fm_name, fm_desc) = head
            .as_deref()
            .map(parse_frontmatter)
            .unwrap_or((None, None));
        let Some(cmd_slug) = fm_name
            .as_deref()
            .and_then(sanitize_command_slug)
            .or_else(|| sanitize_command_slug(dirname))
        else {
            continue;
        };
        let desc = fm_desc
            .filter(|d| !d.is_empty())
            .or_else(|| head.as_deref().and_then(first_body_summary))
            .unwrap_or_else(|| "本地技能".to_string());
        out.push(SlashCommand {
            cmd: format!("/{cmd_slug}"),
            args_hint: "[参数]".to_string(),
            desc: truncate_chars(&desc, MAX_DESC_CHARS),
            group: "skill".to_string(),
            needs_args: Some(false),
            action: None,
            agent: Some(agent.as_str().to_string()),
        });
    }
}

fn read_head(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut buf = Vec::with_capacity(MAX_FRONTMATTER_READ_BYTES as usize);
    f.take(MAX_FRONTMATTER_READ_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn sanitize_command_slug(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches('/');
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
    {
        Some(s.to_string())
    } else {
        None
    }
}

/// Parse `name` and `description` from a Markdown YAML frontmatter block (`--- ... ---`).
pub fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return (None, None);
    };
    if first.trim().trim_start_matches('\u{FEFF}') != "---" {
        return (None, None);
    }
    let mut name: Option<String> = None;
    let mut desc: Option<String> = None;
    let mut in_block_desc = false;
    let mut block_parts: Vec<String> = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        if in_block_desc {
            if line.starts_with(' ') || line.starts_with('\t') {
                if !trimmed.is_empty() {
                    block_parts.push(trimmed.to_string());
                }
                continue;
            } else {
                in_block_desc = false;
                if !block_parts.is_empty() {
                    desc = Some(block_parts.join(" "));
                }
            }
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "name" if name.is_none() => {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        name = Some(v);
                    }
                }
                "description" if desc.is_none() => {
                    if matches!(val, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                        in_block_desc = true;
                        block_parts.clear();
                    } else {
                        let v = unquote_yaml(val);
                        if !v.is_empty() {
                            desc = Some(v);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if in_block_desc && !block_parts.is_empty() && desc.is_none() {
        desc = Some(block_parts.join(" "));
    }
    (name, desc)
}

fn unquote_yaml(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        v[1..v.len() - 1].trim().to_string()
    } else {
        v.to_string()
    }
}

fn first_body_summary(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines
        .next()
        .is_some_and(|l| l.trim().trim_start_matches('\u{FEFF}') == "---")
    {
        for l in lines.by_ref() {
            let t = l.trim();
            if t == "---" || t == "..." {
                break;
            }
        }
    } else {
        lines = text.lines();
    }
    for l in lines {
        let t = l.trim().trim_start_matches('#').trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = collapsed.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses_inline_quoted_and_block_scalars() {
        let md1 = "---\nname: commit\ndescription: \"按照 git 规则自动提交\"\n---\n# Body";
        assert_eq!(
            parse_frontmatter(md1),
            (Some("commit".into()), Some("按照 git 规则自动提交".into()))
        );

        let md2 = "---\nname: 'paper-read'\ndescription: >-\n  Deep analysis of\n  academic papers.\n---\n";
        assert_eq!(
            parse_frontmatter(md2),
            (
                Some("paper-read".into()),
                Some("Deep analysis of academic papers.".into())
            )
        );
    }

    #[test]
    fn scans_local_skills_and_commands_with_60s_cache() {
        let root = std::env::temp_dir().join(format!(
            "starfane-slash-test-{}",
            starfane_proto::id::new_id("T")
        ));
        fs::create_dir_all(root.join(".claude/commands")).unwrap();
        fs::create_dir_all(root.join(".claude/skills/my-skill")).unwrap();
        fs::create_dir_all(root.join(".claude/skills/.hidden")).unwrap();
        fs::create_dir_all(root.join(".codex/skills/review-loop")).unwrap();
        fs::create_dir_all(root.join(".gemini/config/skills/excalidraw-diagram")).unwrap();

        fs::write(
            root.join(".claude/commands/quick-fix.md"),
            "---\ndescription: 快速修复 lint\n---\nbody",
        )
        .unwrap();
        fs::write(
            root.join(".claude/skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: Custom Claude skill\n---\n",
        )
        .unwrap();
        fs::write(
            root.join(".claude/skills/.hidden/SKILL.md"),
            "---\nname: hidden\ndescription: Should be skipped\n---\n",
        )
        .unwrap();
        fs::write(
            root.join(".codex/skills/review-loop/SKILL.md"),
            "---\nname: review-loop\ndescription: Codex review loop\n---\n",
        )
        .unwrap();
        fs::write(
            root.join(".gemini/config/skills/excalidraw-diagram/SKILL.md"),
            "---\nname: excalidraw-diagram\ndescription: Draw Excalidraw diagrams\n---\n",
        )
        .unwrap();

        let mut cache = SlashCache::default();
        let scanned = cache.get_or_refresh(Some(&root));
        let claude_res = build_catalog(Some(AgentKind::Claude), scanned, false);
        assert_eq!(claude_res.agent.as_deref(), Some("claude"));
        assert!(claude_res
            .commands
            .iter()
            .any(|c| c.cmd == "/compact" && c.group == "native"));
        assert!(claude_res
            .commands
            .iter()
            .any(|c| c.cmd == "/quick-fix" && c.desc == "快速修复 lint" && c.group == "skill"));
        assert!(claude_res
            .commands
            .iter()
            .any(|c| c.cmd == "/my-skill" && c.desc == "Custom Claude skill"));
        assert!(!claude_res.commands.iter().any(|c| c.cmd == "/hidden"));
        assert!(claude_res.commands.iter().any(|c| c.cmd == "/feature"
            && c.group == "starfane"
            && c.action.as_deref() == Some("mode_feature")));

        let codex_res = build_catalog(Some(AgentKind::Codex), scanned, false);
        assert!(codex_res.commands.iter().any(|c| c.cmd == "/review-loop"));
        let agy_res = build_catalog(Some(AgentKind::Agy), scanned, false);
        assert!(agy_res
            .commands
            .iter()
            .any(|c| c.cmd == "/swarm" && c.needs_args == Some(true)));
        assert!(agy_res
            .commands
            .iter()
            .any(|c| c.cmd == "/excalidraw-diagram"));

        // English deployment catalog uses ASCII args hints and English descriptions.
        let agy_en = build_catalog(Some(AgentKind::Agy), scanned, true);
        assert!(agy_en
            .commands
            .iter()
            .filter(|c| c.group != "skill")
            .all(|c| c.desc.is_ascii() && c.args_hint.is_ascii()));
        let drawer_en = agy_en
            .commands
            .iter()
            .find(|c| c.cmd == "/drawer")
            .expect("/drawer in catalog");
        assert_eq!(drawer_en.action.as_deref(), Some("open_drawer"));
        assert_eq!(native_commands(AgentKind::Agy).len(), 9);
        assert_eq!(starfane_commands().len(), 5);

        // Add a new skill on disk and confirm 60s TTL cache serves the cached snapshot without re-scanning.
        fs::create_dir_all(root.join(".codex/skills/fresh-skill")).unwrap();
        fs::write(
            root.join(".codex/skills/fresh-skill/SKILL.md"),
            "---\nname: fresh-skill\ndescription: Added after cache\n---\n",
        )
        .unwrap();
        let cached_again = cache.get_or_refresh(Some(&root));
        assert!(!cached_again[&AgentKind::Codex]
            .iter()
            .any(|c| c.cmd == "/fresh-skill"));

        let _ = fs::remove_dir_all(&root);
    }
}
