//! User config (`~/.config/starfane/config.toml`, contracts §7.1) and project
//! team config (`<root>/.starfane/team.toml`, architecture §3.4).

use serde::{Deserialize, Serialize};
use starfane_proto::{PipelineOptions, PlaybookKind, ReviewDepth, StageBinding, StageKind};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

pub const DEFAULT_EASYTIER_CIDR: &str = "10.144.144.0/24";
/// Unassigned, non-ephemeral default Web console port (distinct from 7878/8080/3000 collisions).
pub const DEFAULT_WEB_PORT: u16 = 27418;
/// Default Web console display language configured at deployment (`"zh"` or `"en"`).
pub const DEFAULT_WEB_LANG: &str = "zh";

/// Optional extra scoped API token (e.g. read-only viewer or peer federation client).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopedTokenConfig {
    pub label: String,
    pub token: String,
    pub token_env: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub bind: String,
    pub port: u16,
    /// When `true` (default), every `/api/*` route requires the per-start token (or a configured scoped token).
    /// When `false`, token checks are skipped and connections are strictly gated by `AccessGate`
    /// (loopback + `allowed_cidrs` + enabled presets).
    pub auth_enabled: bool,
    /// Legacy preset flag: allow EasyTier peer IPs and `Host`/`Origin` headers matching `easytier_cidrs`.
    pub allow_easytier: bool,
    /// Allowed EasyTier IPv4 CIDRs (defaults to `["10.144.144.0/24"]`).
    pub easytier_cidrs: Vec<String>,
    /// Legacy preset flag: allow Tailscale (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`, `*.ts.net`).
    pub allow_tailscale: bool,
    /// Network-agnostic peer IP CIDRs (IPv4 and IPv6, e.g. `"10.144.144.0/24"`, `"fd00::/8"`).
    pub allowed_cidrs: Vec<String>,
    /// Network-agnostic allowed `Host`/`Origin` hostnames or `*.suffix` wildcards
    /// (e.g. `"console.example.com"`, `"*.ts.net"`). Supports reverse-proxy ports (80/443/omitted).
    pub allowed_hosts: Vec<String>,
    /// Named network presets (`"easytier"`, `"tailscale"`).
    pub presets: Vec<String>,
    /// Optional extra scoped tokens (supports `read_only = true` viewer tokens or peer tokens).
    pub scoped_tokens: Vec<ScopedTokenConfig>,
    /// Optional terminal monospace font family prepended in the Web console (e.g. `"JetBrainsMono Nerd Font"`).
    pub terminal_font: String,
    /// Web console UI display language fixed at deployment (`"zh"` | `"en"`, defaults to `"zh"`).
    pub lang: String,
}
impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            bind: "127.0.0.1".into(),
            port: DEFAULT_WEB_PORT,
            auth_enabled: true,
            allow_easytier: true,
            easytier_cidrs: vec![DEFAULT_EASYTIER_CIDR.into()],
            allow_tailscale: false,
            allowed_cidrs: Vec::new(),
            allowed_hosts: Vec::new(),
            presets: Vec::new(),
            scoped_tokens: Vec::new(),
            terminal_font: String::new(),
            lang: DEFAULT_WEB_LANG.into(),
        }
    }
}

/// Parse an IPv4 or IPv6 CIDR (or bare IP address → `/32` or `/128`).
pub fn parse_ip_cidr(raw: &str) -> Result<(IpAddr, u8), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("CIDR cannot be empty".into());
    }
    if let Some((ip_str, prefix_str)) = trimmed.split_once('/') {
        let ip = ip_str
            .trim()
            .parse::<IpAddr>()
            .map_err(|e| format!("invalid IP address {ip_str:?}: {e}"))?;
        let prefix = prefix_str
            .trim()
            .parse::<u8>()
            .map_err(|e| format!("invalid CIDR prefix {prefix_str:?}: {e}"))?;
        let max_prefix = match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max_prefix {
            return Err(format!(
                "CIDR prefix {prefix} out of range (0..={max_prefix})"
            ));
        }
        Ok((ip, prefix))
    } else {
        let ip = trimmed
            .parse::<IpAddr>()
            .map_err(|e| format!("invalid IP address {trimmed:?}: {e}"))?;
        let max_prefix = match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        Ok((ip, max_prefix))
    }
}

/// Parse an IPv4 CIDR (`"10.144.144.0/24"`) or bare IPv4 address (`"10.144.144.3"` → `/32`).
pub fn parse_ipv4_cidr(raw: &str) -> Result<(Ipv4Addr, u8), String> {
    match parse_ip_cidr(raw)? {
        (IpAddr::V4(ip), prefix) => Ok((ip, prefix)),
        (IpAddr::V6(_), _) => Err(format!("expected IPv4 CIDR, got IPv6 {raw:?}")),
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

/// True when `ip` matches an IPv6 CIDR `(net, prefix)`.
pub fn ipv6_in_cidr(ip: Ipv6Addr, net: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0u128
    } else {
        u128::MAX << (128 - prefix)
    };
    (u128::from(ip) & mask) == (u128::from(net) & mask)
}

impl WebConfig {
    /// Normalize configured deployment language to canonical `"zh"` or `"en"`.
    pub fn normalized_lang(&self) -> Result<&'static str, String> {
        let lower = self.lang.trim().to_ascii_lowercase();
        match lower.as_str() {
            "" | "zh" | "zh-cn" | "zh_cn" => Ok("zh"),
            "en" | "en-us" | "en_us" => Ok("en"),
            _ => Err(format!(
                "invalid web.lang {:?} (supported: zh, en)",
                self.lang
            )),
        }
    }

    /// Effective canonical language (`"zh"` or `"en"`), falling back to `"zh"` if unset.
    pub fn effective_lang(&self) -> &'static str {
        self.normalized_lang().unwrap_or(DEFAULT_WEB_LANG)
    }

    /// True when the deployed Web UI is configured for English (`web.lang = "en"`).
    pub fn is_en(&self) -> bool {
        self.effective_lang() == "en"
    }

    /// Validate `web` config fields (`lang`, `easytier_cidrs`, `allowed_cidrs`, `presets`, `allowed_hosts`).
    pub fn validate(&self) -> Result<(), String> {
        self.normalized_lang()?;
        for cidr in &self.easytier_cidrs {
            parse_ipv4_cidr(cidr)
                .map_err(|e| format!("invalid web.easytier_cidrs entry {cidr:?}: {e}"))?;
        }
        for cidr in &self.allowed_cidrs {
            parse_ip_cidr(cidr)
                .map_err(|e| format!("invalid web.allowed_cidrs entry {cidr:?}: {e}"))?;
        }
        for preset in &self.presets {
            let p = preset.trim().to_ascii_lowercase();
            if !matches!(p.as_str(), "easytier" | "tailscale") {
                return Err(format!(
                    "unknown web.presets entry {preset:?} (supported: easytier, tailscale)"
                ));
            }
        }
        for host in &self.allowed_hosts {
            let h = host.trim();
            if h.is_empty() || h.contains('/') || h.contains('@') || h.contains(char::is_whitespace)
            {
                return Err(format!("invalid web.allowed_hosts entry {host:?}"));
            }
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
    pub zellij: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZellijConfig {
    /// Auto-discover and operate Zellij (>= 0.44) sessions alongside tmux (gracefully skipped if absent).
    pub enabled: bool,
}

impl Default for ZellijConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Ownership scope for notification delivery (Split Ownership model).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOwnership {
    /// Only deliver notifications for sessions local to this daemon (default; keeps work credentials isolated).
    #[default]
    NodeLocal,
    /// Deliver on the hub (for SSH zero-install hosts or explicitly hub-routed sessions).
    Hub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifierKind {
    Feishu,
    Telegram,
    Discord,
    Slack,
    Wecom,
    Qq,
    Webhook,
    Macos,
}

impl NotifierKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Wecom => "wecom",
            Self::Qq => "qq",
            Self::Webhook => "webhook",
            Self::Macos => "macos",
        }
    }
}

