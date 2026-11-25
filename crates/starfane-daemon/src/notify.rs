//! Opt-in pluggable notification channels (`Notifier`) and differential router (`NotifyRouter`).
//!
//! Design invariants:
//! - **Opt-in by default**: fresh open-source installs have zero notifiers configured and remain silent.
//! - **100% backward compatible**: legacy `[feishu] enabled = true` auto-synthesizes a `"feishu-default"`
//!   notifier handled by the interactive Card 2.0 `lark` adapter.
//! - **Split Ownership**: `node_local` notifiers only deliver for sessions owned locally by this daemon
//!   (never re-notifying remote `Peer` daemon sessions), keeping work credentials isolated on the work host.
//! - **Project-level routing without secrets**: `.starfane/team.toml` `[notify]` references ONLY logical
//!   notifier IDs resolved against `~/.config/starfane/config.toml` `[[notifiers]]`.

use crate::config::{
    NotifierConfig, NotifierKind, NotifyOwnership, NotifyRouteConfig, StarfaneConfig,
    TeamNotifyRoute,
};
use crate::notice::{BoxFuture, Notice, NoticeKind};
use serde::Serialize;
use starfane_proto::{AgentKind, ApiError, LOCAL_HOST_ID};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const DEFAULT_FEISHU_NOTIFIER_ID: &str = "feishu-default";
pub const DEFAULT_TELEGRAM_API_BASE: &str = "https://api.telegram.org";
pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
pub const DEFAULT_DISCORD_USERNAME: &str = "starfane";

const TELEGRAM_MAX_TEXT_CHARS: usize = 4096;
const DISCORD_CONTENT_MAX_CHARS: usize = 2000;
const DISCORD_EMBED_TITLE_MAX_CHARS: usize = 256;
const DISCORD_EMBED_DESC_MAX_CHARS: usize = 4096;
const DISCORD_EMBED_FIELD_VALUE_MAX_CHARS: usize = 1024;
const DISCORD_EMBED_FOOTER_MAX_CHARS: usize = 2048;
const DISCORD_EMBED_TOTAL_MAX_CHARS: usize = 6000;
const SLACK_HEADER_MAX_CHARS: usize = 150;
const SLACK_SECTION_MAX_CHARS: usize = 3000;
const SLACK_FALLBACK_TEXT_MAX_CHARS: usize = 3000;
const WECOM_MARKDOWN_MAX_BYTES: usize = 4096;
const WECOM_TEXT_MAX_BYTES: usize = 2048;
const QQ_MAX_TEXT_CHARS: usize = 4500;
const ERROR_BODY_PREVIEW_MAX_CHARS: usize = 300;

/// Public secret-free summary of a configured notifier returned by `GET /api/notifiers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotifierSummary {
    pub id: String,
    pub kind: NotifierKind,
    pub ownership: NotifyOwnership,
}

/// Pluggable outbound notification channel (Feishu profile, Telegram, Discord, Slack, Webhook, macOS native).
pub trait Notifier: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> NotifierKind;
    fn ownership(&self) -> NotifyOwnership;
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>>;
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>>;
}

/// Context evaluated against `.starfane/team.toml` and `[[notify_routes]]` rules.
#[derive(Debug, Clone)]
pub struct NotifyRouteContext {
    pub node: String,
    pub is_local_host: bool,
    /// Notification ownership of the session's host (`NodeLocal` for local & peer daemons by default;
    /// `Hub` for zero-install SSH hosts or peers explicitly delegated to the hub).
    pub node_ownership: NotifyOwnership,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<AgentKind>,
    pub event_kind: NoticeKind,
}

impl NotifyRouteContext {
    pub fn from_notice(
        notice: &Notice,
        node: String,
        is_local_host: bool,
        node_ownership: NotifyOwnership,
        cwd: Option<String>,
        git_root: Option<String>,
        agent: Option<AgentKind>,
    ) -> Self {
        let project = git_root
            .as_deref()
            .or(cwd.as_deref())
            .and_then(|p| Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty());
        Self {
            node,
            is_local_host,
            node_ownership,
            project,
            cwd,
            agent,
            event_kind: notice.kind,
        }
    }
}

/// Compiled notification router holding all opt-in secondary notifiers + routing table.
pub struct NotifyRouter {
    notifiers: HashMap<String, Arc<dyn Notifier>>,
    kind_by_id: HashMap<String, NotifierKind>,
    ownership_by_id: HashMap<String, NotifyOwnership>,
    routes: Vec<NotifyRouteConfig>,
    has_legacy_feishu: bool,
}

impl NotifyRouter {
    pub fn from_config(cfg: &StarfaneConfig) -> Self {
        let mut notifiers: HashMap<String, Arc<dyn Notifier>> = HashMap::new();
        let mut kind_by_id: HashMap<String, NotifierKind> = HashMap::new();
        let mut ownership_by_id: HashMap<String, NotifyOwnership> = HashMap::new();
        let mut routes = cfg.notify_routes.clone();

        let has_legacy_feishu = cfg.feishu.enabled;

        for nc in &cfg.notifiers {
            if !nc.enabled {
                continue;
            }
            let id = nc.id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            // When legacy `[feishu] enabled = true` owns `feishu-default` (without extra profile),
            // let `lark.rs` deliver interactive Card 2.0; otherwise register as a pluggable notifier.
            if id == DEFAULT_FEISHU_NOTIFIER_ID
                && nc.kind == NotifierKind::Feishu
                && nc.profile.trim().is_empty()
                && has_legacy_feishu
            {
                kind_by_id.insert(id.clone(), NotifierKind::Feishu);
                ownership_by_id.insert(id, nc.ownership);
                continue;
            }
            match build_notifier(nc) {
                Ok(n) => {
                    kind_by_id.insert(id.clone(), nc.kind);
                    ownership_by_id.insert(id.clone(), nc.ownership);
                    notifiers.insert(id, n);
                }
                Err(reason) => {
                    log::warn!("skipping notifier {id:?} ({}): {reason}", nc.kind.as_str());
                }
            }
        }

        // 100% backward compatibility: `[feishu] enabled = true` synthesizes `feishu-default`.
        if has_legacy_feishu && !ownership_by_id.contains_key(DEFAULT_FEISHU_NOTIFIER_ID) {
            kind_by_id.insert(DEFAULT_FEISHU_NOTIFIER_ID.to_string(), NotifierKind::Feishu);
            ownership_by_id.insert(
                DEFAULT_FEISHU_NOTIFIER_ID.to_string(),
                NotifyOwnership::NodeLocal,
            );
            if routes.is_empty() {
                routes.push(NotifyRouteConfig {
                    notifiers: vec![DEFAULT_FEISHU_NOTIFIER_ID.to_string()],
                    ..NotifyRouteConfig::default()
                });
            }
        }

        // If the user configured `[[notifiers]]` without explicit `[[notify_routes]]`,
        // route to all enabled notifiers by default so minimal configs work immediately.
        if routes.is_empty() && !ownership_by_id.is_empty() {
            let mut ids: Vec<String> = ownership_by_id.keys().cloned().collect();
            ids.sort();
            routes.push(NotifyRouteConfig {
                notifiers: ids,
                ..NotifyRouteConfig::default()
            });
        }

        Self {
            notifiers,
            kind_by_id,
            ownership_by_id,
            routes,
            has_legacy_feishu,
        }
    }

