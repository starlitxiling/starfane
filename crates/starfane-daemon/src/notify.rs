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
use starfane_proto::{AgentKind, ApiError};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const DEFAULT_FEISHU_NOTIFIER_ID: &str = "feishu-default";

/// Pluggable outbound notification channel (Feishu profile, Telegram, Webhook, macOS native).
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
    ownership_by_id: HashMap<String, NotifyOwnership>,
    routes: Vec<NotifyRouteConfig>,
    has_legacy_feishu: bool,
}

impl NotifyRouter {
    pub fn from_config(cfg: &StarfaneConfig) -> Self {
        let mut notifiers: HashMap<String, Arc<dyn Notifier>> = HashMap::new();
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
                ownership_by_id.insert(id, nc.ownership);
                continue;
            }
            if let Some(n) = build_notifier(nc) {
                ownership_by_id.insert(id.clone(), nc.ownership);
                notifiers.insert(id, n);
            }
        }

        // 100% backward compatibility: `[feishu] enabled = true` synthesizes `feishu-default`.
        if has_legacy_feishu && !ownership_by_id.contains_key(DEFAULT_FEISHU_NOTIFIER_ID) {
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

    /// Fan out `notice` to all resolved pluggable notifiers (excluding `feishu-default`, which `lark.rs` delivers).
    pub async fn dispatch_pluggable(
        &self,
        notice: &Notice,
        ctx: &NotifyRouteContext,
        team_routes: &[TeamNotifyRoute],
    ) {
        let targets = self.resolve_targets(ctx, team_routes);
        for id in targets {
            if id == DEFAULT_FEISHU_NOTIFIER_ID && self.has_legacy_feishu {
                continue;
            }
            let Some(notifier) = self.notifiers.get(&id).cloned() else {
                continue;
            };
            let n_clone = notice.clone();
            tokio::spawn(async move {
                if let Err(e) = notifier.send(&n_clone).await {
                    log::warn!("notifier {id:?} send failed: {e}");
                }
            });
        }
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

fn build_notifier(cfg: &NotifierConfig) -> Option<Arc<dyn Notifier>> {
    match cfg.kind {
        NotifierKind::Feishu => Some(Arc::new(FeishuCliNotifier {
            id: cfg.id.trim().to_string(),
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
            let token = resolve_secret(&cfg.bot_token, &cfg.bot_token_env)?;
            if cfg.chat_id.trim().is_empty() {
                return None;
            }
            Some(Arc::new(TelegramNotifier {
                id: cfg.id.trim().to_string(),
                ownership: cfg.ownership,
                bot_token: token,
                chat_id: cfg.chat_id.trim().to_string(),
            }))
        }
        NotifierKind::Webhook => {
            let url = cfg.url.trim();
            if url.is_empty() {
                return None;
            }
            let secret = if cfg.secret_env.trim().is_empty() {
                None
            } else {
                std::env::var(cfg.secret_env.trim())
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            };
            Some(Arc::new(WebhookNotifier {
                id: cfg.id.trim().to_string(),
                ownership: cfg.ownership,
                url: url.to_string(),
                secret,
            }))
        }
        NotifierKind::Macos => Some(Arc::new(MacosNotifier {
            id: cfg.id.trim().to_string(),
            ownership: cfg.ownership,
            sound: cfg.sound.trim().to_string(),
        })),
    }
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
            let text = format!("{}\n\n{}", notice.title, notice.body_md);
            let res = self.send_message(&text).await?;
            Ok(Some(res))
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move { self.send_message(text).await })
    }
}

impl TelegramNotifier {
    async fn send_message(&self, text: &str) -> Result<String, ApiError> {
        // Pass URL containing bot token via curl `-K -` stdin so token never appears in `ps` args.
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let payload = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
        });
        curl_json_post(&url, &payload, None).await
    }
}

struct WebhookNotifier {
    id: String,
    ownership: NotifyOwnership,
    url: String,
    secret: Option<String>,
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
            });
            let res = curl_json_post(&self.url, &payload, self.secret.as_deref()).await?;
            Ok(Some(res))
        })
    }
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            let payload = serde_json::json!({ "kind": "test", "text": text });
            curl_json_post(&self.url, &payload, self.secret.as_deref()).await
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

impl MacosNotifier {
    async fn display(&self, title: &str, body: &str) -> Result<(), ApiError> {
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace(['\n', '\r'], " ")
        };
        let mut script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(title)
        );
        if !self.sound.is_empty() {
            script.push_str(&format!(" sound name \"{}\"", esc(&self.sound)));
        }
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

async fn curl_json_post(
    url: &str,
    payload: &serde_json::Value,
    bearer_secret: Option<&str>,
) -> Result<String, ApiError> {
    if url.contains(['\r', '\n']) {
        return Err(ApiError::bad_request("invalid newline in notifier URL"));
    }
    if bearer_secret.is_some_and(|s| s.contains(['\r', '\n'])) {
        return Err(ApiError::bad_request("invalid newline in notifier secret"));
    }
    let body = serde_json::to_string(payload)
        .map_err(|e| ApiError::internal(format!("serialize payload: {e}")))?;
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    };
    let mut cfg_stdin = format!("url = \"{}\"\n", esc(url));
    if let Some(sec) = bearer_secret {
        cfg_stdin.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            esc(sec)
        ));
    }
    cfg_stdin.push_str(&format!("data-raw = \"{}\"\n", esc(&body)));
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "--max-time",
            "10",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-K",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::internal(format!("spawn curl: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg_stdin.as_bytes()).await;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| ApiError::internal(format!("wait curl: {e}")))?;
    if !out.status.success() {
        return Err(ApiError::internal(format!(
            "webhook HTTP error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
}