fn deserialize_opt_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptStringOrInt;
    impl<'de> serde::de::Visitor<'de> for OptStringOrInt {
        type Value = Option<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string or integer")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_any(OptStringOrInt)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
    }

    deserializer.deserialize_option(OptStringOrInt)
}

/// Opt-in pluggable notification channel configured in `~/.config/starfane/config.toml` (`[[notifiers]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifierConfig {
    pub id: String,
    pub kind: NotifierKind,
    pub enabled: bool,
    pub ownership: NotifyOwnership,
    /// Optional `lark-cli --profile <name>` profile for multi-tenant Feishu accounts.
    pub profile: String,
    pub lark_cli: String,
    pub chat_id: String,
    pub allow_open_ids: Vec<String>,
    pub allow_chat_ids: Vec<String>,
    /// Bot token for Telegram or Slack Web API (prefer `bot_token_env` so tokens stay out of config files).
    pub bot_token: String,
    pub bot_token_env: String,
    /// Optional forum/topic thread ID: positive integer for Telegram `message_thread_id`,
    /// or snowflake/string ID appended as `?thread_id=...` for Discord webhooks.
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub thread_id: Option<String>,
    /// Optional API base URL override (Telegram defaults to `https://api.telegram.org`,
    /// Slack Web API defaults to `https://slack.com/api`).
    pub api_base_url: Option<String>,
    /// Incoming Webhook URL for Discord, Slack, or generic Webhook (prefer `webhook_url_env`).
    pub webhook_url: Option<String>,
    /// Env var name holding the Incoming Webhook URL for Discord, Slack, or generic Webhook.
    pub webhook_url_env: Option<String>,
    /// Optional Discord webhook display username (defaults to `"starfane"`).
    pub username: Option<String>,
    /// Slack Web API `chat.postMessage` target channel ID or name (e.g. `"C01234567"` or `"#ops"`).
    pub channel: Option<String>,
    /// Optional WeCom Group Robot `mentioned_list` (user IDs or `"@all"`).
    pub mentioned_list: Vec<String>,
    /// Optional WeCom Group Robot `mentioned_mobile_list` (mobile numbers or `"@all"`).
    pub mentioned_mobile_list: Vec<String>,
    /// Optional QQ OneBot v11 target group ID (accepts string or integer in TOML).
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub group_id: Option<String>,
    /// Optional QQ OneBot v11 target private user ID (accepts string or integer in TOML).
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub user_id: Option<String>,
    /// Generic webhook endpoint URL (backward-compatible primary field for `kind = "webhook"`).
    pub url: String,
    /// Env var holding an optional webhook Bearer/secret header value.
    pub secret_env: String,
    /// Optional extra HTTP headers sent by `kind = "webhook"` requests.
    pub headers: BTreeMap<String, String>,
    /// Optional macOS notification sound name (e.g. `"Glass"`).
    pub sound: String,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: NotifierKind::Webhook,
            enabled: true,
            ownership: NotifyOwnership::NodeLocal,
            profile: String::new(),
            lark_cli: "lark-cli".into(),
            chat_id: String::new(),
            allow_open_ids: Vec::new(),
            allow_chat_ids: Vec::new(),
            bot_token: String::new(),
            bot_token_env: String::new(),
            thread_id: None,
            api_base_url: None,
            webhook_url: None,
            webhook_url_env: None,
            username: None,
            channel: None,
            mentioned_list: Vec::new(),
            mentioned_mobile_list: Vec::new(),
            group_id: None,
            user_id: None,
            url: String::new(),
            secret_env: String::new(),
            headers: BTreeMap::new(),
            sound: String::new(),
        }
    }
}

fn validate_http_url(label: &str, raw: &str) -> Result<(), String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "{label} cannot contain whitespace or control characters"
        ));
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("{label} must start with http:// or https://"))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.starts_with(':') {
        return Err(format!("{label} must include a valid host"));
    }
    Ok(())
}

fn validate_env_var_name(label: &str, raw: &str) -> Result<(), String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !first_ok || !rest_ok {
        return Err(format!(
            "{label} {name:?} must be a valid environment variable name ([A-Za-z_][A-Za-z0-9_]*)"
        ));
    }
    Ok(())
}

fn validate_no_control_chars(label: &str, raw: &str) -> Result<(), String> {
    if raw.chars().any(char::is_control) {
        return Err(format!("{label} cannot contain control characters"));
    }
    Ok(())
}