    /// True if any pluggable non-legacy notifiers are registered.
    pub fn has_pluggable_notifiers(&self) -> bool {
        !self.notifiers.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Notifier>> {
        self.notifiers.get(id).cloned()
    }

    pub fn list_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.ownership_by_id.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Secret-free summaries (`id`, `kind`, `ownership`) sorted by `id`.
    pub fn list_summaries(&self) -> Vec<NotifierSummary> {
        self.list_ids()
            .into_iter()
            .filter_map(|id| {
                let kind = *self.kind_by_id.get(&id)?;
                let ownership = *self.ownership_by_id.get(&id)?;
                Some(NotifierSummary {
                    id,
                    kind,
                    ownership,
                })
            })
            .collect()
    }

    /// Resolve which logical notifier IDs should receive a notification for `ctx`.
    /// Project `.starfane/team.toml` routes take precedence when they match; otherwise global `[[notify_routes]]` apply.
    /// Split Ownership is always enforced: `NodeLocal` notifiers never fire for remote `Peer` sessions.
    pub fn resolve_targets(
        &self,
        ctx: &NotifyRouteContext,
        team_routes: &[TeamNotifyRoute],
    ) -> Vec<String> {
        let mut chosen = Vec::new();
        let mut seen = HashSet::new();

        // 1. Project-level `.starfane/team.toml` `[notify]` routes (if any match).
        for r in team_routes {
            if matches_team_route(r, ctx) {
                for id in &r.notifiers {
                    if self.allows_notifier_for_ctx(id, None, ctx) && seen.insert(id.clone()) {
                        chosen.push(id.clone());
                    }
                }
            }
        }
        if !chosen.is_empty() {
            return chosen;
        }

        // 2. Global `~/.config/starfane/config.toml` `[[notify_routes]]`.
        for r in &self.routes {
            if matches_global_route(r, ctx) {
                for id in &r.notifiers {
                    if self.allows_notifier_for_ctx(id, r.ownership, ctx) && seen.insert(id.clone())
                    {
                        chosen.push(id.clone());
                    }
                }
            }
        }
        chosen
    }

    /// Whether the primary Card 2.0 `lark.rs` loop should deliver this notice.
    pub fn should_deliver_primary_feishu(
        &self,
        ctx: &NotifyRouteContext,
        team_routes: &[TeamNotifyRoute],
    ) -> bool {
        if !self.has_legacy_feishu {
            return false;
        }
        self.resolve_targets(ctx, team_routes)
            .iter()
            .any(|id| id == DEFAULT_FEISHU_NOTIFIER_ID)
    }

    /// Fan out `notice` concurrently to all resolved pluggable notifiers (excluding `feishu-default`,
    /// which `lark.rs` delivers), isolating per-channel failures so one broken channel never blocks
    /// healthy ones. Returns per-channel delivery outcomes in resolved target order.
    pub async fn dispatch_pluggable_report(
        &self,
        notice: &Notice,
        ctx: &NotifyRouteContext,
        team_routes: &[TeamNotifyRoute],
    ) -> Vec<(String, Result<Option<String>, ApiError>)> {
        let targets = self.resolve_targets(ctx, team_routes);
        let mut handles = Vec::new();
        for id in targets {
            if id == DEFAULT_FEISHU_NOTIFIER_ID && self.has_legacy_feishu {
                continue;
            }
            let Some(notifier) = self.notifiers.get(&id).cloned() else {
                continue;
            };
            let n_clone = notice.clone();
            let id_clone = id.clone();
            let handle = tokio::spawn(async move {
                let res = notifier.send(&n_clone).await;
                if let Err(ref e) = res {
                    log::warn!("notifier {id_clone:?} send failed: {e}");
                }
                (id_clone, res)
            });
            handles.push((id, handle));
        }

        let mut results = Vec::with_capacity(handles.len());
        for (id, handle) in handles {
            match handle.await {
                Ok(pair) => results.push(pair),
                Err(join_err) => {
                    let err = ApiError::internal(format!("notifier task panicked: {join_err}"));
                    log::warn!("notifier {id:?} task join error: {err}");
                    results.push((id, Err(err)));
                }
            }
        }
        results
    }

    /// Fan out `notice` to all resolved pluggable notifiers (excluding `feishu-default`, which `lark.rs` delivers).
    pub async fn dispatch_pluggable(
        &self,
        notice: &Notice,
        ctx: &NotifyRouteContext,
        team_routes: &[TeamNotifyRoute],
    ) {
        let _ = self
            .dispatch_pluggable_report(notice, ctx, team_routes)
            .await;
    }

    fn allows_notifier_for_ctx(
        &self,
        id: &str,
        route_ownership: Option<NotifyOwnership>,
        ctx: &NotifyRouteContext,
    ) -> bool {
        let Some(&channel_ownership) = self.ownership_by_id.get(id) else {
            return false;
        };
        // Split Ownership: a remote Peer host with `node_ownership == NodeLocal` owns and delivers
        // its own notifications locally; the Hub must NEVER double-notify it.
        if !ctx.is_local_host && ctx.node_ownership == NotifyOwnership::NodeLocal {
            return false;
        }
        let effective = route_ownership.unwrap_or(channel_ownership);
        match effective {
            NotifyOwnership::NodeLocal => ctx.is_local_host,
            NotifyOwnership::Hub => true,
        }
    }
}

fn notice_kind_name(k: NoticeKind) -> &'static str {
    match k {
        NoticeKind::BindingConfirm => "binding_confirm",
        NoticeKind::PlanApproval => "plan_approval",
        NoticeKind::Blocked => "blocked",
        NoticeKind::Error => "error",
        NoticeKind::TurnComplete => "turn_complete",
        NoticeKind::TriageReady => "triage_ready",
        NoticeKind::ReviewResult => "review_result",
        NoticeKind::FinalReport => "final_report",
        NoticeKind::Progress => "progress",
        NoticeKind::DeliveryUnconfirmed => "delivery_unconfirmed",
    }
}

fn matches_event_kinds(allowed: &[String], kind: NoticeKind) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let name = notice_kind_name(kind);
    allowed
        .iter()
        .any(|k| k.trim().eq_ignore_ascii_case(name) || k.trim() == "*")
}

fn matches_agent(expected: Option<&str>, actual: Option<AgentKind>) -> bool {
    let Some(exp) = expected
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
    else {
        return true;
    };
    actual.is_some_and(|a| a.as_str().eq_ignore_ascii_case(exp))
}

fn matches_node(expected: Option<&str>, actual: &str) -> bool {
    let Some(exp) = expected
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
    else {
        return true;
    };
    exp.eq_ignore_ascii_case(actual)
}

fn matches_team_route(r: &TeamNotifyRoute, ctx: &NotifyRouteContext) -> bool {
    matches_node(r.node.as_deref(), &ctx.node)
        && matches_agent(r.agent.as_deref(), ctx.agent)
        && matches_event_kinds(&r.event_kinds, ctx.event_kind)
}

fn matches_global_route(r: &NotifyRouteConfig, ctx: &NotifyRouteContext) -> bool {
    if !matches_node(r.node.as_deref(), &ctx.node) {
        return false;
    }
    if !matches_agent(r.agent.as_deref(), ctx.agent) {
        return false;
    }
    if !matches_event_kinds(&r.event_kinds, ctx.event_kind) {
        return false;
    }
    if let Some(proj) = r
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
    {
        if !ctx
            .project
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(proj))
        {
            return false;
        }
    }
    if let Some(cwd_prefix) = r.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !ctx
            .cwd
            .as_deref()
            .is_some_and(|c| Path::new(c).starts_with(Path::new(cwd_prefix)))
        {
            return false;
        }
    }
    true
}

