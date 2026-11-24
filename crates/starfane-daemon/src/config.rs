//! User config (`~/.config/starfane/config.toml`, contracts §7.1) and project
//! team config (`<root>/.starfane/team.toml`, architecture §3.4).

use serde::{Deserialize, Serialize};
use starfane_proto::{PipelineOptions, PlaybookKind, ReviewDepth, StageBinding, StageKind};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;

pub const DEFAULT_EASYTIER_CIDR: &str = "10.144.144.0/24";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub bind: String,
    pub port: u16,
    /// When `true` (default), every `/api/*` route requires the per-start token.
    /// When `false`, token checks are skipped and connections are strictly gated to
    /// loopback (`127.0.0.0/8`, `::1`), EasyTier (`easytier_cidrs` when `allow_easytier`),
    /// and Tailscale CGNAT (`100.64.0.0/10` when `allow_tailscale`).
    pub auth_enabled: bool,
    /// Allow EasyTier peer IPs and `Host`/`Origin` headers matching `easytier_cidrs`.
    pub allow_easytier: bool,
    /// Allowed EasyTier IPv4 CIDRs (defaults to `["10.144.144.0/24"]`).
    pub easytier_cidrs: Vec<String>,
    /// Allow Tailscale CGNAT (`100.64.0.0/10`) peer IPs and `*.ts.net` hosts.
    pub allow_tailscale: bool,
}
impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            bind: "127.0.0.1".into(),
            port: 7878,
            auth_enabled: true,
            allow_easytier: true,
            easytier_cidrs: vec![DEFAULT_EASYTIER_CIDR.into()],
            allow_tailscale: false,
        }
    }
}

/// Parse an IPv4 CIDR (`"10.144.144.0/24"`) or bare IPv4 address (`"10.144.144.3"` → `/32`).
pub fn parse_ipv4_cidr(raw: &str) -> Result<(Ipv4Addr, u8), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("CIDR cannot be empty".into());
    }
    if let Some((ip_str, prefix_str)) = trimmed.split_once('/') {
        let ip = ip_str
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|e| format!("invalid IPv4 address {ip_str:?}: {e}"))?;
        let prefix = prefix_str
            .trim()
            .parse::<u8>()
            .map_err(|e| format!("invalid CIDR prefix {prefix_str:?}: {e}"))?;
        if prefix > 32 {
            return Err(format!("CIDR prefix {prefix} out of range (0..=32)"));
        }
        Ok((ip, prefix))
    } else {
        let ip = trimmed
            .parse::<Ipv4Addr>()
            .map_err(|e| format!("invalid IPv4 address {trimmed:?}: {e}"))?;
        Ok((ip, 32))
    }
}

/// True when `ip` matches any valid IPv4 CIDR/address in `cidrs`.
pub fn ipv4_in_cidrs(ip: Ipv4Addr, cidrs: &[String]) -> bool {
    let ip_bits = u32::from(ip);
    cidrs.iter().any(|entry| {
        let Ok((net_ip, prefix)) = parse_ipv4_cidr(entry) else {
            return false;
        };
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        (ip_bits & mask) == (u32::from(net_ip) & mask)
    })
}