impl NotifierConfig {
    /// Parse `thread_id` as a positive Telegram `message_thread_id`.
    pub fn telegram_thread_id(&self) -> Result<Option<i64>, String> {
        let Some(raw) = self.thread_id.as_deref() else {
            return Ok(None);
        };
        let t = raw.trim();
        if t.is_empty() {
            return Err(format!(
                "telegram notifier {:?} thread_id cannot be empty",
                self.id.trim()
            ));
        }
        let parsed = t.parse::<i64>().map_err(|_| {
            format!(
                "telegram notifier {:?} thread_id {t:?} must be a positive integer",
                self.id.trim()
            )
        })?;
        if parsed <= 0 {
            return Err(format!(
                "telegram notifier {:?} thread_id {parsed} must be > 0",
                self.id.trim()
            ));
        }
        Ok(Some(parsed))
    }

    /// Parse a positive QQ OneBot v11 `group_id` or `user_id` integer.
    pub fn parse_positive_u64_field(
        &self,
        field: &str,
        value: Option<&str>,
    ) -> Result<Option<u64>, String> {
        let Some(raw) = value else {
            return Ok(None);
        };
        let t = raw.trim();
        if t.is_empty() {
            return Err(format!(
                "qq notifier {:?} {field} cannot be empty",
                self.id.trim()
            ));
        }
        let parsed = t.parse::<u64>().map_err(|_| {
            format!(
                "qq notifier {:?} {field} {t:?} must be a positive integer",
                self.id.trim()
            )
        })?;
        if parsed == 0 {
            return Err(format!(
                "qq notifier {:?} {field} must be > 0",
                self.id.trim()
            ));
        }
        Ok(Some(parsed))
    }

    pub fn validate(&self) -> Result<(), String> {
        let id = self.id.trim();
        if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "invalid notifiers[].id {id:?} (must be non-empty without whitespace or control characters)"
            ));
        }
        if !self.enabled {
            return Ok(());
        }

        let has_inline_token = !self.bot_token.trim().is_empty();
        let has_env_token = !self.bot_token_env.trim().is_empty();
        if has_inline_token
            && self
                .bot_token
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "notifier {id:?} bot_token cannot contain whitespace or control characters"
            ));
        }
        if has_env_token {
            validate_env_var_name(
                &format!("notifier {id:?} bot_token_env"),
                &self.bot_token_env,
            )?;
        }

        if let Some(ref raw_base) = self.api_base_url {
            validate_http_url(&format!("notifier {id:?} api_base_url"), raw_base)?;
        }
        if let Some(ref raw_hook) = self.webhook_url {
            validate_http_url(&format!("notifier {id:?} webhook_url"), raw_hook)?;
        }
        if let Some(ref raw_hook_env) = self.webhook_url_env {
            validate_env_var_name(&format!("notifier {id:?} webhook_url_env"), raw_hook_env)?;
        }

        match self.kind {
            NotifierKind::Feishu => {
                validate_no_control_chars(
                    &format!("feishu notifier {id:?} lark_cli"),
                    &self.lark_cli,
                )?;
                validate_no_control_chars(
                    &format!("feishu notifier {id:?} profile"),
                    &self.profile,
                )?;
                validate_no_control_chars(
                    &format!("feishu notifier {id:?} chat_id"),
                    &self.chat_id,
                )?;
                if (id != "feishu-default" || !self.profile.trim().is_empty())
                    && self.chat_id.trim().is_empty()
                {
                    return Err(format!("feishu notifier {id:?} requires non-empty chat_id"));
                }
            }
            NotifierKind::Telegram => {
                if !has_inline_token && !has_env_token {
                    return Err(format!(
                        "telegram notifier {id:?} requires bot_token or bot_token_env"
                    ));
                }
                if self.chat_id.trim().is_empty() {
                    return Err(format!(
                        "telegram notifier {id:?} requires non-empty chat_id"
                    ));
                }
                validate_no_control_chars(
                    &format!("telegram notifier {id:?} chat_id"),
                    &self.chat_id,
                )?;
                self.telegram_thread_id()?;
            }
            NotifierKind::Discord => {
                let has_hook = self
                    .webhook_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_hook_env = self
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_hook && !has_hook_env {
                    return Err(format!(
                        "discord notifier {id:?} requires webhook_url or webhook_url_env"
                    ));
                }
                if let Some(ref user) = self.username {
                    let u = user.trim();
                    if u.is_empty() {
                        return Err(format!("discord notifier {id:?} username cannot be empty"));
                    }
                    validate_no_control_chars(&format!("discord notifier {id:?} username"), u)?;
                    if u.chars().count() > 80 {
                        return Err(format!(
                            "discord notifier {id:?} username must be <= 80 characters"
                        ));
                    }
                }
                if let Some(ref tid) = self.thread_id {
                    let t = tid.trim();
                    if t.is_empty()
                        || !t
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        return Err(format!(
                            "discord notifier {id:?} thread_id {tid:?} must be non-empty ASCII [A-Za-z0-9_-]"
                        ));
                    }
                }
            }
            NotifierKind::Slack => {
                let has_hook = self
                    .webhook_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_hook_env = self
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_webhook_mode = has_hook || has_hook_env;
                let has_bot_mode = has_inline_token || has_env_token;
                if let Some(ref ch) = self.channel {
                    let c = ch.trim();
                    if c.is_empty() {
                        return Err(format!("slack notifier {id:?} channel cannot be empty"));
                    }
                    validate_no_control_chars(&format!("slack notifier {id:?} channel"), c)?;
                }
                let has_channel = self
                    .channel
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());

                if has_webhook_mode && has_bot_mode {
                    return Err(format!(
                        "slack notifier {id:?} cannot configure both webhook_url/webhook_url_env and bot_token/bot_token_env; choose either Incoming Webhook mode or Bot Web API mode"
                    ));
                }
                if has_webhook_mode {
                    if self.api_base_url.is_some() {
                        return Err(format!(
                            "slack notifier {id:?} api_base_url is only valid in Bot Web API mode"
                        ));
                    }
                } else if has_bot_mode {
                    if !has_channel {
                        return Err(format!(
                            "slack notifier {id:?} in Bot Web API mode requires non-empty channel"
                        ));
                    }
                } else if has_channel || self.api_base_url.is_some() {
                    return Err(format!(
                        "slack notifier {id:?} requires bot_token or bot_token_env when channel or api_base_url is set"
                    ));
                } else {
                    return Err(format!(
                        "slack notifier {id:?} requires either webhook_url/webhook_url_env (Incoming Webhook mode) or bot_token/bot_token_env + channel (Bot Web API mode)"
                    ));
                }
            }
            NotifierKind::Wecom => {
                let has_hook = self
                    .webhook_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_hook_env = self
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_hook && !has_hook_env {
                    return Err(format!(
                        "wecom notifier {id:?} requires webhook_url or webhook_url_env"
                    ));
                }
                for (field, list) in [
                    ("mentioned_list", &self.mentioned_list),
                    ("mentioned_mobile_list", &self.mentioned_mobile_list),
                ] {
                    for entry in list {
                        if entry.trim().is_empty() {
                            return Err(format!(
                                "wecom notifier {id:?} {field} cannot contain empty entries"
                            ));
                        }
                        validate_no_control_chars(
                            &format!("wecom notifier {id:?} {field}"),
                            entry,
                        )?;
                    }
                }
            }
            NotifierKind::Qq => {
                let has_hook = self
                    .webhook_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_hook_env = self
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_webhook_mode = has_hook || has_hook_env;
                let has_api_base = self
                    .api_base_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let group_id =
                    self.parse_positive_u64_field("group_id", self.group_id.as_deref())?;
                let user_id = self.parse_positive_u64_field("user_id", self.user_id.as_deref())?;
                let has_target = group_id.is_some() || user_id.is_some();

                if !self.secret_env.trim().is_empty() {
                    validate_env_var_name(
                        &format!("qq notifier {id:?} secret_env"),
                        &self.secret_env,
                    )?;
                }
                if has_webhook_mode && has_api_base {
                    return Err(format!(
                        "qq notifier {id:?} cannot configure both webhook_url/webhook_url_env and api_base_url; choose either OneBot v11 HTTP API mode or Webhook gateway mode"
                    ));
                }
                if has_api_base {
                    if !has_target {
                        return Err(format!(
                            "qq notifier {id:?} in OneBot v11 mode requires group_id and/or user_id"
                        ));
                    }
                } else if !has_webhook_mode {
                    return Err(format!(
                        "qq notifier {id:?} requires either api_base_url + group_id/user_id (OneBot v11 mode) or webhook_url/webhook_url_env (Webhook gateway mode)"
                    ));
                }
            }
            NotifierKind::Webhook => {
                let has_url = !self.url.trim().is_empty();
                let has_hook = self
                    .webhook_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                let has_hook_env = self
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if has_url {
                    validate_http_url(&format!("webhook notifier {id:?} url"), &self.url)?;
                } else if !has_hook && !has_hook_env {
                    return Err(format!(
                        "webhook notifier {id:?} requires url (or webhook_url / webhook_url_env)"
                    ));
                }
                if !self.secret_env.trim().is_empty() {
                    validate_env_var_name(
                        &format!("webhook notifier {id:?} secret_env"),
                        &self.secret_env,
                    )?;
                }
                for (k, v) in &self.headers {
                    let hk = k.trim();
                    if hk.is_empty()
                        || hk
                            .chars()
                            .any(|c| c == ':' || c.is_whitespace() || c.is_control())
                    {
                        return Err(format!(
                            "webhook notifier {id:?} has invalid header name {k:?}"
                        ));
                    }
                    validate_no_control_chars(
                        &format!("webhook notifier {id:?} header {hk:?}"),
                        v,
                    )?;
                }
            }
            NotifierKind::Macos => {
                validate_no_control_chars(&format!("macos notifier {id:?} sound"), &self.sound)?;
            }
        }
        Ok(())
    }
}