fn build_notifier(cfg: &NotifierConfig) -> Result<Arc<dyn Notifier>, String> {
    let id = cfg.id.trim().to_string();
    match cfg.kind {
        NotifierKind::Feishu => Ok(Arc::new(FeishuCliNotifier {
            id,
            ownership: cfg.ownership,
            lark_cli: if cfg.lark_cli.trim().is_empty() {
                "lark-cli".into()
            } else {
                cfg.lark_cli.trim().to_string()
            },
            profile: cfg.profile.trim().to_string(),
            chat_id: cfg.chat_id.trim().to_string(),
        })),
        NotifierKind::Telegram => {
            let token = resolve_secret(&cfg.bot_token, &cfg.bot_token_env).ok_or_else(|| {
                if !cfg.bot_token_env.trim().is_empty() {
                    format!("env var {:?} is unset or empty", cfg.bot_token_env.trim())
                } else {
                    "missing bot_token / bot_token_env".to_string()
                }
            })?;
            let chat_id = cfg.chat_id.trim();
            if chat_id.is_empty() {
                return Err("missing chat_id".to_string());
            }
            let thread_id = cfg.telegram_thread_id()?;
            let api_base_url = cfg
                .api_base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_TELEGRAM_API_BASE)
                .trim_end_matches('/')
                .to_string();
            ensure_http_url("api_base_url", &api_base_url)?;
            Ok(Arc::new(TelegramNotifier {
                id,
                ownership: cfg.ownership,
                bot_token: token,
                chat_id: chat_id.to_string(),
                thread_id,
                api_base_url,
            }))
        }
        NotifierKind::Discord => {
            let webhook_url =
                resolve_opt_secret(cfg.webhook_url.as_deref(), cfg.webhook_url_env.as_deref())
                    .ok_or_else(|| {
                        if let Some(env_k) = cfg
                            .webhook_url_env
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            format!("env var {env_k:?} is unset or empty")
                        } else {
                            "missing webhook_url / webhook_url_env".to_string()
                        }
                    })?;
            ensure_http_url("webhook_url", &webhook_url)?;
            let username = cfg
                .username
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_DISCORD_USERNAME)
                .to_string();
            let thread_id = cfg
                .thread_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Ok(Arc::new(DiscordNotifier {
                id,
                ownership: cfg.ownership,
                webhook_url,
                username,
                thread_id,
            }))
        }
        NotifierKind::Slack => {
            let has_webhook_cfg = cfg
                .webhook_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
                || cfg
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
            let has_bot_cfg =
                !cfg.bot_token.trim().is_empty() || !cfg.bot_token_env.trim().is_empty();
            if has_webhook_cfg && has_bot_cfg {
                return Err(
                    "ambiguous config: specify either webhook_url/webhook_url_env or bot_token/bot_token_env"
                        .to_string(),
                );
            }
            if has_webhook_cfg {
                let webhook_url =
                    resolve_opt_secret(cfg.webhook_url.as_deref(), cfg.webhook_url_env.as_deref())
                        .ok_or_else(|| {
                            if let Some(env_k) = cfg
                                .webhook_url_env
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                format!("env var {env_k:?} is unset or empty")
                            } else {
                                "missing webhook_url / webhook_url_env".to_string()
                            }
                        })?;
                ensure_http_url("webhook_url", &webhook_url)?;
                return Ok(Arc::new(SlackNotifier {
                    id,
                    ownership: cfg.ownership,
                    transport: SlackTransport::IncomingWebhook { webhook_url },
                }));
            }
            if has_bot_cfg {
                let bot_token =
                    resolve_secret(&cfg.bot_token, &cfg.bot_token_env).ok_or_else(|| {
                        if !cfg.bot_token_env.trim().is_empty() {
                            format!("env var {:?} is unset or empty", cfg.bot_token_env.trim())
                        } else {
                            "missing bot_token / bot_token_env".to_string()
                        }
                    })?;
                let channel = cfg
                    .channel
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "Slack Bot API mode requires non-empty channel".to_string())?
                    .to_string();
                let api_base_url = cfg
                    .api_base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(DEFAULT_SLACK_API_BASE)
                    .trim_end_matches('/')
                    .to_string();
                ensure_http_url("api_base_url", &api_base_url)?;
                return Ok(Arc::new(SlackNotifier {
                    id,
                    ownership: cfg.ownership,
                    transport: SlackTransport::BotApi {
                        bot_token,
                        channel,
                        api_base_url,
                    },
                }));
            }
            Err(
                "requires either webhook_url/webhook_url_env or bot_token/bot_token_env + channel"
                    .to_string(),
            )
        }
        NotifierKind::Wecom => {
            let webhook_url =
                resolve_opt_secret(cfg.webhook_url.as_deref(), cfg.webhook_url_env.as_deref())
                    .ok_or_else(|| {
                        if let Some(env_k) = cfg
                            .webhook_url_env
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            format!("env var {env_k:?} is unset or empty")
                        } else {
                            "missing webhook_url / webhook_url_env".to_string()
                        }
                    })?;
            ensure_http_url("webhook_url", &webhook_url)?;
            let mentioned_list = cfg
                .mentioned_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let mentioned_mobile_list = cfg
                .mentioned_mobile_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Ok(Arc::new(WecomNotifier {
                id,
                ownership: cfg.ownership,
                webhook_url,
                mentioned_list,
                mentioned_mobile_list,
            }))
        }
        NotifierKind::Qq => {
            let has_webhook_cfg = cfg
                .webhook_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
                || cfg
                    .webhook_url_env
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
            let has_api_base = cfg
                .api_base_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            if has_webhook_cfg && has_api_base {
                return Err(
                    "ambiguous config: specify either api_base_url (OneBot v11) or webhook_url/webhook_url_env"
                        .to_string(),
                );
            }
            if has_api_base {
                let api_base_url = cfg
                    .api_base_url
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches('/')
                    .to_string();
                ensure_http_url("api_base_url", &api_base_url)?;
                let group_id = cfg.parse_positive_u64_field("group_id", cfg.group_id.as_deref())?;
                let user_id = cfg.parse_positive_u64_field("user_id", cfg.user_id.as_deref())?;
                if group_id.is_none() && user_id.is_none() {
                    return Err("QQ OneBot v11 mode requires group_id and/or user_id".to_string());
                }
                let access_token = resolve_secret(&cfg.bot_token, &cfg.bot_token_env);
                if access_token.is_none() && !cfg.bot_token_env.trim().is_empty() {
                    return Err(format!(
                        "env var {:?} is unset or empty",
                        cfg.bot_token_env.trim()
                    ));
                }
                return Ok(Arc::new(QqNotifier {
                    id,
                    ownership: cfg.ownership,
                    transport: QqTransport::OneBotV11 {
                        api_base_url,
                        group_id,
                        user_id,
                        access_token,
                    },
                }));
            }
            if has_webhook_cfg {
                let webhook_url =
                    resolve_opt_secret(cfg.webhook_url.as_deref(), cfg.webhook_url_env.as_deref())
                        .ok_or_else(|| {
                            if let Some(env_k) = cfg
                                .webhook_url_env
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                format!("env var {env_k:?} is unset or empty")
                            } else {
                                "missing webhook_url / webhook_url_env".to_string()
                            }
                        })?;
                ensure_http_url("webhook_url", &webhook_url)?;
                let secret = resolve_secret(&cfg.bot_token, &cfg.bot_token_env)
                    .or_else(|| resolve_secret("", &cfg.secret_env));
                return Ok(Arc::new(QqNotifier {
                    id,
                    ownership: cfg.ownership,
                    transport: QqTransport::Webhook {
                        webhook_url,
                        secret,
                    },
                }));
            }
            Err(
                "requires either api_base_url + group_id/user_id (OneBot v11) or webhook_url/webhook_url_env"
                    .to_string(),
            )
        }
        NotifierKind::Webhook => {
            let url = if !cfg.url.trim().is_empty() {
                cfg.url.trim().to_string()
            } else {
                resolve_opt_secret(cfg.webhook_url.as_deref(), cfg.webhook_url_env.as_deref())
                    .ok_or_else(|| "missing url / webhook_url / webhook_url_env".to_string())?
            };
            ensure_http_url("url", &url)?;
            let secret = if cfg.secret_env.trim().is_empty() {
                None
            } else {
                match std::env::var(cfg.secret_env.trim()) {
                    Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
                    _ => {
                        log::warn!(
                            "webhook notifier {:?}: secret_env {:?} is unset or empty; sending without Bearer header",
                            id,
                            cfg.secret_env.trim()
                        );
                        None
                    }
                }
            };
            Ok(Arc::new(WebhookNotifier {
                id,
                ownership: cfg.ownership,
                url,
                secret,
                headers: cfg.headers.clone(),
            }))
        }
        NotifierKind::Macos => Ok(Arc::new(MacosNotifier {
            id,
            ownership: cfg.ownership,
            sound: cfg.sound.trim().to_string(),
        })),
    }
}

fn ensure_http_url(label: &str, url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "invalid {label}: empty or contains whitespace/control chars"
        ));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(format!(
            "invalid {label}: must start with http:// or https://"
        ));
    }
    Ok(())
}

fn resolve_secret(inline: &str, env_key: &str) -> Option<String> {
    if !inline.trim().is_empty() {
        return Some(inline.trim().to_string());
    }
    if !env_key.trim().is_empty() {
        return std::env::var(env_key.trim())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    None
}

fn resolve_opt_secret(inline: Option<&str>, env_key: Option<&str>) -> Option<String> {
    resolve_secret(inline.unwrap_or(""), env_key.unwrap_or(""))
}

// ---------------------------------------------------------------------------
// Shared formatting & UTF-8 safe helpers
// ---------------------------------------------------------------------------

/// Truncate `s` to at most `max_chars` Unicode scalar values on UTF-8 boundaries,
/// appending `'…'` when truncated (total char count never exceeds `max_chars`).
pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = s.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_none() {
        return taken;
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Truncate `s` to at most `max_bytes` UTF-8 bytes without splitting any Unicode scalar value,
/// appending `'…'` (3 UTF-8 bytes) when truncated (total byte length never exceeds `max_bytes`).
pub(crate) fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    const ELLIPSIS: &str = "…";
    if max_bytes < ELLIPSIS.len() {
        let mut end = 0;
        for (idx, ch) in s.char_indices() {
            if idx + ch.len_utf8() > max_bytes {
                break;
            }
            end = idx + ch.len_utf8();
        }
        return s[..end].to_string();
    }
    let budget = max_bytes - ELLIPSIS.len();
    let mut end = 0;
    for (idx, ch) in s.char_indices() {
        if idx + ch.len_utf8() > budget {
            break;
        }
        end = idx + ch.len_utf8();
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len());
    out.push_str(&s[..end]);
    out.push_str(ELLIPSIS);
    out
}

fn body_preview(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "(empty response body)".to_string()
    } else {
        truncate_chars(trimmed, ERROR_BODY_PREVIEW_MAX_CHARS)
    }
}