impl WebConfig {
    /// Validate `web` config fields (`easytier_cidrs`, etc.).
    pub fn validate(&self) -> Result<(), String> {
        for cidr in &self.easytier_cidrs {
            parse_ipv4_cidr(cidr)
                .map_err(|e| format!("invalid web.easytier_cidrs entry {cidr:?}: {e}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub watch_window_secs: u64,
    pub escalate_after_secs: u64,
    pub away_digest_after_secs: u64,
    pub cooldown_secs: u64,
}
impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            watch_window_secs: 90,
            escalate_after_secs: 180,
            away_digest_after_secs: 600,
            cooldown_secs: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSection {
    pub user_inactive_secs: u64,
    pub paste_settle_ms: u64,
    pub ack_timeout_ms: u64,
    pub max_pointer_len: usize,
}
impl Default for GateSection {
    fn default() -> Self {
        GateSection {
            user_inactive_secs: 15,
            paste_settle_ms: 250,
            ack_timeout_ms: 4000,
            max_pointer_len: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeishuConfig {
    pub enabled: bool,
    pub lark_cli: String,
    pub chat_id: String,
    pub allow_open_ids: Vec<String>,
    pub allow_chat_ids: Vec<String>,
}
impl Default for FeishuConfig {
    fn default() -> Self {
        FeishuConfig {
            enabled: false,
            lark_cli: "lark-cli".into(),
            chat_id: String::new(),
            allow_open_ids: Vec::new(),
            allow_chat_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    pub claude: String,
    pub codex: String,
    pub agy: String,
    pub tmux: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TmuxBackupConfig {
    /// Enable periodic tmux layout & agent-resume snapshots (`false` by default).
    pub enabled: bool,
    /// Snapshot check interval in seconds (default `300`, minimum `10` when enabled).
    pub interval_secs: u64,
    /// Rolling number of historical `snapshot-*.json` files retained (default `20`).
    pub max_snapshots: usize,
    /// Automatically restore `latest.json` at daemon startup when no tmux sessions exist.
    pub auto_restore_on_start: bool,
    /// Resume saved `claude` (`--resume`) and `codex` (`resume`) sessions after recreating panes.
    pub restore_agents: bool,
    /// Append bypass-permission flags when resuming `claude` / `codex` agents (`false` by default).
    pub restore_bypass_permissions: bool,
    /// Session names excluded from snapshots.
    pub exclude_sessions: Vec<String>,
}

impl Default for TmuxBackupConfig {
    fn default() -> Self {
        TmuxBackupConfig {
            enabled: false,
            interval_secs: 300,
            max_snapshots: 20,
            auto_restore_on_start: false,
            restore_agents: true,
            restore_bypass_permissions: false,
            exclude_sessions: Vec::new(),
        }
    }
}

impl TmuxBackupConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled {
            if self.interval_secs < 10 {
                return Err("tmux.backup.interval_secs must be >= 10 when enabled".into());
            }
            if self.max_snapshots < 1 {
                return Err("tmux.backup.max_snapshots must be >= 1 when enabled".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TmuxConfig {
    /// `tmux -L <name>`; empty = default server.
    pub socket_name: String,
    pub backup: TmuxBackupConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Also surface non-agent tmux panes (plain shells, training scripts) as `AgentKind::Shell`.
    pub include_shell_panes: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StarfaneConfig {
    pub web: WebConfig,
    pub notify: NotifyConfig,
    pub gate: GateSection,
    pub feishu: FeishuConfig,
    pub paths: PathsConfig,
    pub tmux: TmuxConfig,
    pub discovery: DiscoveryConfig,
}

impl StarfaneConfig {
    /// Missing file → defaults. Parse errors are reported (never silently ignored).
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg: StarfaneConfig =
                    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                cfg.web
                    .validate()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                cfg.tmux
                    .backup
                    .validate()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StarfaneConfig::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn tmux_socket(&self) -> Option<String> {
        let s = self.tmux.socket_name.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    pub fn tmux_bin(&self) -> String {
        non_empty(&self.paths.tmux).unwrap_or_else(|| "tmux".into())
    }

    pub fn agent_bin(&self, agent: starfane_proto::AgentKind) -> String {
        let configured = match agent {
            starfane_proto::AgentKind::Claude => &self.paths.claude,
            starfane_proto::AgentKind::Codex => &self.paths.codex,
            starfane_proto::AgentKind::Agy => &self.paths.agy,
            starfane_proto::AgentKind::Shell => return "sh".to_string(),
        };
        non_empty(configured).unwrap_or_else(|| agent.as_str().to_string())
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

// ---------------------------------------------------------------------------
// team.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamConfig {
    pub feature: BTreeMap<StageKind, StageBinding>,
    pub debug: BTreeMap<StageKind, StageBinding>,
    pub options: PipelineOptions,
    pub aliases: BTreeMap<String, String>,
    pub spawn_args: BTreeMap<starfane_proto::AgentKind, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTeam {
    playbooks: RawPlaybooks,
    models: RawModels,
    spawn: RawSpawn,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPlaybooks {
    feature: BTreeMap<String, toml::Value>,
    debug: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModels {
    aliases: BTreeMap<String, String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSpawn {
    claude_args: Vec<String>,
    codex_args: Vec<String>,
    agy_args: Vec<String>,
}

impl TeamConfig {
    /// Built-in defaults (architecture §3.4).
    pub fn builtin() -> Self {
        let b = |s: &str| StageBinding::from_shorthand(s).unwrap_or_default();
        let mut feature = BTreeMap::new();
        feature.insert(StageKind::Plan, b("claude"));
        feature.insert(StageKind::Implement, b("codex"));
        feature.insert(StageKind::TestReview, b("agy"));
        feature.insert(StageKind::Report, b("same_as:plan"));
        let mut debug = BTreeMap::new();
        debug.insert(StageKind::Triage, b("codex"));
        debug.insert(StageKind::Deep, b("claude"));
        TeamConfig {
            feature,
            debug,
            options: PipelineOptions::default(),
            aliases: BTreeMap::new(),
            spawn_args: BTreeMap::new(),
        }
    }

    /// Load `<root>/.starfane/team.toml` merged over built-in defaults.
    pub fn load(project_root: &Path) -> Result<Self, String> {
        let path = starfane_proto::paths::project_dir(project_root).join("team.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::builtin()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawTeam = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut team = Self::builtin();
        for (key, value) in &raw.playbooks.feature {
            match key.as_str() {
                "review_depth" => {
                    let s = value.as_str().ok_or("review_depth must be a string")?;
                    team.options.review_depth =
                        ReviewDepth::parse(s).ok_or_else(|| format!("bad review_depth {s}"))?;
                }
                "max_review_rounds" => {
                    team.options.max_review_rounds = value
                        .as_integer()
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or("max_review_rounds must be a non-negative integer")?;
                }
                stage => insert_binding(&mut team.feature, stage, value, PlaybookKind::Feature)?,
            }
        }
        for (key, value) in &raw.playbooks.debug {
            match key.as_str() {
                "triage_timebox" => {
                    let s = value
                        .as_str()
                        .ok_or("triage_timebox must be a string like 10m")?;
                    team.options.triage_timebox_secs =
                        parse_duration_secs(s).ok_or_else(|| format!("bad triage_timebox {s}"))?;
                }
                "auto_deep" => {
                    team.options.auto_deep = value.as_bool().ok_or("auto_deep must be bool")?;
                }
                stage => insert_binding(&mut team.debug, stage, value, PlaybookKind::Debug)?,
            }
        }
        team.aliases = raw.models.aliases;
        use starfane_proto::AgentKind as A;
        team.spawn_args.insert(A::Claude, raw.spawn.claude_args);
        team.spawn_args.insert(A::Codex, raw.spawn.codex_args);
        team.spawn_args.insert(A::Agy, raw.spawn.agy_args);
        Ok(team)
    }

    pub fn defaults_for(&self, playbook: PlaybookKind) -> &BTreeMap<StageKind, StageBinding> {
        match playbook {
            PlaybookKind::Feature => &self.feature,
            PlaybookKind::Debug => &self.debug,
        }
    }
}

fn insert_binding(
    map: &mut BTreeMap<StageKind, StageBinding>,
    stage: &str,
    value: &toml::Value,
    playbook: PlaybookKind,
) -> Result<(), String> {
    let kind = StageKind::parse(stage).ok_or_else(|| format!("unknown stage {stage}"))?;
    if kind.playbook() != playbook {
        return Err(format!(
            "stage {stage} does not belong to {}",
            playbook.as_str()
        ));
    }
    let s = value
        .as_str()
        .ok_or_else(|| format!("stage {stage} must be a shorthand string"))?;
    let binding = StageBinding::from_shorthand(s).map_err(|e| e.to_string())?;
    map.insert(kind, binding);
    Ok(())
}

/// `"90s" | "10m" | "2h" | "600"` → seconds.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let t = s.trim();
    let last = t.chars().last()?;
    let (num, mult): (&str, u64) = match last {
        's' | 'S' => (&t[..t.len() - 1], 1),
        'm' | 'M' => (&t[..t.len() - 1], 60),
        'h' | 'H' => (&t[..t.len() - 1], 3600),
        'd' | 'D' => (&t[..t.len() - 1], 86_400),
        c if c.is_ascii_digit() => (t, 1),
        _ => return None,
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_partial() {
        let def = WebConfig::default();
        assert_eq!(def.bind, "127.0.0.1");
        assert_eq!(def.port, 7878);
        assert!(def.auth_enabled);
        assert!(def.allow_easytier);
        assert_eq!(def.easytier_cidrs, vec![DEFAULT_EASYTIER_CIDR]);
        assert!(!def.allow_tailscale);

        let c: StarfaneConfig =
            toml::from_str("[web]\nport = 9000\n[feishu]\nenabled = true\n").unwrap();
        assert_eq!(c.web.port, 9000);
        assert_eq!(c.web.bind, "127.0.0.1");
        assert!(c.web.allow_easytier);
        assert_eq!(c.web.easytier_cidrs, vec![DEFAULT_EASYTIER_CIDR]);
        assert!(!c.web.allow_tailscale);
        assert!(c.feishu.enabled);
        assert_eq!(c.gate.paste_settle_ms, 250);
        assert_eq!(c.tmux_socket(), None);
        assert_eq!(c.agent_bin(starfane_proto::AgentKind::Agy), "agy");
    }

    #[test]
    fn easytier_cidr_parsing_and_validation() {
        let parse = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(
            parse_ipv4_cidr("10.144.144.0/24").unwrap(),
            (parse("10.144.144.0"), 24)
        );
        assert_eq!(
            parse_ipv4_cidr("10.144.144.3").unwrap(),
            (parse("10.144.144.3"), 32)
        );
        assert!(parse_ipv4_cidr("").is_err());
        assert!(parse_ipv4_cidr("10.144.144.0/33").is_err());
        assert!(parse_ipv4_cidr("not-an-ip/24").is_err());

        let cidrs = vec!["10.144.144.0/24".to_string()];
        assert!(ipv4_in_cidrs(parse("10.144.144.0"), &cidrs));
        assert!(ipv4_in_cidrs(parse("10.144.144.3"), &cidrs));
        assert!(ipv4_in_cidrs(parse("10.144.144.255"), &cidrs));
        assert!(!ipv4_in_cidrs(parse("10.144.145.1"), &cidrs));
        assert!(!ipv4_in_cidrs(parse("10.17.0.50"), &cidrs));
        assert!(!ipv4_in_cidrs(parse("100.64.10.20"), &cidrs));

        let dir = std::env::temp_dir().join("starfane-cfg-cidr-test.toml");
        std::fs::write(&dir, "[web]\neasytier_cidrs = [\"bad-cidr\"]\n").unwrap();
        let err = StarfaneConfig::load(&dir).unwrap_err();
        let _ = std::fs::remove_file(&dir);
        assert!(err.contains("easytier_cidrs"), "{err}");
    }

    #[test]
    fn missing_config_is_default() {
        let c = StarfaneConfig::load(Path::new("/nonexistent/starfane/config.toml")).unwrap();
        assert_eq!(c, StarfaneConfig::default());
    }

    #[test]
    fn team_parse() {
        let t = TeamConfig::parse(
            r#"
[playbooks.feature]
plan = "claude:opus@high"
implement = "codex"
test_review = "agy /swarm"
report = "same_as:plan"
review_depth = "D2"
[playbooks.debug]
triage = "codex"
deep = "claude:opus@high"
triage_timebox = "5m"
auto_deep = false
[models.aliases]
opus = "claude:opus"
[spawn]
claude_args = ["--dangerously-skip-permissions"]
"#,
        )
        .unwrap();
        assert_eq!(t.feature[&StageKind::Plan].model.as_deref(), Some("opus"));
        assert_eq!(t.options.review_depth, ReviewDepth::D2);
        assert_eq!(t.options.triage_timebox_secs, 300);
        assert!(!t.options.auto_deep);
        assert_eq!(t.aliases["opus"], "claude:opus");
        assert_eq!(t.spawn_args[&starfane_proto::AgentKind::Claude].len(), 1);
        assert!(TeamConfig::parse("[playbooks.feature]\ntriage = \"codex\"\n").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("10m"), Some(600));
        assert_eq!(parse_duration_secs("90s"), Some(90));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("15"), Some(15));
        assert_eq!(parse_duration_secs("x"), None);
    }

    #[test]
    fn discovery_and_tmux_backup_config_validation() {
        let def = StarfaneConfig::default();
        assert!(!def.discovery.include_shell_panes);
        assert!(!def.tmux.backup.enabled);
        assert_eq!(def.tmux.backup.interval_secs, 300);
        assert_eq!(def.tmux.backup.max_snapshots, 20);
        assert!(!def.tmux.backup.auto_restore_on_start);
        assert!(def.tmux.backup.restore_agents);
        assert!(!def.tmux.backup.restore_bypass_permissions);

        let dir = std::env::temp_dir().join("starfane-cfg-bk-test.toml");
        std::fs::write(&dir, "[tmux.backup]\nenabled = true\ninterval_secs = 5\n").unwrap();
        let err = StarfaneConfig::load(&dir).unwrap_err();
        assert!(err.contains("interval_secs"), "{err}");

        std::fs::write(
            &dir,
            "[discovery]\ninclude_shell_panes = true\n[tmux.backup]\nenabled = true\ninterval_secs = 60\nmax_snapshots = 10\nauto_restore_on_start = true\nexclude_sessions = [\"tmp\"]\n",
        )
        .unwrap();
        let ok = StarfaneConfig::load(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        assert!(ok.discovery.include_shell_panes);
        assert!(ok.tmux.backup.enabled);
        assert_eq!(ok.tmux.backup.interval_secs, 60);
        assert_eq!(ok.tmux.backup.max_snapshots, 10);
        assert!(ok.tmux.backup.auto_restore_on_start);
        assert_eq!(ok.tmux.backup.exclude_sessions, vec!["tmp".to_string()]);
    }
}