/// Differential routing rule mapping `(node, project, cwd, agent, event_kinds)` -> logical notifier IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyRouteConfig {
    pub notifiers: Vec<String>,
    pub node: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    pub event_kinds: Vec<String>,
    pub ownership: Option<NotifyOwnership>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalNodeConfig {
    pub id: String,
    pub display_name: String,
}

impl Default for LocalNodeConfig {
    fn default() -> Self {
        Self {
            id: starfane_proto::LOCAL_HOST_ID.to_string(),
            display_name: "Local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKindConfig {
    Ssh,
    #[serde(alias = "ssh_peer")]
    Peer,
}

fn default_true() -> bool {
    true
}

pub const DEFAULT_SSH_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Remote federated host entry (`[[nodes]]`) supporting mixed zero-install SSH and peer `starfaned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Immutable host identifier (e.g. `"tencent"`, `"mac-xiling"`). Used in pane prefixes `<id>:%1`.
    pub id: String,
    /// Optional human-friendly mutable display name (overridable at runtime via `/api/hosts/{id}/rename`).
    pub display_name: Option<String>,
    pub kind: NodeKindConfig,
    pub enabled: bool,
    /// SSH target (`user@host` or `~/.ssh/config` alias) when `kind = "ssh"`, or optional
    /// SSH loopback tunnel bootstrap target when `kind = "peer"` / `"ssh_peer"`.
    pub ssh_target: String,
    pub ssh_args: Vec<String>,
    /// Optional `ssh` binary override (empty falls back to `STARFANE_SSH_BIN` env or `"ssh"`).
    pub ssh_bin: String,
    /// OpenSSH `ConnectTimeout` in seconds for `kind = "ssh"` (default `5`).
    pub connect_timeout_secs: u64,
    /// Enable OpenSSH zlib compression (`-o Compression=yes`, default `true`).
    pub compression: bool,
    /// Enable real-time push watcher (`tmux -C` + multi-session digest on SSH, `/api/events` SSE
    /// subscriber on Peer; default `true`).
    #[serde(default = "default_true")]
    pub push_watch: bool,
    pub tmux_bin: String,
    pub tmux_socket: String,
    /// Optional remote `claude` binary override for SSH spawns (empty defaults to `"claude"`).
    pub claude_bin: String,
    /// Optional remote `codex` binary override for SSH spawns (empty defaults to `"codex"`).
    pub codex_bin: String,
    /// Optional remote `agy` binary override for SSH spawns (empty defaults to `"agy"`).
    pub agy_bin: String,
    pub include_shell_panes: bool,
    /// Candidate outbound HTTP(S) base URLs tried in priority order when `kind = "peer"`.
    pub endpoints: Vec<String>,
    /// Peer auth token (prefer `token_env`).
    pub token: String,
    pub token_env: String,
    /// Defaults to `hub` for `ssh` and `node_local` (Split Ownership) for `peer`.
    pub notify_ownership: Option<NotifyOwnership>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            display_name: None,
            kind: NodeKindConfig::Ssh,
            enabled: true,
            ssh_target: String::new(),
            ssh_args: Vec::new(),
            ssh_bin: String::new(),
            connect_timeout_secs: DEFAULT_SSH_CONNECT_TIMEOUT_SECS,
            compression: true,
            push_watch: true,
            tmux_bin: "tmux".into(),
            tmux_socket: String::new(),
            claude_bin: String::new(),
            codex_bin: String::new(),
            agy_bin: String::new(),
            include_shell_panes: false,
            endpoints: Vec::new(),
            token: String::new(),
            token_env: String::new(),
            notify_ownership: None,
        }
    }
}

impl NodeConfig {
    pub fn effective_ownership(&self) -> NotifyOwnership {
        self.notify_ownership.unwrap_or(match self.kind {
            NodeKindConfig::Ssh => NotifyOwnership::Hub,
            NodeKindConfig::Peer => NotifyOwnership::NodeLocal,
        })
    }

    pub fn ssh_bin_override(&self) -> Option<String> {
        non_empty(&self.ssh_bin)
    }

    pub fn effective_connect_timeout_secs(&self) -> u64 {
        self.connect_timeout_secs.max(1)
    }

    pub fn agent_bin(&self, agent: starfane_proto::AgentKind) -> String {
        let configured = match agent {
            starfane_proto::AgentKind::Claude => &self.claude_bin,
            starfane_proto::AgentKind::Codex => &self.codex_bin,
            starfane_proto::AgentKind::Agy => &self.agy_bin,
            starfane_proto::AgentKind::Shell => return "sh".to_string(),
        };
        non_empty(configured).unwrap_or_else(|| agent.as_str().to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        let id = self.id.trim();
        if id.is_empty()
            || id == starfane_proto::LOCAL_HOST_ID
            || id == "zj"
            || id.contains([':', '/', ' ', '\t'])
        {
            return Err(format!(
                "invalid nodes[].id {id:?} (must be non-empty, not 'local'/'zj', and contain no ':', '/', or spaces)"
            ));
        }
        if !self.enabled {
            return Ok(());
        }
        match self.kind {
            NodeKindConfig::Ssh => {
                if self.ssh_target.trim().is_empty() {
                    return Err(format!("ssh node {id:?} requires non-empty ssh_target"));
                }
                if self.connect_timeout_secs == 0 {
                    return Err(format!("ssh node {id:?} connect_timeout_secs must be >= 1"));
                }
                validate_no_control_chars(&format!("ssh node {id:?} ssh_bin"), &self.ssh_bin)?;
                validate_no_control_chars(
                    &format!("ssh node {id:?} claude_bin"),
                    &self.claude_bin,
                )?;
                validate_no_control_chars(&format!("ssh node {id:?} codex_bin"), &self.codex_bin)?;
                validate_no_control_chars(&format!("ssh node {id:?} agy_bin"), &self.agy_bin)?;
            }
            NodeKindConfig::Peer => {
                let has_ssh_tunnel = !self.ssh_target.trim().is_empty();
                if self.endpoints.iter().any(|u| u.trim().is_empty()) {
                    return Err(format!(
                        "peer node {id:?} endpoints cannot contain empty URLs"
                    ));
                }
                if self.endpoints.is_empty() && !has_ssh_tunnel {
                    return Err(format!(
                        "peer node {id:?} requires at least one non-empty URL in endpoints (or ssh_target for SSH tunnel bootstrap)"
                    ));
                }
                if has_ssh_tunnel {
                    if self.connect_timeout_secs == 0 {
                        return Err(format!(
                            "peer node {id:?} connect_timeout_secs must be >= 1 when ssh_target is set"
                        ));
                    }
                    validate_no_control_chars(&format!("peer node {id:?} ssh_bin"), &self.ssh_bin)?;
                }
            }
        }
        Ok(())
    }
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
    pub zellij: ZellijConfig,
    pub discovery: DiscoveryConfig,
    pub notifiers: Vec<NotifierConfig>,
    pub notify_routes: Vec<NotifyRouteConfig>,
    pub node: LocalNodeConfig,
    pub nodes: Vec<NodeConfig>,
}

impl StarfaneConfig {
    /// Missing file → defaults. Parse errors are reported (never silently ignored).
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg: StarfaneConfig =
                    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                cfg.validate()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StarfaneConfig::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.web.validate()?;
        self.tmux.backup.validate()?;
        let mut seen_notifiers = std::collections::HashSet::new();
        for n in &self.notifiers {
            n.validate()?;
            let id = n.id.trim();
            if !seen_notifiers.insert(id.to_string()) {
                return Err(format!("duplicate notifiers[].id {id:?}"));
            }
        }
        let mut seen_nodes = std::collections::HashSet::new();
        for node in &self.nodes {
            node.validate()?;
            if !seen_nodes.insert(node.id.trim().to_string()) {
                return Err(format!("duplicate nodes[].id {:?}", node.id));
            }
        }
        Ok(())
    }

    pub fn tmux_socket(&self) -> Option<String> {
        let s = self.tmux.socket_name.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    pub fn tmux_bin(&self) -> String {
        non_empty(&self.paths.tmux).unwrap_or_else(|| "tmux".into())
    }

    pub fn zellij_bin(&self) -> String {
        non_empty(&self.paths.zellij).unwrap_or_else(|| "zellij".into())
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

/// Project-scoped notification route in `.starfane/team.toml` referencing ONLY logical notifier IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamNotifyRoute {
    pub notifiers: Vec<String>,
    pub node: Option<String>,
    pub agent: Option<String>,
    pub event_kinds: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamConfig {
    pub feature: BTreeMap<StageKind, StageBinding>,
    pub debug: BTreeMap<StageKind, StageBinding>,
    pub options: PipelineOptions,
    pub aliases: BTreeMap<String, String>,
    pub spawn_args: BTreeMap<starfane_proto::AgentKind, Vec<String>>,
    pub notify_routes: Vec<TeamNotifyRoute>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTeam {
    playbooks: RawPlaybooks,
    models: RawModels,
    spawn: RawSpawn,
    notify: Option<toml::Value>,
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
            notify_routes: Vec::new(),
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
        if let Some(notify_val) = raw.notify {
            team.notify_routes = parse_team_notify_routes(&notify_val)?;
        }
        Ok(team)
    }

    pub fn defaults_for(&self, playbook: PlaybookKind) -> &BTreeMap<StageKind, StageBinding> {
        match playbook {
            PlaybookKind::Feature => &self.feature,
            PlaybookKind::Debug => &self.debug,
        }
    }
}

fn reject_secret_keys_in_team_notify(val: &toml::Value) -> Result<(), String> {
    const FORBIDDEN_KEYS: &[&str] = &[
        "token",
        "bot_token",
        "bot_token_env",
        "secret",
        "secret_env",
        "app_secret",
        "webhook",
        "webhook_url",
        "webhook_url_env",
        "api_base_url",
        "url",
        "password",
        "api_key",
        "chat_id",
        "channel",
        "group_id",
        "user_id",
        "mentioned_list",
        "mentioned_mobile_list",
        "thread_id",
        "headers",
        "profile",
    ];
    match val {
        toml::Value::Table(tbl) => {
            for (k, v) in tbl {
                let lower = k.to_ascii_lowercase();
                if FORBIDDEN_KEYS.iter().any(|bad| lower == *bad) {
                    return Err(format!(
                        ".starfane/team.toml [notify] must not contain credential/channel key {k:?}; define channels in ~/.config/starfane/config.toml [[notifiers]] and reference logical ids only"
                    ));
                }
                reject_secret_keys_in_team_notify(v)?;
            }
        }
        toml::Value::Array(arr) => {
            for item in arr {
                reject_secret_keys_in_team_notify(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_team_notify_routes(val: &toml::Value) -> Result<Vec<TeamNotifyRoute>, String> {
    reject_secret_keys_in_team_notify(val)?;
    let tbl = val
        .as_table()
        .ok_or_else(|| "[notify] must be a TOML table".to_string())?;
    let mut routes = Vec::new();
    // Shorthand: `[notify] notifiers = ["feishu-work"]`
    if let Some(arr) = tbl.get("notifiers") {
        let ids: Vec<String> = arr
            .clone()
            .try_into()
            .map_err(|e| format!("[notify].notifiers: {e}"))?;
        if !ids.is_empty() {
            routes.push(TeamNotifyRoute {
                notifiers: ids,
                node: None,
                agent: None,
                event_kinds: Vec::new(),
            });
        }
    }
    if let Some(raw_routes) = tbl.get("routes") {
        let parsed: Vec<TeamNotifyRoute> = raw_routes
            .clone()
            .try_into()
            .map_err(|e| format!("[notify].routes: {e}"))?;
        routes.extend(parsed);
    }
    Ok(routes)
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
        assert_eq!(def.port, DEFAULT_WEB_PORT);
        assert_eq!(def.lang, DEFAULT_WEB_LANG);
        assert_eq!(def.effective_lang(), "zh");
        assert!(!def.is_en());
        assert!(def.auth_enabled);
        assert!(def.allow_easytier);
        assert_eq!(def.easytier_cidrs, vec![DEFAULT_EASYTIER_CIDR]);
        assert!(!def.allow_tailscale);

        let en_cfg: StarfaneConfig = toml::from_str("[web]\nlang = \"en-US\"\n").unwrap();
        en_cfg.validate().unwrap();
        assert_eq!(en_cfg.web.effective_lang(), "en");
        assert!(en_cfg.web.is_en());
        let zh_cfg: StarfaneConfig = toml::from_str("[web]\nlang = \"zh-CN\"\n").unwrap();
        zh_cfg.validate().unwrap();
        assert_eq!(zh_cfg.web.effective_lang(), "zh");
        let bad_lang: StarfaneConfig = toml::from_str("[web]\nlang = \"fr\"\n").unwrap();
        assert!(bad_lang.validate().is_err());

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

    #[test]
    fn readme_toml_config_examples_parse_and_validate() {
        fn extract_first_toml_block(md: &str) -> String {
            let start = md.find("```toml\n").expect("```toml fence in README") + "```toml\n".len();
            let rest = &md[start..];
            let end = rest.find("```").expect("closing fence in README");
            rest[..end].to_string()
        }