struct NoticeMeta {
    event_label: &'static str,
    host: String,
    agent: Option<String>,
    session_id: Option<String>,
    pane: Option<String>,
    pipeline_id: Option<String>,
    review_id: Option<String>,
}

fn extract_host_prefix(id_str: &str) -> Option<&str> {
    let (prefix, rest) = id_str.split_once(':')?;
    if !prefix.is_empty() && prefix != "zj" && !rest.is_empty() {
        Some(prefix)
    } else {
        None
    }
}

fn extract_notice_meta(notice: &Notice) -> NoticeMeta {
    let session_id = notice
        .session_id
        .as_ref()
        .map(|s| s.as_str().to_string())
        .or_else(|| {
            notice
                .reply_session
                .as_ref()
                .map(|r| r.session_id.as_str().to_string())
        });
    let pane_id_str = notice
        .reply_session
        .as_ref()
        .and_then(|r| r.pane_id.as_ref())
        .map(|p| p.as_str());
    let host = pane_id_str
        .and_then(extract_host_prefix)
        .or_else(|| session_id.as_deref().and_then(extract_host_prefix))
        .unwrap_or(LOCAL_HOST_ID)
        .to_string();
    let agent = notice
        .reply_session
        .as_ref()
        .map(|r| r.agent.as_str().to_string());
    let pane = notice.reply_session.as_ref().and_then(|r| {
        r.tmux_label
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| r.pane_id.as_ref().map(|p| p.as_str().to_string()))
    });
    NoticeMeta {
        event_label: notice_kind_name(notice.kind),
        host,
        agent,
        session_id,
        pane,
        pipeline_id: notice.pipeline_id.as_ref().map(|p| p.as_str().to_string()),
        review_id: notice.review_id.as_ref().map(|r| r.as_str().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Concrete Notifiers (zero extra crate bloat)
// ---------------------------------------------------------------------------

struct FeishuCliNotifier {
    id: String,
    ownership: NotifyOwnership,
    lark_cli: String,
    profile: String,
    chat_id: String,
}

impl Notifier for FeishuCliNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Feishu
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let text = format!("{}\n{}", notice.title, notice.body_md);
            let id = self.send_text(&text).await?;
            Ok(Some(id))
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move { self.send_text(text).await })
    }
}

impl FeishuCliNotifier {
    async fn send_text(&self, text: &str) -> Result<String, ApiError> {
        if self.chat_id.is_empty() {
            return Err(ApiError::bad_request(format!(
                "feishu notifier {:?} missing chat_id",
                self.id
            )));
        }
        let mut cmd = Command::new(&self.lark_cli);
        if !self.profile.is_empty() {
            cmd.arg("--profile").arg(&self.profile);
        }
        cmd.args([
            "im",
            "+messages-send",
            "--chat-id",
            &self.chat_id,
            "--text",
            text,
        ]);
        let out = cmd
            .output()
            .await
            .map_err(|e| ApiError::internal(format!("spawn lark-cli: {e}")))?;
        if !out.status.success() {
            return Err(ApiError::internal(format!(
                "lark-cli failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

struct TelegramNotifier {
    id: String,
    ownership: NotifyOwnership,
    bot_token: String,
    chat_id: String,
    thread_id: Option<i64>,
    api_base_url: String,
}

impl Notifier for TelegramNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Telegram
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let text = format_telegram_notice(notice);
            self.send_message(&text).await
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let msg_id = self.send_message(text).await?;
            Ok(msg_id.unwrap_or_else(|| "ok".into()))
        })
    }
}

fn format_telegram_notice(notice: &Notice) -> String {
    let meta = extract_notice_meta(notice);
    let mut parts = vec![format!("Host: {}", meta.host)];
    if let Some(ref agent) = meta.agent {
        parts.push(format!("Agent: {agent}"));
    }
    if let Some(ref sid) = meta.session_id {
        parts.push(format!("Session: {sid}"));
    }
    if let Some(ref pane) = meta.pane {
        parts.push(format!("Pane: {pane}"));
    }
    if let Some(ref pid) = meta.pipeline_id {
        parts.push(format!("Pipeline: {pid}"));
    }

    let mut out = format!("[starfane · {}] {}", meta.event_label, notice.title.trim());
    out.push('\n');
    out.push_str(&parts.join(" | "));
    let body = notice.body_md.trim();
    if !body.is_empty() {
        out.push_str("\n\n");
        out.push_str(body);
    }
    truncate_chars(&out, TELEGRAM_MAX_TEXT_CHARS)
}

impl TelegramNotifier {
    async fn send_message(&self, text: &str) -> Result<Option<String>, ApiError> {
        // Pass URL containing bot token via curl `-K -` stdin so token never appears in `ps` args.
        let url = format!("{}/bot{}/sendMessage", self.api_base_url, self.bot_token);
        let safe_text = truncate_chars(text, TELEGRAM_MAX_TEXT_CHARS);
        let mut obj = serde_json::Map::new();
        obj.insert(
            "chat_id".into(),
            serde_json::Value::String(self.chat_id.clone()),
        );
        obj.insert("text".into(), serde_json::Value::String(safe_text));
        if let Some(tid) = self.thread_id {
            obj.insert("message_thread_id".into(), serde_json::Value::from(tid));
        }
        let payload = serde_json::Value::Object(obj);
        let empty_headers = BTreeMap::new();
        let resp = curl_http_post(&url, &payload, None, &empty_headers).await?;

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&resp.body);
        if !(200..300).contains(&resp.status) {
            let detail = match parsed {
                Ok(ref v) => v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| body_preview(&resp.body)),
                Err(_) => body_preview(&resp.body),
            };
            return Err(ApiError::internal(format!(
                "telegram sendMessage HTTP {}: {detail}",
                resp.status
            )));
        }

        let v = parsed.map_err(|e| {
            ApiError::internal(format!(
                "telegram sendMessage invalid JSON response ({e}): {}",
                body_preview(&resp.body)
            ))
        })?;
        if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown Telegram API error");
            let code_suffix = v
                .get("error_code")
                .and_then(|c| c.as_i64())
                .map(|c| format!(" (code {c})"))
                .unwrap_or_default();
            return Err(ApiError::internal(format!(
                "telegram sendMessage failed{code_suffix}: {desc}"
            )));
        }

        let msg_id = v
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|id| {
                id.as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| id.as_u64().map(|n| n.to_string()))
                    .or_else(|| id.as_str().map(str::to_string))
            });
        Ok(msg_id)
    }
}

struct DiscordNotifier {
    id: String,
    ownership: NotifyOwnership,
    webhook_url: String,
    username: String,
    thread_id: Option<String>,
}

impl Notifier for DiscordNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Discord
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let payload = self.build_embed_payload(notice);
            self.post_webhook(&payload).await
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "username": self.username,
                "content": truncate_chars(text, DISCORD_CONTENT_MAX_CHARS),
            });
            let id = self.post_webhook(&payload).await?;
            Ok(id.unwrap_or_else(|| "ok".into()))
        })
    }
}

pub(crate) fn discord_webhook_target_url(base: &str, thread_id: Option<&str>) -> String {
    let Some(tid) = thread_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return base.to_string();
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}thread_id={tid}")
}

fn discord_severity_color(kind: NoticeKind) -> u32 {
    match kind {
        NoticeKind::Error | NoticeKind::Blocked | NoticeKind::DeliveryUnconfirmed => 0xD93829,
        NoticeKind::PlanApproval | NoticeKind::BindingConfirm | NoticeKind::TriageReady => 0xE59819,
        NoticeKind::TurnComplete | NoticeKind::ReviewResult | NoticeKind::FinalReport => 0x237A4B,
        NoticeKind::Progress => 0x3B6EA5,
    }
}

impl DiscordNotifier {
    fn build_embed_payload(&self, notice: &Notice) -> serde_json::Value {
        let meta = extract_notice_meta(notice);
        let raw_title = if notice.title.trim().is_empty() {
            "Starfane notification"
        } else {
            notice.title.trim()
        };
        let title = truncate_chars(raw_title, DISCORD_EMBED_TITLE_MAX_CHARS);

        let mut field_pairs: Vec<(&'static str, String)> =
            vec![("Host", meta.host), ("Event", meta.event_label.to_string())];
        if let Some(agent) = meta.agent {
            field_pairs.push(("Agent", agent));
        }
        if let Some(sid) = meta.session_id {
            field_pairs.push(("Session", sid));
        }
        if let Some(pane) = meta.pane {
            field_pairs.push(("Pane", pane));
        }
        if let Some(pid) = meta.pipeline_id {
            field_pairs.push(("Pipeline", pid));
        }
        if let Some(rid) = meta.review_id {
            field_pairs.push(("Review", rid));
        }

        let mut fields_json = Vec::with_capacity(field_pairs.len());
        let mut used_chars = title.chars().count();
        for (name, raw_val) in field_pairs {
            let val = truncate_chars(&raw_val, DISCORD_EMBED_FIELD_VALUE_MAX_CHARS);
            used_chars += name.chars().count() + val.chars().count();
            fields_json.push(serde_json::json!({
                "name": name,
                "value": val,
                "inline": true,
            }));
        }

        let footer_text = truncate_chars(
            &format!("starfane · {}", notice.key.trim()),
            DISCORD_EMBED_FOOTER_MAX_CHARS,
        );
        used_chars += footer_text.chars().count();

        // Respect BOTH Discord's 4096-char description cap and 6000-char total embed cap.
        let desc_budget = DISCORD_EMBED_TOTAL_MAX_CHARS
            .saturating_sub(used_chars)
            .min(DISCORD_EMBED_DESC_MAX_CHARS);
        let description = truncate_chars(notice.body_md.trim(), desc_budget);

        serde_json::json!({
            "username": self.username,
            "embeds": [{
                "title": title,
                "description": description,
                "color": discord_severity_color(notice.kind),
                "fields": fields_json,
                "footer": { "text": footer_text },
            }],
        })
    }

    async fn post_webhook(&self, payload: &serde_json::Value) -> Result<Option<String>, ApiError> {
        let target_url = discord_webhook_target_url(&self.webhook_url, self.thread_id.as_deref());
        let empty_headers = BTreeMap::new();
        let resp = curl_http_post(&target_url, payload, None, &empty_headers).await?;

        if resp.status == 429 {
            let detail = match serde_json::from_str::<serde_json::Value>(&resp.body) {
                Ok(v) => {
                    let retry = v
                        .get("retry_after")
                        .and_then(|r| {
                            r.as_f64()
                                .map(|n| format!("{n}"))
                                .or_else(|| r.as_str().map(str::to_string))
                        })
                        .unwrap_or_else(|| "unknown".into());
                    let msg = v
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("rate limited");
                    format!("retry_after={retry}s: {msg}")
                }
                Err(_) => body_preview(&resp.body),
            };
            return Err(ApiError::internal(format!(
                "discord webhook rate limited (HTTP 429, {detail})"
            )));
        }

        if !(200..300).contains(&resp.status) {
            return Err(ApiError::internal(format!(
                "discord webhook HTTP {}: {}",
                resp.status,
                body_preview(&resp.body)
            )));
        }

        // Discord returns 204 No Content by default, or 200 JSON `{"id":"..."}` when `?wait=true`.
        if resp.status == 204 || resp.body.trim().is_empty() {
            return Ok(None);
        }
        let msg_id = serde_json::from_str::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| {
                v.get("id").and_then(|id| {
                    id.as_str()
                        .map(str::to_string)
                        .or_else(|| id.as_u64().map(|n| n.to_string()))
                })
            });
        Ok(msg_id)
    }
}

enum SlackTransport {
    IncomingWebhook {
        webhook_url: String,
    },
    BotApi {
        bot_token: String,
        channel: String,
        api_base_url: String,
    },
}

struct SlackNotifier {
    id: String,
    ownership: NotifyOwnership,
    transport: SlackTransport,
}

impl Notifier for SlackNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Slack
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let (fallback_text, blocks) = build_slack_notice_blocks(notice);
            self.send_blocks(fallback_text, blocks).await
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let (fallback_text, blocks) = build_slack_test_blocks(text);
            let ts = self.send_blocks(fallback_text, blocks).await?;
            Ok(ts.unwrap_or_else(|| "ok".into()))
        })
    }
}

fn build_slack_notice_blocks(notice: &Notice) -> (String, serde_json::Value) {
    let meta = extract_notice_meta(notice);
    let raw_title = if notice.title.trim().is_empty() {
        "Starfane notification"
    } else {
        notice.title.trim()
    };
    let header_text = truncate_chars(raw_title, SLACK_HEADER_MAX_CHARS);
    let body = notice.body_md.trim();
    let fallback_raw = if body.is_empty() {
        raw_title.to_string()
    } else {
        format!("{raw_title}\n{body}")
    };
    let fallback_text = truncate_chars(&fallback_raw, SLACK_FALLBACK_TEXT_MAX_CHARS);

    let mut blocks = vec![serde_json::json!({
        "type": "header",
        "text": {
            "type": "plain_text",
            "text": header_text,
        }
    })];

    if !body.is_empty() {
        blocks.push(serde_json::json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": truncate_chars(body, SLACK_SECTION_MAX_CHARS),
            }
        }));
    }

    let mut ctx_parts = vec![
        format!("*Host:* `{}`", meta.host),
        format!("*Event:* `{}`", meta.event_label),
    ];
    if let Some(agent) = meta.agent {
        ctx_parts.push(format!("*Agent:* `{agent}`"));
    }
    if let Some(sid) = meta.session_id {
        ctx_parts.push(format!("*Session:* `{sid}`"));
    }
    if let Some(pane) = meta.pane {
        ctx_parts.push(format!("*Pane:* `{pane}`"));
    }
    if let Some(pid) = meta.pipeline_id {
        ctx_parts.push(format!("*Pipeline:* `{pid}`"));
    }
    if let Some(rid) = meta.review_id {
        ctx_parts.push(format!("*Review:* `{rid}`"));
    }

    blocks.push(serde_json::json!({
        "type": "context",
        "elements": [{
            "type": "mrkdwn",
            "text": truncate_chars(&ctx_parts.join("  ·  "), SLACK_SECTION_MAX_CHARS),
        }]
    }));

    (fallback_text, serde_json::Value::Array(blocks))
}

fn build_slack_test_blocks(text: &str) -> (String, serde_json::Value) {
    let body = if text.trim().is_empty() {
        "(empty test message)"
    } else {
        text.trim()
    };
    let fallback_text = truncate_chars(body, SLACK_FALLBACK_TEXT_MAX_CHARS);
    let blocks = serde_json::json!([
        {
            "type": "header",
            "text": {
                "type": "plain_text",
                "text": "Starfane Test",
            }
        },
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": truncate_chars(body, SLACK_SECTION_MAX_CHARS),
            }
        }
    ]);
    (fallback_text, blocks)
}

impl SlackNotifier {
    async fn send_blocks(
        &self,
        fallback_text: String,
        blocks: serde_json::Value,
    ) -> Result<Option<String>, ApiError> {
        let empty_headers = BTreeMap::new();
        match &self.transport {
            SlackTransport::IncomingWebhook { webhook_url } => {
                let payload = serde_json::json!({
                    "text": fallback_text,
                    "blocks": blocks,
                });
                let resp = curl_http_post(webhook_url, &payload, None, &empty_headers).await?;
                if !(200..300).contains(&resp.status) {
                    return Err(ApiError::internal(format!(
                        "slack webhook HTTP {}: {}",
                        resp.status,
                        body_preview(&resp.body)
                    )));
                }
                let trimmed = resp.body.trim();
                if trimmed.eq_ignore_ascii_case("ok") {
                    return Ok(None);
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
                        let ts = v.get("ts").and_then(|t| t.as_str()).map(str::to_string);
                        return Ok(ts);
                    }
                    let err = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown Slack webhook error");
                    return Err(ApiError::internal(format!(
                        "slack webhook rejected payload: {err}"
                    )));
                }
                Err(ApiError::internal(format!(
                    "slack webhook unexpected response: {}",
                    body_preview(trimmed)
                )))
            }
            SlackTransport::BotApi {
                bot_token,
                channel,
                api_base_url,
            } => {
                let url = format!("{api_base_url}/chat.postMessage");
                let payload = serde_json::json!({
                    "channel": channel,
                    "text": fallback_text,
                    "blocks": blocks,
                });
                let resp = curl_http_post(&url, &payload, Some(bot_token), &empty_headers).await?;
                if !(200..300).contains(&resp.status) {
                    return Err(ApiError::internal(format!(
                        "slack chat.postMessage HTTP {}: {}",
                        resp.status,
                        body_preview(&resp.body)
                    )));
                }
                let v: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| {
                    ApiError::internal(format!(
                        "slack chat.postMessage invalid JSON response ({e}): {}",
                        body_preview(&resp.body)
                    ))
                })?;
                if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
                    let err = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown Slack API error");
                    return Err(ApiError::internal(format!(
                        "slack chat.postMessage failed: {err}"
                    )));
                }
                let ts = v.get("ts").and_then(|t| t.as_str()).map(str::to_string);
                Ok(ts)
            }
        }
    }
}