        fn uncomment_examples(block: &str) -> String {
            block
                .lines()
                .map(|line| {
                    if let Some(rest) = line.strip_prefix("# ") {
                        let t = rest.trim_start();
                        if t.starts_with('[')
                            || t.split_once('=').is_some_and(|(k, _)| {
                                let k = k.trim();
                                !k.is_empty()
                                    && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                            })
                        {
                            return rest;
                        }
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !(repo_root.join("Cargo.toml").exists()
            && repo_root.join("crates/starfane-daemon/Cargo.toml").exists())
        {
            return;
        }
        let readme_en =
            std::fs::read_to_string(repo_root.join("README.md")).expect("read README.md");
        let readme_zh = std::fs::read_to_string(repo_root.join("README.zh-CN.md"))
            .expect("read README.zh-CN.md");
        for (name, md) in [
            ("README.md", readme_en.as_str()),
            ("README.zh-CN.md", readme_zh.as_str()),
        ] {
            let active = extract_first_toml_block(md);
            let cfg_active: StarfaneConfig =
                toml::from_str(&active).unwrap_or_else(|e| panic!("{name} active TOML: {e}"));
            cfg_active
                .validate()
                .unwrap_or_else(|e| panic!("{name} active validate: {e}"));
            assert!(cfg_active.zellij.enabled, "{name} zellij default enabled");
            assert!(
                cfg_active.notifiers.is_empty(),
                "{name} fresh config has zero active notifiers"
            );

            let full = uncomment_examples(&active);
            let cfg_full: StarfaneConfig =
                toml::from_str(&full).unwrap_or_else(|e| panic!("{name} full TOML: {e}\n{full}"));
            cfg_full
                .validate()
                .unwrap_or_else(|e| panic!("{name} full validate: {e}"));
            assert_eq!(cfg_full.web.scoped_tokens.len(), 1, "{name} scoped_tokens");
            assert!(cfg_full.web.scoped_tokens[0].read_only);
            assert_eq!(cfg_full.notifiers.len(), 9, "{name} notifiers");
            let kinds: Vec<NotifierKind> = cfg_full.notifiers.iter().map(|n| n.kind).collect();
            assert_eq!(
                kinds,
                vec![
                    NotifierKind::Feishu,
                    NotifierKind::Telegram,
                    NotifierKind::Discord,
                    NotifierKind::Slack,
                    NotifierKind::Slack,
                    NotifierKind::Wecom,
                    NotifierKind::Qq,
                    NotifierKind::Webhook,
                    NotifierKind::Macos,
                ],
                "{name} notifier kinds"
            );
            assert_eq!(
                cfg_full.notifiers[1].telegram_thread_id().unwrap(),
                Some(42),
                "{name} telegram thread_id"
            );
            assert_eq!(cfg_full.notify_routes.len(), 1, "{name} notify_routes");
            assert_eq!(cfg_full.nodes.len(), 2, "{name} nodes");
            assert_eq!(cfg_full.nodes[0].kind, NodeKindConfig::Ssh);
            assert_eq!(cfg_full.nodes[1].kind, NodeKindConfig::Peer);
            assert_eq!(cfg_full.nodes[1].endpoints.len(), 2);
        }

        // Verify documented `.starfane/team.toml` `[notify]` example parses cleanly.
        let team = TeamConfig::parse("[notify]\nnotifiers = [\"feishu-work\"]\n").unwrap();
        assert_eq!(team.notify_routes.len(), 1);
        assert_eq!(team.notify_routes[0].notifiers, vec!["feishu-work"]);
    }

    #[test]
    fn notifier_config_parsing_and_validation() {
        // Valid Telegram (integer thread_id + custom api_base_url), Discord, Slack webhook, Slack bot, Webhook, macOS.
        let ok: StarfaneConfig = toml::from_str(
            r#"
[[notifiers]]
id = "tg-main"
kind = "telegram"
bot_token_env = "TG_TOKEN"
chat_id = "-100123456"
thread_id = 99
api_base_url = "https://tg-proxy.example.com"

[[notifiers]]
id = "discord-ops"
kind = "discord"
webhook_url_env = "DISCORD_HOOK"
username = "starfane-bot"
thread_id = "1234567890123456789"

[[notifiers]]
id = "slack-hook"
kind = "slack"
webhook_url_env = "SLACK_HOOK"

[[notifiers]]
id = "slack-bot"
kind = "slack"
bot_token_env = "SLACK_BOT_TOKEN"
channel = "C01234567"
api_base_url = "https://slack-proxy.example.com/api"

[[notifiers]]
id = "wecom-ops"
kind = "wecom"
webhook_url_env = "WECOM_HOOK"
mentioned_list = ["alice", "@all"]
mentioned_mobile_list = ["13800001111"]

[[notifiers]]
id = "qq-onebot"
kind = "qq"
api_base_url = "http://127.0.0.1:3000"
group_id = 123456789
user_id = "987654321"
bot_token_env = "QQ_ONEBOT_TOKEN"

[[notifiers]]
id = "qq-hook"
kind = "qq"
webhook_url_env = "QQ_WEBHOOK"

[[notifiers]]
id = "hook-custom"
kind = "webhook"
url = "https://hooks.example.com/events"
secret_env = "HOOK_SECRET"
[notifiers.headers]
X-Starfane-Env = "prod"

[[notifiers]]
id = "mac-local"
kind = "macos"
sound = "Glass"

[[notifiers]]
id = "disabled-incomplete"
kind = "discord"
enabled = false
"#,
        )
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(ok.notifiers[0].telegram_thread_id().unwrap(), Some(99));
        assert_eq!(
            ok.notifiers[1].thread_id.as_deref(),
            Some("1234567890123456789")
        );
        assert_eq!(ok.notifiers[4].kind, NotifierKind::Wecom);
        assert_eq!(ok.notifiers[4].mentioned_list, vec!["alice", "@all"]);
        assert_eq!(ok.notifiers[5].kind, NotifierKind::Qq);
        assert_eq!(ok.notifiers[5].group_id.as_deref(), Some("123456789"));
        assert_eq!(ok.notifiers[5].user_id.as_deref(), Some("987654321"));
        assert_eq!(ok.notifiers[6].kind, NotifierKind::Qq);
        assert_eq!(ok.notifiers[7].headers["X-Starfane-Env"], "prod");

        // String thread_id also works for Telegram.
        let tg_str: StarfaneConfig = toml::from_str(
            r#"
[[notifiers]]
id = "tg-str"
kind = "telegram"
bot_token = "123:abc"
chat_id = "42"
thread_id = "77"
"#,
        )
        .unwrap();
        tg_str.validate().unwrap();
        assert_eq!(tg_str.notifiers[0].telegram_thread_id().unwrap(), Some(77));

        // Invalid cases must fail loudly (never silently accepted).
        let bad_cases = [
            // Telegram missing token
            r#"[[notifiers]]
id = "tg-bad"
kind = "telegram"
chat_id = "123"
"#,
            // Telegram missing chat_id
            r#"[[notifiers]]
id = "tg-bad"
kind = "telegram"
bot_token_env = "TG_TOKEN"
"#,
            // Telegram non-positive thread_id
            r#"[[notifiers]]
id = "tg-bad"
kind = "telegram"
bot_token_env = "TG_TOKEN"
chat_id = "123"
thread_id = 0
"#,
            // Telegram non-numeric thread_id
            r#"[[notifiers]]
id = "tg-bad"
kind = "telegram"
bot_token_env = "TG_TOKEN"
chat_id = "123"
thread_id = "not-a-number"
"#,
            // Telegram invalid api_base_url scheme
            r#"[[notifiers]]
id = "tg-bad"
kind = "telegram"
bot_token_env = "TG_TOKEN"
chat_id = "123"
api_base_url = "ftp://example.com"
"#,
            // Discord missing webhook_url / webhook_url_env
            r#"[[notifiers]]
id = "dc-bad"
kind = "discord"
"#,
            // Discord invalid thread_id query injection
            r#"[[notifiers]]
id = "dc-bad"
kind = "discord"
webhook_url = "https://discord.com/api/webhooks/1/tok"
thread_id = "123&wait=true"
"#,
            // Discord username > 80 chars
            r#"[[notifiers]]
id = "dc-bad"
kind = "discord"
webhook_url = "https://discord.com/api/webhooks/1/tok"
username = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
            // Slack missing both modes
            r#"[[notifiers]]
id = "sl-bad"
kind = "slack"
"#,
            // Slack configuring BOTH webhook and bot token
            r#"[[notifiers]]
id = "sl-bad"
kind = "slack"
webhook_url_env = "SLACK_HOOK"
bot_token_env = "SLACK_BOT"
channel = "C123"
"#,
            // Slack bot mode missing channel
            r#"[[notifiers]]
id = "sl-bad"
kind = "slack"
bot_token_env = "SLACK_BOT"
"#,
            // Slack webhook mode with api_base_url
            r#"[[notifiers]]
id = "sl-bad"
kind = "slack"
webhook_url = "https://hooks.slack.com/services/T/B/X"
api_base_url = "https://slack.com/api"
"#,
            // WeCom missing webhook_url / webhook_url_env
            r#"[[notifiers]]
id = "wc-bad"
kind = "wecom"
"#,
            // WeCom empty mentioned_list entry
            r#"[[notifiers]]
id = "wc-bad"
kind = "wecom"
webhook_url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=test"
mentioned_list = ["   "]
"#,
            // QQ missing both OneBot api_base_url and webhook_url
            r#"[[notifiers]]
id = "qq-bad"
kind = "qq"
"#,
            // QQ OneBot mode missing group_id / user_id
            r#"[[notifiers]]
id = "qq-bad"
kind = "qq"
api_base_url = "http://127.0.0.1:3000"
"#,
            // QQ OneBot mode invalid non-numeric group_id
            r#"[[notifiers]]
id = "qq-bad"
kind = "qq"
api_base_url = "http://127.0.0.1:3000"
group_id = "not-numeric"
"#,
            // QQ ambiguous both webhook and OneBot api_base_url
            r#"[[notifiers]]
id = "qq-bad"
kind = "qq"
api_base_url = "http://127.0.0.1:3000"
group_id = "12345"
webhook_url = "https://qq-gateway.example.com/hook"
"#,
            // Webhook invalid header name
            r#"[[notifiers]]
id = "wh-bad"
kind = "webhook"
url = "https://hooks.example.com/ev"
[notifiers.headers]
"Bad:Header" = "v"
"#,
        ];
        for (idx, src) in bad_cases.iter().enumerate() {
            let cfg: StarfaneConfig =
                toml::from_str(src).unwrap_or_else(|e| panic!("case {idx} should parse TOML: {e}"));
            assert!(
                cfg.validate().is_err(),
                "case {idx} should fail validate():\n{src}"
            );
        }

        // team.toml must reject all new secret/endpoint keys.
        for key in [
            "webhook_url",
            "webhook_url_env",
            "bot_token_env",
            "api_base_url",
            "channel",
            "group_id",
            "user_id",
            "thread_id",
        ] {
            let src = format!("[notify]\nnotifiers = [\"ops\"]\n{key} = \"bad\"\n");
            let err = TeamConfig::parse(&src).unwrap_err();
            assert!(err.contains(key), "expected {key} in error: {err}");
        }
    }

    #[test]
    fn ssh_node_config_defaults_and_overrides() {
        // Legacy SSH node TOML omitting all new fields remains 100% valid with sane defaults.
        let legacy: StarfaneConfig = toml::from_str(
            r#"
[[nodes]]
id = "gpu-a"
kind = "ssh"
ssh_target = "ubuntu@gpu-a"
"#,
        )
        .unwrap();
        legacy.validate().unwrap();
        let node = &legacy.nodes[0];
        assert_eq!(node.ssh_bin, "");
        assert_eq!(node.ssh_bin_override(), None);
        assert_eq!(node.connect_timeout_secs, DEFAULT_SSH_CONNECT_TIMEOUT_SECS);
        assert!(node.compression);
        assert!(node.push_watch);
        assert!(!node.include_shell_panes);
        assert_eq!(node.agent_bin(starfane_proto::AgentKind::Claude), "claude");
        assert_eq!(node.agent_bin(starfane_proto::AgentKind::Codex), "codex");
        assert_eq!(node.agent_bin(starfane_proto::AgentKind::Agy), "agy");

        // Explicit overrides parse and validate cleanly (including push_watch=false and ssh_peer alias).
        let custom: StarfaneConfig = toml::from_str(
            r#"
[[nodes]]
id = "gpu-b"
kind = "ssh"
ssh_target = "ubuntu@gpu-b"
ssh_bin = "/opt/bin/fake-ssh"
connect_timeout_secs = 12
compression = false
push_watch = false
include_shell_panes = true
claude_bin = "/home/ubuntu/.local/bin/claude"
codex_bin = "/home/ubuntu/.npm-global/bin/codex"
agy_bin = "/home/ubuntu/.local/bin/agy"

[[nodes]]
id = "mac-tun"
kind = "ssh_peer"
ssh_target = "mac-xiling"
"#,
        )
        .unwrap();
        custom.validate().unwrap();
        let n = &custom.nodes[0];
        assert_eq!(n.ssh_bin_override().as_deref(), Some("/opt/bin/fake-ssh"));
        assert_eq!(n.connect_timeout_secs, 12);
        assert!(!n.compression);
        assert!(!n.push_watch);
        assert!(n.include_shell_panes);
        assert_eq!(
            n.agent_bin(starfane_proto::AgentKind::Claude),
            "/home/ubuntu/.local/bin/claude"
        );
        assert_eq!(
            n.agent_bin(starfane_proto::AgentKind::Codex),
            "/home/ubuntu/.npm-global/bin/codex"
        );
        assert_eq!(
            n.agent_bin(starfane_proto::AgentKind::Agy),
            "/home/ubuntu/.local/bin/agy"
        );
        let tun = &custom.nodes[1];
        assert_eq!(tun.kind, NodeKindConfig::Peer);
        assert_eq!(tun.ssh_target, "mac-xiling");
        assert!(tun.endpoints.is_empty());
        assert!(tun.push_watch);

        // Zero connect_timeout_secs fails validation.
        let bad: StarfaneConfig = toml::from_str(
            r#"
[[nodes]]
id = "gpu-bad"
kind = "ssh"
ssh_target = "ubuntu@gpu-bad"
connect_timeout_secs = 0
"#,
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }
}