struct WecomNotifier {
    id: String,
    ownership: NotifyOwnership,
    webhook_url: String,
    mentioned_list: Vec<String>,
    mentioned_mobile_list: Vec<String>,
}

impl Notifier for WecomNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Wecom
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let payload = self.build_notice_payload(notice);
            self.post_wecom(&payload).await?;
            Ok(None)
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let payload = self.build_text_or_markdown_payload(text);
            self.post_wecom(&payload).await?;
            Ok("ok".into())
        })
    }
}

impl WecomNotifier {
    fn has_mentions(&self) -> bool {
        !self.mentioned_list.is_empty() || !self.mentioned_mobile_list.is_empty()
    }

    fn build_notice_payload(&self, notice: &Notice) -> serde_json::Value {
        let meta = extract_notice_meta(notice);
        let mut parts = vec![format!("Host: {}", meta.host)];
        if let Some(ref agent) = meta.agent {
            parts.push(format!("Agent: {agent}"));
        }
        if let Some(ref sid) = meta.session_id {
            parts.push(format!("Session: {sid}"));
        }
        if let Some(ref pane) = meta.pane {
            parts.push(format!("Pane: {pane}"));
        }
        if let Some(ref pid) = meta.pipeline_id {
            parts.push(format!("Pipeline: {pid}"));
        }

        let title = if notice.title.trim().is_empty() {
            "Starfane notification"
        } else {
            notice.title.trim()
        };
        let body = notice.body_md.trim();

        if self.has_mentions() {
            let mut raw = format!(
                "[starfane · {}] {title}\n{}",
                meta.event_label,
                parts.join(" | ")
            );
            if !body.is_empty() {
                raw.push_str("\n\n");
                raw.push_str(body);
            }
            self.build_text_payload(&raw)
        } else {
            let mut md = format!(
                "### [{}] {title}\n> {}",
                meta.event_label,
                parts.join(" | ")
            );
            if !body.is_empty() {
                md.push_str("\n\n");
                md.push_str(body);
            }
            serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "content": truncate_utf8_bytes(&md, WECOM_MARKDOWN_MAX_BYTES),
                }
            })
        }
    }

    fn build_text_or_markdown_payload(&self, text: &str) -> serde_json::Value {
        if self.has_mentions() {
            self.build_text_payload(text)
        } else {
            serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "content": truncate_utf8_bytes(text, WECOM_MARKDOWN_MAX_BYTES),
                }
            })
        }
    }

    fn build_text_payload(&self, content: &str) -> serde_json::Value {
        let mut text_obj = serde_json::Map::new();
        text_obj.insert(
            "content".into(),
            serde_json::Value::String(truncate_utf8_bytes(content, WECOM_TEXT_MAX_BYTES)),
        );
        if !self.mentioned_list.is_empty() {
            text_obj.insert(
                "mentioned_list".into(),
                serde_json::Value::from(self.mentioned_list.clone()),
            );
        }
        if !self.mentioned_mobile_list.is_empty() {
            text_obj.insert(
                "mentioned_mobile_list".into(),
                serde_json::Value::from(self.mentioned_mobile_list.clone()),
            );
        }
        serde_json::json!({
            "msgtype": "text",
            "text": serde_json::Value::Object(text_obj),
        })
    }

    async fn post_wecom(&self, payload: &serde_json::Value) -> Result<(), ApiError> {
        let empty_headers = BTreeMap::new();
        let resp = curl_http_post(&self.webhook_url, payload, None, &empty_headers).await?;
        if !(200..300).contains(&resp.status) {
            return Err(ApiError::internal(format!(
                "wecom webhook HTTP {}: {}",
                resp.status,
                body_preview(&resp.body)
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| {
            ApiError::internal(format!(
                "wecom webhook invalid JSON response ({e}): {}",
                body_preview(&resp.body)
            ))
        })?;
        let errcode = v.get("errcode").and_then(|c| c.as_i64());
        if errcode != Some(0) {
            let errmsg = v
                .get("errmsg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown WeCom API error");
            return Err(ApiError::internal(format!(
                "wecom webhook failed (errcode={}): {errmsg}",
                errcode
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "missing".into())
            )));
        }
        Ok(())
    }
}

enum QqTransport {
    OneBotV11 {
        api_base_url: String,
        group_id: Option<u64>,
        user_id: Option<u64>,
        access_token: Option<String>,
    },
    Webhook {
        webhook_url: String,
        secret: Option<String>,
    },
}

struct QqNotifier {
    id: String,
    ownership: NotifyOwnership,
    transport: QqTransport,
}

impl Notifier for QqNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Qq
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let text = format_qq_notice(notice);
            match &self.transport {
                QqTransport::OneBotV11 { .. } => self.send_onebot(&text).await,
                QqTransport::Webhook {
                    webhook_url,
                    secret,
                } => {
                    let payload = serde_json::json!({
                        "key": notice.key,
                        "kind": notice.kind,
                        "title": notice.title,
                        "body_md": notice.body_md,
                        "message": text,
                        "session_id": notice.session_id,
                        "pipeline_id": notice.pipeline_id,
                        "review_id": notice.review_id,
                        "reply_session": notice.reply_session,
                    });
                    self.post_webhook(webhook_url, secret.as_deref(), &payload)
                        .await
                }
            }
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let safe_text = truncate_chars(text, QQ_MAX_TEXT_CHARS);
            match &self.transport {
                QqTransport::OneBotV11 { .. } => {
                    let id = self.send_onebot(&safe_text).await?;
                    Ok(id.unwrap_or_else(|| "ok".into()))
                }
                QqTransport::Webhook {
                    webhook_url,
                    secret,
                } => {
                    let payload = serde_json::json!({
                        "kind": "test",
                        "text": safe_text,
                        "message": safe_text,
                    });
                    let res = self
                        .post_webhook(webhook_url, secret.as_deref(), &payload)
                        .await?;
                    Ok(res.unwrap_or_else(|| "ok".into()))
                }
            }
        })
    }
}

fn format_qq_notice(notice: &Notice) -> String {
    let meta = extract_notice_meta(notice);
    let mut parts = vec![format!("Host: {}", meta.host)];
    if let Some(ref agent) = meta.agent {
        parts.push(format!("Agent: {agent}"));
    }
    if let Some(ref sid) = meta.session_id {
        parts.push(format!("Session: {sid}"));
    }
    if let Some(ref pane) = meta.pane {
        parts.push(format!("Pane: {pane}"));
    }
    if let Some(ref pid) = meta.pipeline_id {
        parts.push(format!("Pipeline: {pid}"));
    }
    let mut out = format!("[starfane · {}] {}", meta.event_label, notice.title.trim());
    out.push('\n');
    out.push_str(&parts.join(" | "));
    let body = notice.body_md.trim();
    if !body.is_empty() {
        out.push_str("\n\n");
        out.push_str(body);
    }
    truncate_chars(&out, QQ_MAX_TEXT_CHARS)
}

impl QqNotifier {
    async fn send_onebot(&self, message: &str) -> Result<Option<String>, ApiError> {
        let QqTransport::OneBotV11 {
            api_base_url,
            group_id,
            user_id,
            access_token,
        } = &self.transport
        else {
            return Err(ApiError::internal("invalid QQ transport state"));
        };
        let safe_msg = truncate_chars(message, QQ_MAX_TEXT_CHARS);
        let mut message_ids = Vec::new();

        if let Some(gid) = *group_id {
            let url = format!("{api_base_url}/send_group_msg");
            let payload = serde_json::json!({
                "group_id": gid,
                "message": safe_msg,
            });
            if let Some(mid) = self
                .post_onebot(&url, access_token.as_deref(), &payload)
                .await?
            {
                message_ids.push(mid);
            }
        }
        if let Some(uid) = *user_id {
            let url = format!("{api_base_url}/send_private_msg");
            let payload = serde_json::json!({
                "user_id": uid,
                "message": safe_msg,
            });
            if let Some(mid) = self
                .post_onebot(&url, access_token.as_deref(), &payload)
                .await?
            {
                message_ids.push(mid);
            }
        }
        if message_ids.is_empty() {
            Ok(None)
        } else {
            Ok(Some(message_ids.join(",")))
        }
    }

    async fn post_onebot(
        &self,
        url: &str,
        access_token: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<Option<String>, ApiError> {
        let empty_headers = BTreeMap::new();
        let resp = curl_http_post(url, payload, access_token, &empty_headers).await?;
        if !(200..300).contains(&resp.status) {
            return Err(ApiError::internal(format!(
                "qq onebot HTTP {}: {}",
                resp.status,
                body_preview(&resp.body)
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| {
            ApiError::internal(format!(
                "qq onebot invalid JSON response ({e}): {}",
                body_preview(&resp.body)
            ))
        })?;
        let retcode = v.get("retcode").and_then(|r| r.as_i64());
        let status = v.get("status").and_then(|s| s.as_str());
        if retcode != Some(0) || !status.is_some_and(|s| s.eq_ignore_ascii_case("ok")) {
            let detail = v
                .get("wording")
                .and_then(|w| w.as_str())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or("unknown OneBot API error");
            return Err(ApiError::internal(format!(
                "qq onebot failed (status={}, retcode={}): {detail}",
                status.unwrap_or("missing"),
                retcode
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "missing".into())
            )));
        }
        let msg_id = v
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|id| {
                id.as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| id.as_u64().map(|n| n.to_string()))
                    .or_else(|| id.as_str().map(str::to_string))
            });
        Ok(msg_id)
    }

    async fn post_webhook(
        &self,
        webhook_url: &str,
        secret: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<Option<String>, ApiError> {
        let empty_headers = BTreeMap::new();
        let resp = curl_http_post(webhook_url, payload, secret, &empty_headers).await?;
        if !(200..300).contains(&resp.status) {
            return Err(ApiError::internal(format!(
                "qq webhook HTTP {}: {}",
                resp.status,
                body_preview(&resp.body)
            )));
        }
        let trimmed = resp.body.trim();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(rc) = v.get("retcode").and_then(|r| r.as_i64()) {
                if rc != 0 {
                    let msg = v
                        .get("wording")
                        .or_else(|| v.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("QQ webhook retcode error");
                    return Err(ApiError::internal(format!(
                        "qq webhook failed (retcode={rc}): {msg}"
                    )));
                }
            }
            if v.get("ok").and_then(|b| b.as_bool()) == Some(false) {
                let msg = v
                    .get("error")
                    .or_else(|| v.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("QQ webhook rejected payload");
                return Err(ApiError::internal(format!("qq webhook failed: {msg}")));
            }
        }
        Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
    }
}

struct WebhookNotifier {
    id: String,
    ownership: NotifyOwnership,
    url: String,
    secret: Option<String>,
    headers: BTreeMap<String, String>,
}

impl Notifier for WebhookNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Webhook
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "key": notice.key,
                "kind": notice.kind,
                "title": notice.title,
                "body_md": notice.body_md,
                "session_id": notice.session_id,
                "pipeline_id": notice.pipeline_id,
                "review_id": notice.review_id,
                "reply_session": notice.reply_session,
            });
            let resp =
                curl_http_post(&self.url, &payload, self.secret.as_deref(), &self.headers).await?;
            if !(200..300).contains(&resp.status) {
                return Err(ApiError::internal(format!(
                    "webhook HTTP {}: {}",
                    resp.status,
                    body_preview(&resp.body)
                )));
            }
            let trimmed = resp.body.trim().to_string();
            Ok((!trimmed.is_empty()).then_some(trimmed))
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let payload = serde_json::json!({ "kind": "test", "text": text });
            let resp =
                curl_http_post(&self.url, &payload, self.secret.as_deref(), &self.headers).await?;
            if !(200..300).contains(&resp.status) {
                return Err(ApiError::internal(format!(
                    "webhook HTTP {}: {}",
                    resp.status,
                    body_preview(&resp.body)
                )));
            }
            let trimmed = resp.body.trim();
            Ok(if trimmed.is_empty() {
                "ok".to_string()
            } else {
                trimmed.to_string()
            })
        })
    }
}

struct MacosNotifier {
    id: String,
    ownership: NotifyOwnership,
    sound: String,
}

impl Notifier for MacosNotifier {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> NotifierKind {
        NotifierKind::Macos
    }
    fn ownership(&self) -> NotifyOwnership {
        self.ownership
    }
    fn send<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<Option<String>, ApiError>> {
        Box::pin(async move {
            self.display(&notice.title, &notice.body_md).await?;
            Ok(None)
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            self.display("Starfane", text).await?;
            Ok("ok".into())
        })
    }
}

/// Build an AppleScript `display notification` command with strict string escaping.
pub(crate) fn build_macos_applescript(title: &str, body: &str, sound: &str) -> String {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r', '\t'], " ")
            .replace('\0', "")
    };
    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let trimmed_sound = sound.trim();
    if !trimmed_sound.is_empty() {
        script.push_str(&format!(" sound name \"{}\"", esc(trimmed_sound)));
    }
    script
}

impl MacosNotifier {
    async fn display(&self, title: &str, body: &str) -> Result<(), ApiError> {
        let script = build_macos_applescript(title, body, &self.sound);
        let out = Command::new("osascript")
            .args(["-e", &script])
            .output()
            .await
            .map_err(|e| ApiError::internal(format!("osascript: {e}")))?;
        if !out.status.success() {
            return Err(ApiError::internal(format!(
                "osascript failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Zero-dependency async HTTP POST via `curl -K -` stdin (keeps tokens out of `ps`)
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    body: String,
}

fn curl_escape_config_val(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

async fn curl_http_post(
    url: &str,
    payload: &serde_json::Value,
    bearer_secret: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> Result<HttpResponse, ApiError> {
    if url.contains(['\r', '\n', '\0']) {
        return Err(ApiError::bad_request(
            "invalid control char in notifier URL",
        ));
    }
    if bearer_secret.is_some_and(|s| s.contains(['\r', '\n', '\0'])) {
        return Err(ApiError::bad_request(
            "invalid control char in notifier secret",
        ));
    }
    let body = serde_json::to_string(payload)
        .map_err(|e| ApiError::internal(format!("serialize payload: {e}")))?;

    let mut cfg_stdin = format!("url = \"{}\"\n", curl_escape_config_val(url));
    let mut has_content_type = false;
    for (k, v) in extra_headers {
        let hk = k.trim();
        if hk.is_empty() || hk.contains([':', '\r', '\n', '\0']) || v.contains(['\r', '\n', '\0']) {
            return Err(ApiError::bad_request(format!(
                "invalid custom header {k:?}"
            )));
        }
        if hk.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        cfg_stdin.push_str(&format!(
            "header = \"{}: {}\"\n",
            curl_escape_config_val(hk),
            curl_escape_config_val(v)
        ));
    }
    if !has_content_type {
        cfg_stdin.push_str("header = \"Content-Type: application/json; charset=utf-8\"\n");
    }
    if let Some(sec) = bearer_secret {
        cfg_stdin.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_escape_config_val(sec)
        ));
    }
    cfg_stdin.push_str(&format!(
        "data-raw = \"{}\"\n",
        curl_escape_config_val(&body)
    ));

    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "-X",
            "POST",
            "-w",
            "\n%{http_code}",
            "-K",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::internal(format!("spawn curl: {e}")))?;

    let write_res = if let Some(mut stdin) = child.stdin.take() {
        let res = stdin.write_all(cfg_stdin.as_bytes()).await;
        let _ = stdin.shutdown().await;
        res
    } else {
        Ok(())
    };

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| ApiError::internal(format!("wait curl: {e}")))?;
    if !out.status.success() {
        return Err(ApiError::internal(format!(
            "HTTP transport error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    write_res.map_err(|e| ApiError::internal(format!("write curl stdin: {e}")))?;

    let full = String::from_utf8_lossy(&out.stdout);
    let (body_part, code_part) = full.rsplit_once('\n').unwrap_or(("", full.as_ref()));
    let status = code_part.trim().parse::<u16>().map_err(|_| {
        ApiError::internal(format!(
            "curl returned unparseable HTTP status: {:?}",
            code_part.trim()
        ))
    })?;
    Ok(HttpResponse {
        status,
        body: body_part.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TeamConfig;

    #[test]
    fn default_config_is_opt_in_silent() {
        let cfg = StarfaneConfig::default();
        let router = NotifyRouter::from_config(&cfg);
        assert!(router.list_ids().is_empty());
        assert!(router.list_summaries().is_empty());
        let ctx = NotifyRouteContext {
            node: "local".into(),
            is_local_host: true,
            node_ownership: NotifyOwnership::NodeLocal,
            project: Some("workflow".into()),
            cwd: Some("/repo/workflow".into()),
            agent: Some(AgentKind::Claude),
            event_kind: NoticeKind::TurnComplete,
        };
        assert!(router.resolve_targets(&ctx, &[]).is_empty());
        assert!(!router.should_deliver_primary_feishu(&ctx, &[]));
    }

    #[test]
    fn legacy_feishu_synthesizes_default_and_enforces_split_ownership() {
        let mut cfg = StarfaneConfig::default();
        cfg.feishu.enabled = true;
        cfg.notifiers = vec![NotifierConfig {
            id: "tg-hub".into(),
            kind: NotifierKind::Webhook,
            enabled: true,
            ownership: NotifyOwnership::Hub,
            url: "http://127.0.0.1:9/hook".into(),
            ..NotifierConfig::default()
        }];
        cfg.notify_routes = vec![
            NotifyRouteConfig {
                notifiers: vec![DEFAULT_FEISHU_NOTIFIER_ID.into()],
                node: Some("local".into()),
                cwd: Some("/work/app".into()),
                ..NotifyRouteConfig::default()
            },
            NotifyRouteConfig {
                notifiers: vec!["tg-hub".into()],
                ..NotifyRouteConfig::default()
            },
        ];
        let router = NotifyRouter::from_config(&cfg);

        // Local host matching `/work/app` boundary -> both routes match; `/work/app-v2` must NOT match `/work/app`!
        let local_ctx = NotifyRouteContext {
            node: "local".into(),
            is_local_host: true,
            node_ownership: NotifyOwnership::NodeLocal,
            project: Some("app".into()),
            cwd: Some("/work/app/sub".into()),
            agent: Some(AgentKind::Claude),
            event_kind: NoticeKind::TurnComplete,
        };
        assert_eq!(
            router.resolve_targets(&local_ctx, &[]),
            vec![DEFAULT_FEISHU_NOTIFIER_ID.to_string(), "tg-hub".to_string()]
        );
        let sibling_ctx = NotifyRouteContext {
            cwd: Some("/work/app-v2".into()),
            ..local_ctx.clone()
        };
        assert_eq!(
            router.resolve_targets(&sibling_ctx, &[]),
            vec!["tg-hub".to_string()],
            "cwd prefix must respect directory boundaries"
        );

        // Remote SSH host (`Hub` ownership) -> Hub notifier delivers, NodeLocal feishu-default NEVER fires even if team.toml names it!
        let ssh_ctx = NotifyRouteContext {
            node: "tencent".into(),
            is_local_host: false,
            node_ownership: NotifyOwnership::Hub,
            project: Some("work".into()),
            cwd: Some("/srv/work".into()),
            agent: Some(AgentKind::Codex),
            event_kind: NoticeKind::TurnComplete,
        };
        let team = TeamConfig::parse(
            r#"
[notify]
notifiers = ["feishu-default", "tg-hub"]
"#,
        )
        .unwrap();
        assert_eq!(
            router.resolve_targets(&ssh_ctx, &team.notify_routes),
            vec!["tg-hub".to_string()],
            "Split Ownership must block node_local feishu-default on remote SSH host"
        );

        // Remote Peer host (`NodeLocal` ownership) -> Peer's own daemon owns notifications,
        // so Hub must NOT double-notify even via `tg-hub`!
        let peer_ctx = NotifyRouteContext {
            node: "mac-xiling".into(),
            is_local_host: false,
            node_ownership: NotifyOwnership::NodeLocal,
            project: Some("work".into()),
            cwd: Some("/Users/xiling/work".into()),
            agent: Some(AgentKind::Claude),
            event_kind: NoticeKind::TurnComplete,
        };
        assert!(
            router.resolve_targets(&peer_ctx, &[]).is_empty(),
            "Split Ownership must prevent Hub from double-notifying a NodeLocal Peer host"
        );
    }

    #[test]
    fn team_toml_rejects_embedded_secrets() {
        let bad = TeamConfig::parse(
            r#"
[notify]
notifiers = ["tg"]
bot_token = "12345:SECRET"
"#,
        );
        assert!(bad.is_err());
        assert!(bad.unwrap_err().contains("bot_token"));

        let bad_url = TeamConfig::parse(
            r#"
[notify]
[[notify.routes]]
notifiers = ["hook"]
url = "https://hooks.example.com/secret"
"#,
        );
        assert!(bad_url.is_err());
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries_and_limits() {
        assert_eq!(truncate_chars("hello", 0), "");
        assert_eq!(truncate_chars("hello", 1), "…");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello!", 5), "hell…");

        // Multi-byte CJK + 4-byte emoji must never panic or split mid-character.
        let mixed = "中文告警🚀🔥✅完成状态";
        let cut = truncate_chars(mixed, 6);
        assert_eq!(cut.chars().count(), 6);
        assert_eq!(cut, "中文告警🚀…");
        assert_eq!(truncate_chars(mixed, 20), mixed);
    }

    #[test]
    fn truncate_utf8_bytes_respects_byte_and_char_boundaries() {
        assert_eq!(truncate_utf8_bytes("hello", 0), "");
        assert_eq!(truncate_utf8_bytes("hello", 2), "he");
        assert_eq!(truncate_utf8_bytes("hello", 5), "hello");
        assert_eq!(truncate_utf8_bytes("hello world", 5), "he…");

        // Each CJK char is 3 bytes, emoji is 4 bytes, '…' is 3 bytes.
        let mixed = "中文🚀告警消息测试";
        for cap in 0..=mixed.len() + 2 {
            let out = truncate_utf8_bytes(mixed, cap);
            assert!(
                out.len() <= cap,
                "cap={cap}: byte len {} exceeded limit ({out:?})",
                out.len()
            );
        }
        let cut_12 = truncate_utf8_bytes(mixed, 12);
        // Budget = 12 - 3 = 9 bytes; "中"(3)+"文"(3) = 6, +"🚀"(4)=10 > 9 -> "中文…" (9 bytes <= 12).
        assert_eq!(cut_12, "中文…");
        assert!(cut_12.len() <= 12);
    }

    #[test]
    fn discord_thread_id_query_appending() {
        assert_eq!(
            discord_webhook_target_url("https://discord.com/api/webhooks/1/tok", None),
            "https://discord.com/api/webhooks/1/tok"
        );
        assert_eq!(
            discord_webhook_target_url(
                "https://discord.com/api/webhooks/1/tok",
                Some("987654321012345678")
            ),
            "https://discord.com/api/webhooks/1/tok?thread_id=987654321012345678"
        );
        assert_eq!(
            discord_webhook_target_url(
                "https://discord.com/api/webhooks/1/tok?wait=true",
                Some("987654321012345678")
            ),
            "https://discord.com/api/webhooks/1/tok?wait=true&thread_id=987654321012345678"
        );
    }

    #[test]
    fn macos_applescript_escaping_is_safe() {
        let script = build_macos_applescript(
            "Build \"Alert\" \\ Node",
            "Line 1\nLine 2\r\t\"quoted\" \\path\0end",
            "Glass \"Bell\"",
        );
        assert_eq!(
            script,
            "display notification \"Line 1 Line 2  \\\"quoted\\\" \\\\pathend\" with title \"Build \\\"Alert\\\" \\\\ Node\" sound name \"Glass \\\"Bell\\\"\""
        );

        let no_sound = build_macos_applescript("中文标题", "内容已完成", "   ");
        assert_eq!(
            no_sound,
            "display notification \"内容已完成\" with title \"中文标题\""
        );
    }
}
