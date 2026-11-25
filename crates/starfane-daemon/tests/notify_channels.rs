//! End-to-end mock-HTTP integration tests for pluggable notification channels:
//! - Telegram (`bot_token_env`, `api_base_url`, `message_thread_id`, HTTP 500 + `{"ok":false}`)
//! - Discord (`embeds`, severity color, fields, UTF-8 safe truncation, `?thread_id=`, 204/200/429/400)
//! - Slack (Incoming Webhook `"ok"` + failure AND Bot Web API `chat.postMessage` Bearer + `ts` + `ok:false`)
//! - WeCom / 企业微信 (`msgtype:"markdown"` 4096-byte UTF-8 truncation, `mentioned_list`/`mentioned_mobile_list`, `errcode!=0` + HTTP 500)
//! - QQ (OneBot v11 `/send_group_msg` & `/send_private_msg`, Bearer token, `retcode!=0` + HTTP 500, plus webhook fallback)
//! - Webhook (JSON payload, Bearer `secret_env`, custom `headers`, non-2xx failure)
//! - 6-channel `NotifyRouter` fan-out, failure isolation, Split Ownership (`node_local` vs `hub`),
//!   `.starfane/team.toml` routing, and Web API `GET /api/notifiers` + `POST /api/notifiers/{id}/test`.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use starfane_daemon::config::{
    NotifierConfig, NotifierKind, NotifyOwnership, NotifyRouteConfig, StarfaneConfig, TeamConfig,
    WebConfig,
};
use starfane_daemon::notice::{Notice, NoticeKind, SessionReplyTarget};
use starfane_daemon::notify::{NotifyRouteContext, NotifyRouter};
use starfane_daemon::{Core, DaemonConfig};
use starfane_proto::{id, AgentKind, PaneId, PipelineId, ReviewId, SessionId};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone)]
struct RecordedReq {
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Value,
}

#[derive(Default)]
struct MockServerState {
    requests: Mutex<Vec<RecordedReq>>,
    next_response: Mutex<Option<(StatusCode, String, String)>>, // (status, content_type, body)
}

impl MockServerState {
    fn record(
        &self,
        path: String,
        query: HashMap<String, String>,
        headers: &HeaderMap,
        body: Value,
    ) {
        let mut hmap = HashMap::new();
        for (k, v) in headers {
            if let Ok(vs) = v.to_str() {
                hmap.insert(k.as_str().to_ascii_lowercase(), vs.to_string());
            }
        }
        self.requests.lock().unwrap().push(RecordedReq {
            path,
            query,
            headers: hmap,
            body,
        });
    }

    fn set_response(&self, status: StatusCode, content_type: &str, body: impl Into<String>) {
        *self.next_response.lock().unwrap() = Some((status, content_type.to_string(), body.into()));
    }

    fn take_requests(&self) -> Vec<RecordedReq> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

async fn mock_handler(
    State(st): State<Arc<MockServerState>>,
    AxumPath(path): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (
    StatusCode,
    [(axum::http::header::HeaderName, String); 1],
    String,
) {
    st.record(format!("/{path}"), query, &headers, body);
    let (status, ctype, resp_body) = st.next_response.lock().unwrap().take().unwrap_or((
        StatusCode::OK,
        "application/json".into(),
        r#"{"ok":true}"#.into(),
    ));
    (
        status,
        [(axum::http::header::CONTENT_TYPE, ctype)],
        resp_body,
    )
}

async fn spawn_mock_server() -> (
    SocketAddr,
    Arc<MockServerState>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(MockServerState::default());
    let app = Router::new()
        .route("/{*path}", post(mock_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, state, handle)
}

fn sample_notice(kind: NoticeKind, title: &str, body_md: &str) -> Notice {
    Notice {
        key: "k-notify-01".into(),
        kind,
        title: title.into(),
        body_md: body_md.into(),
        pipeline_id: Some(PipelineId::new("pipe-42")),
        session_id: Some(SessionId::new("tencent:sess-99")),
        review_id: Some(ReviewId::new("rev-7")),
        actions: Vec::new(),
        binding_form: Vec::new(),
        reply_session: Some(SessionReplyTarget {
            session_id: SessionId::new("tencent:sess-99"),
            pane_id: Some(PaneId::new("tencent:%12")),
            tmux_label: Some("work:0.2".into()),
            agent: AgentKind::Claude,
        }),
    }
}

fn mk_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("starfane-notify-{tag}-{}", id::new_id("T")));
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn raw_http(addr: SocketAddr, req: &str) -> (u16, String, String) {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    let full = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = full.split_once("\r\n\r\n").unwrap_or((&full, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

#[tokio::test]
async fn telegram_delivery_send_test_and_error_handling() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let env_key = "STARFANE_TEST_TG_BOT_TOKEN_01";
    std::env::set_var(env_key, "123456:TEST_SECRET_TOKEN");

    let cfg: StarfaneConfig = toml::from_str(&format!(
        r#"
[[notifiers]]
id = "tg-ops"
kind = "telegram"
bot_token_env = "{env_key}"
chat_id = "-10099887766"
thread_id = 42
api_base_url = "http://{addr}/"
ownership = "hub"
"#
    ))
    .unwrap();
    cfg.validate().unwrap();

    let router = NotifyRouter::from_config(&cfg);
    let tg = router.get("tg-ops").expect("tg-ops registered");
    assert_eq!(tg.kind(), NotifierKind::Telegram);

    // 1. Successful notice delivery with message_thread_id & message_id extraction.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":true,"result":{"message_id":9001}}"#,
    );
    let notice = sample_notice(
        NoticeKind::TurnComplete,
        "Build Ready",
        "All 42 checks passed.",
    );
    let msg_id = tg.send(&notice).await.expect("telegram send succeeds");
    assert_eq!(msg_id.as_deref(), Some("9001"));

    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/bot123456:TEST_SECRET_TOKEN/sendMessage");
    assert_eq!(reqs[0].body["chat_id"], "-10099887766");
    assert_eq!(reqs[0].body["message_thread_id"], 42);
    let sent_text = reqs[0].body["text"].as_str().unwrap();
    assert!(sent_text.contains("Build Ready"), "{sent_text}");
    assert!(sent_text.contains("All 42 checks passed."), "{sent_text}");
    assert!(sent_text.contains("Host: tencent"), "{sent_text}");
    assert!(sent_text.contains("Agent: claude"), "{sent_text}");

    // 2. Successful send_test returning message_id.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":true,"result":{"message_id":9002}}"#,
    );
    let test_id = tg.send_test("hello telegram").await.expect("send_test ok");
    assert_eq!(test_id, "9002");
    let reqs = mock.take_requests();
    assert_eq!(reqs[0].body["text"], "hello telegram");
    assert_eq!(reqs[0].body["message_thread_id"], 42);

    // 3. HTTP 500 must fail explicitly.
    mock.set_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "application/json",
        r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#,
    );
    let err_500 = tg.send_test("should fail").await.unwrap_err();
    assert!(
        err_500.message.contains("500") && err_500.message.contains("Internal Server Error"),
        "unexpected error: {}",
        err_500.message
    );
    let _ = mock.take_requests();

    // 4. HTTP 200 with Telegram `{"ok":false,"description":"Bad Request"}` must ALSO fail explicitly!
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
    );
    let err_api = tg.send_test("bad chat").await.unwrap_err();
    assert!(
        err_api.message.contains("Bad Request: chat not found"),
        "unexpected error: {}",
        err_api.message
    );

    srv.abort();
}

#[tokio::test]
async fn discord_delivery_embeds_truncation_thread_id_and_errors() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let env_key = "STARFANE_TEST_DISCORD_HOOK_01";
    std::env::set_var(
        env_key,
        format!("http://{addr}/api/webhooks/1001/secret-tok"),
    );

    let cfg: StarfaneConfig = toml::from_str(&format!(
        r#"
[[notifiers]]
id = "discord-ops"
kind = "discord"
webhook_url_env = "{env_key}"
username = "starfane-ci"
thread_id = "987654321012345678"
ownership = "hub"
"#
    ))
    .unwrap();
    cfg.validate().unwrap();

    let router = NotifyRouter::from_config(&cfg);
    let dc = router.get("discord-ops").expect("discord-ops registered");
    assert_eq!(dc.kind(), NotifierKind::Discord);

    // 1. HTTP 204 No Content + rich embed fields + severity color + UTF-8 safe truncation (> 4096 chars).
    mock.set_response(StatusCode::NO_CONTENT, "text/plain", "");
    let long_utf8 = "告警日志🚀中文".repeat(800); // 7 * 800 = 5600 Unicode chars
    let err_notice = sample_notice(NoticeKind::Error, "Pipeline Failed", &long_utf8);
    let res_204 = dc.send(&err_notice).await.expect("204 succeeds");
    assert_eq!(res_204, None);

    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/api/webhooks/1001/secret-tok");
    assert_eq!(
        reqs[0].query.get("thread_id").map(String::as_str),
        Some("987654321012345678")
    );
    assert_eq!(reqs[0].body["username"], "starfane-ci");
    let embed = &reqs[0].body["embeds"][0];
    assert_eq!(embed["title"], "Pipeline Failed");
    assert_eq!(embed["color"], 0xD93829u32);
    let desc = embed["description"].as_str().unwrap();
    assert!(
        desc.ends_with('…'),
        "long description must be truncated with ellipsis"
    );
    assert!(
        desc.chars().count() <= 4096,
        "description exceeded 4096 chars: {}",
        desc.chars().count()
    );
    let fields = embed["fields"].as_array().unwrap();
    assert!(fields
        .iter()
        .any(|f| f["name"] == "Host" && f["value"] == "tencent"));
    assert!(fields
        .iter()
        .any(|f| f["name"] == "Event" && f["value"] == "error"));
    assert!(fields
        .iter()
        .any(|f| f["name"] == "Agent" && f["value"] == "claude"));
    assert!(fields
        .iter()
        .any(|f| f["name"] == "Session" && f["value"] == "tencent:sess-99"));
    assert!(fields
        .iter()
        .any(|f| f["name"] == "Pane" && f["value"] == "work:0.2"));
    assert!(embed["footer"]["text"]
        .as_str()
        .unwrap()
        .contains("k-notify-01"));

    // 2. HTTP 200 JSON `{"id":"..."}` (e.g. `?wait=true` + `&thread_id=...`) + green severity color.
    let cfg_wait = StarfaneConfig {
        notifiers: vec![NotifierConfig {
            id: "dc-wait".into(),
            kind: NotifierKind::Discord,
            enabled: true,
            webhook_url: Some(format!("http://{addr}/api/webhooks/1002/tok?wait=true")),
            thread_id: Some("555666".into()),
            ..NotifierConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    cfg_wait.validate().unwrap();
    let dc_wait = NotifyRouter::from_config(&cfg_wait).get("dc-wait").unwrap();
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"id":"112233445566"}"#,
    );
    let ok_notice = sample_notice(NoticeKind::TurnComplete, "Done", "Ready for review");
    let res_200 = dc_wait.send(&ok_notice).await.expect("200 JSON succeeds");
    assert_eq!(res_200.as_deref(), Some("112233445566"));
    let reqs = mock.take_requests();
    assert_eq!(reqs[0].query.get("wait").map(String::as_str), Some("true"));
    assert_eq!(
        reqs[0].query.get("thread_id").map(String::as_str),
        Some("555666")
    );
    assert_eq!(reqs[0].body["username"], "starfane");
    assert_eq!(reqs[0].body["embeds"][0]["color"], 0x237A4Bu32);

    // 3. send_test respects 2000-char UTF-8 safe truncation and handles 204 cleanly.
    mock.set_response(StatusCode::NO_CONTENT, "text/plain", "");
    let long_test = "测🔥".repeat(1200); // 2400 chars
    let test_res = dc.send_test(&long_test).await.expect("send_test 204 ok");
    assert_eq!(test_res, "ok");
    let reqs = mock.take_requests();
    let content = reqs[0].body["content"].as_str().unwrap();
    assert_eq!(content.chars().count(), 2000);
    assert!(content.ends_with('…'));

    // 4. HTTP 429 rate limit surfaces retry_after clearly.
    mock.set_response(
        StatusCode::TOO_MANY_REQUESTS,
        "application/json",
        r#"{"message":"You are being rate limited.","retry_after":1.25}"#,
    );
    let err_429 = dc.send_test("burst").await.unwrap_err();
    assert!(
        err_429.message.contains("429") && err_429.message.contains("1.25"),
        "expected 429 & retry_after in error: {}",
        err_429.message
    );
    let _ = mock.take_requests();

    // 5. HTTP 400 surfaces error body clearly.
    mock.set_response(
        StatusCode::BAD_REQUEST,
        "application/json",
        r#"{"code":50006,"message":"Cannot send an empty message"}"#,
    );
    let err_400 = dc.send_test("bad").await.unwrap_err();
    assert!(
        err_400.message.contains("400") && err_400.message.contains("Cannot send an empty message"),
        "expected 400 & body in error: {}",
        err_400.message
    );

    srv.abort();
}

#[tokio::test]
async fn slack_incoming_webhook_and_bot_api_delivery_and_errors() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let hook_env = "STARFANE_TEST_SLACK_HOOK_01";
    let bot_env = "STARFANE_TEST_SLACK_BOT_01";
    std::env::set_var(hook_env, format!("http://{addr}/services/T00/B00/XXXX"));
    std::env::set_var(bot_env, "xoxb-test-slack-token");

    let cfg: StarfaneConfig = toml::from_str(&format!(
        r#"
[[notifiers]]
id = "slack-hook"
kind = "slack"
webhook_url_env = "{hook_env}"
ownership = "hub"

[[notifiers]]
id = "slack-bot"
kind = "slack"
bot_token_env = "{bot_env}"
channel = "C01234567"
api_base_url = "http://{addr}/api"
ownership = "hub"
"#
    ))
    .unwrap();
    cfg.validate().unwrap();

    let router = NotifyRouter::from_config(&cfg);
    let hook = router.get("slack-hook").expect("slack-hook registered");
    let bot = router.get("slack-bot").expect("slack-bot registered");
    assert_eq!(hook.kind(), NotifierKind::Slack);
    assert_eq!(bot.kind(), NotifierKind::Slack);

    let notice = sample_notice(
        NoticeKind::PlanApproval,
        "Plan Needs Review",
        "Approve rollout?",
    );

    // --- Part A: Slack Incoming Webhook mode ---
    mock.set_response(StatusCode::OK, "text/plain", "ok");
    let hook_res = hook.send(&notice).await.expect("slack webhook ok");
    assert_eq!(hook_res, None);
    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/services/T00/B00/XXXX");
    assert!(reqs[0].body["text"]
        .as_str()
        .unwrap()
        .contains("Plan Needs Review"));
    let blocks = reqs[0].body["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "header");
    assert_eq!(blocks[0]["text"]["text"], "Plan Needs Review");
    assert_eq!(blocks[1]["type"], "section");
    assert_eq!(blocks[1]["text"]["text"], "Approve rollout?");
    assert_eq!(blocks[2]["type"], "context");
    assert!(blocks[2]["elements"][0]["text"]
        .as_str()
        .unwrap()
        .contains("*Host:* `tencent`"));

    // Incoming Webhook send_test succeeds on "ok"
    mock.set_response(StatusCode::OK, "text/plain", "ok");
    assert_eq!(hook.send_test("slack ping").await.unwrap(), "ok");
    let _ = mock.take_requests();

    // Incoming Webhook fails explicitly on HTTP 200 non-"ok" body AND on HTTP 403
    mock.set_response(StatusCode::OK, "text/plain", "invalid_payload");
    let err_body = hook.send_test("bad").await.unwrap_err();
    assert!(
        err_body.message.contains("invalid_payload"),
        "expected invalid_payload: {}",
        err_body.message
    );
    let _ = mock.take_requests();

    mock.set_response(StatusCode::FORBIDDEN, "text/plain", "invalid_token");
    let err_403 = hook.send_test("forbidden").await.unwrap_err();
    assert!(
        err_403.message.contains("403") && err_403.message.contains("invalid_token"),
        "expected 403 & invalid_token: {}",
        err_403.message
    );
    let _ = mock.take_requests();

    // --- Part B: Slack Bot Token `chat.postMessage` mode ---
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":true,"channel":"C01234567","ts":"1700000000.000100"}"#,
    );
    let ts = bot.send(&notice).await.expect("slack bot ok");
    assert_eq!(ts.as_deref(), Some("1700000000.000100"));
    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/api/chat.postMessage");
    assert_eq!(
        reqs[0].headers.get("authorization").map(String::as_str),
        Some("Bearer xoxb-test-slack-token")
    );
    assert_eq!(reqs[0].body["channel"], "C01234567");
    assert!(reqs[0].body["blocks"].as_array().unwrap().len() >= 3);

    // Bot API send_test returns `ts`
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":true,"ts":"1700000000.000200"}"#,
    );
    assert_eq!(
        bot.send_test("bot ping").await.unwrap(),
        "1700000000.000200"
    );
    let _ = mock.take_requests();

    // Bot API HTTP 200 with `{"ok":false,"error":"channel_not_found"}` MUST fail explicitly!
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":false,"error":"channel_not_found"}"#,
    );
    let err_cnf = bot.send_test("missing channel").await.unwrap_err();
    assert!(
        err_cnf.message.contains("channel_not_found"),
        "expected channel_not_found: {}",
        err_cnf.message
    );

    srv.abort();
}

#[tokio::test]
async fn wecom_delivery_markdown_byte_truncation_mentions_and_errors() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let env_key = "STARFANE_TEST_WECOM_HOOK_01";
    std::env::set_var(
        env_key,
        format!("http://{addr}/cgi-bin/webhook/send?key=wecom-secret"),
    );

    let cfg: StarfaneConfig = toml::from_str(&format!(
        r#"
[[notifiers]]
id = "wecom-md"
kind = "wecom"
webhook_url_env = "{env_key}"
ownership = "hub"

[[notifiers]]
id = "wecom-mention"
kind = "wecom"
webhook_url = "http://{addr}/cgi-bin/webhook/send?key=wecom-mention"
mentioned_list = ["alice", "@all"]
mentioned_mobile_list = ["13800001111"]
ownership = "hub"
"#
    ))
    .unwrap();
    cfg.validate().unwrap();

    let router = NotifyRouter::from_config(&cfg);
    let wecom_md = router.get("wecom-md").expect("wecom-md registered");
    let wecom_mention = router
        .get("wecom-mention")
        .expect("wecom-mention registered");
    assert_eq!(wecom_md.kind(), NotifierKind::Wecom);

    // 1. Markdown mode with multi-byte UTF-8 body exceeding 4096 bytes -> safely truncated <= 4096 bytes.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"errcode":0,"errmsg":"ok"}"#,
    );
    let long_zh = "企业微信构建告警🚀测试日志".repeat(200); // > 6000 UTF-8 bytes
    let notice = sample_notice(NoticeKind::Blocked, "Need Approval", &long_zh);
    let res = wecom_md.send(&notice).await.expect("wecom markdown ok");
    assert_eq!(res, None);

    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/cgi-bin/webhook/send");
    assert_eq!(
        reqs[0].query.get("key").map(String::as_str),
        Some("wecom-secret")
    );
    assert_eq!(reqs[0].body["msgtype"], "markdown");
    let md = reqs[0].body["markdown"]["content"].as_str().unwrap();
    assert!(
        md.len() <= 4096,
        "WeCom markdown content must be <= 4096 UTF-8 bytes, got {}",
        md.len()
    );
    assert!(
        md.ends_with('…'),
        "truncated markdown must end with ellipsis"
    );
    assert!(md.contains("Need Approval"), "{md}");
    assert!(md.contains("Host: tencent"), "{md}");

    // 2. Mention mode sends `msgtype: "text"` with `mentioned_list` & `mentioned_mobile_list` (<= 2048 bytes).
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"errcode":0,"errmsg":"ok"}"#,
    );
    assert_eq!(
        wecom_mention
            .send_test(&long_zh)
            .await
            .expect("mention test ok"),
        "ok"
    );
    let reqs = mock.take_requests();
    assert_eq!(reqs[0].body["msgtype"], "text");
    assert_eq!(
        reqs[0].body["text"]["mentioned_list"],
        json!(["alice", "@all"])
    );
    assert_eq!(
        reqs[0].body["text"]["mentioned_mobile_list"],
        json!(["13800001111"])
    );
    let txt = reqs[0].body["text"]["content"].as_str().unwrap();
    assert!(
        txt.len() <= 2048,
        "WeCom text content must be <= 2048 UTF-8 bytes, got {}",
        txt.len()
    );
    assert!(txt.ends_with('…'));

    // 3. HTTP 200 with non-zero `errcode` MUST fail explicitly (never silent!).
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#,
    );
    let err_code = wecom_md.send_test("ping").await.unwrap_err();
    assert!(
        err_code.message.contains("93000") && err_code.message.contains("invalid webhook url"),
        "expected errcode & errmsg: {}",
        err_code.message
    );
    let _ = mock.take_requests();

    // 4. HTTP 500 MUST fail explicitly.
    mock.set_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/plain",
        "wecom down",
    );
    let err_500 = wecom_md.send_test("ping").await.unwrap_err();
    assert!(
        err_500.message.contains("500") && err_500.message.contains("wecom down"),
        "expected 500 & body: {}",
        err_500.message
    );

    srv.abort();
}

#[tokio::test]
async fn qq_onebot_v11_group_private_webhook_fallback_and_errors() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let tok_env = "STARFANE_TEST_QQ_ONEBOT_TOK_01";
    let hook_env = "STARFANE_TEST_QQ_HOOK_01";
    let secret_env = "STARFANE_TEST_QQ_HOOK_SECRET_01";
    std::env::set_var(tok_env, "onebot-secret-token");
    std::env::set_var(hook_env, format!("http://{addr}/qq-gateway/push"));
    std::env::set_var(secret_env, "qq-gw-bearer");

    let cfg: StarfaneConfig = toml::from_str(&format!(
        r#"
[[notifiers]]
id = "qq-group"
kind = "qq"
api_base_url = "http://{addr}/onebot"
group_id = 123456789
bot_token_env = "{tok_env}"
ownership = "hub"

[[notifiers]]
id = "qq-private"
kind = "qq"
api_base_url = "http://{addr}/onebot"
user_id = "987654321"
ownership = "hub"

[[notifiers]]
id = "qq-webhook"
kind = "qq"
webhook_url_env = "{hook_env}"
secret_env = "{secret_env}"
ownership = "hub"
"#
    ))
    .unwrap();
    cfg.validate().unwrap();

    let router = NotifyRouter::from_config(&cfg);
    let qq_group = router.get("qq-group").expect("qq-group registered");
    let qq_private = router.get("qq-private").expect("qq-private registered");
    let qq_webhook = router.get("qq-webhook").expect("qq-webhook registered");
    assert_eq!(qq_group.kind(), NotifierKind::Qq);

    let notice = sample_notice(NoticeKind::TurnComplete, "QQ Task Done", "All specs green.");

    // 1. OneBot v11 `/send_group_msg` with Bearer token & `data.message_id`.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"status":"ok","retcode":0,"data":{"message_id":88001}}"#,
    );
    let gid_res = qq_group.send(&notice).await.expect("qq group ok");
    assert_eq!(gid_res.as_deref(), Some("88001"));
    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/onebot/send_group_msg");
    assert_eq!(
        reqs[0].headers.get("authorization").map(String::as_str),
        Some("Bearer onebot-secret-token")
    );
    assert_eq!(reqs[0].body["group_id"], 123456789u64);
    let msg = reqs[0].body["message"].as_str().unwrap();
    assert!(
        msg.contains("QQ Task Done") && msg.contains("Host: tencent"),
        "{msg}"
    );

    // 2. OneBot v11 `/send_private_msg` with `user_id` & `send_test` returning message_id.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"status":"ok","retcode":0,"data":{"message_id":88002}}"#,
    );
    let pid_res = qq_private.send_test("hello qq dm").await.expect("qq dm ok");
    assert_eq!(pid_res, "88002");
    let reqs = mock.take_requests();
    assert_eq!(reqs[0].path, "/onebot/send_private_msg");
    assert_eq!(reqs[0].body["user_id"], 987654321u64);
    assert_eq!(reqs[0].body["message"], "hello qq dm");

    // 3. OneBot v11 HTTP 200 with `retcode != 0` / `status != "ok"` MUST fail explicitly!
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"status":"failed","retcode":100,"wording":"group not found"}"#,
    );
    let err_rc = qq_group.send_test("missing group").await.unwrap_err();
    assert!(
        err_rc.message.contains("100") && err_rc.message.contains("group not found"),
        "expected retcode & wording: {}",
        err_rc.message
    );
    let _ = mock.take_requests();

    // 4. OneBot v11 HTTP 500 MUST fail explicitly.
    mock.set_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/plain",
        "napcat crashed",
    );
    let err_500 = qq_group.send_test("boom").await.unwrap_err();
    assert!(
        err_500.message.contains("500") && err_500.message.contains("napcat crashed"),
        "expected 500 & body: {}",
        err_500.message
    );
    let _ = mock.take_requests();

    // 5. QQ Webhook gateway fallback mode (success + non-2xx failure).
    mock.set_response(StatusCode::OK, "application/json", r#"{"ok":true}"#);
    let wh_res = qq_webhook.send(&notice).await.expect("qq webhook ok");
    assert!(wh_res.is_some());
    let reqs = mock.take_requests();
    assert_eq!(reqs[0].path, "/qq-gateway/push");
    assert_eq!(
        reqs[0].headers.get("authorization").map(String::as_str),
        Some("Bearer qq-gw-bearer")
    );
    assert_eq!(reqs[0].body["title"], "QQ Task Done");
    assert!(reqs[0].body["message"]
        .as_str()
        .unwrap()
        .contains("QQ Task Done"));

    mock.set_response(StatusCode::BAD_GATEWAY, "text/plain", "gateway offline");
    let err_gw = qq_webhook.send_test("ping").await.unwrap_err();
    assert!(
        err_gw.message.contains("502") && err_gw.message.contains("gateway offline"),
        "expected 502 & gateway offline: {}",
        err_gw.message
    );

    srv.abort();
}

#[tokio::test]
async fn webhook_delivery_custom_headers_bearer_secret_and_errors() {
    let (addr, mock, srv) = spawn_mock_server().await;
    let secret_env = "STARFANE_TEST_WEBHOOK_SECRET_01";
    std::env::set_var(secret_env, "wh-bearer-xyz");

    let mut headers = BTreeMap::new();
    headers.insert("X-Starfane-Env".into(), "staging".into());
    headers.insert("X-Trace-Id".into(), "trace-777".into());

    let cfg = StarfaneConfig {
        notifiers: vec![NotifierConfig {
            id: "hook-ops".into(),
            kind: NotifierKind::Webhook,
            enabled: true,
            url: format!("http://{addr}/events"),
            secret_env: secret_env.into(),
            headers,
            ..NotifierConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    cfg.validate().unwrap();

    let hook = NotifyRouter::from_config(&cfg).get("hook-ops").unwrap();
    mock.set_response(
        StatusCode::ACCEPTED,
        "application/json",
        r#"{"queued":true}"#,
    );
    let notice = sample_notice(NoticeKind::FinalReport, "Report Ready", "Summary body");
    let res = hook.send(&notice).await.expect("webhook send succeeds");
    assert_eq!(res.as_deref(), Some(r#"{"queued":true}"#));

    let reqs = mock.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/events");
    assert_eq!(
        reqs[0].headers.get("authorization").map(String::as_str),
        Some("Bearer wh-bearer-xyz")
    );
    assert_eq!(
        reqs[0].headers.get("x-starfane-env").map(String::as_str),
        Some("staging")
    );
    assert_eq!(
        reqs[0].headers.get("x-trace-id").map(String::as_str),
        Some("trace-777")
    );
    assert_eq!(reqs[0].body["key"], "k-notify-01");
    assert_eq!(reqs[0].body["kind"], "final_report");
    assert_eq!(reqs[0].body["title"], "Report Ready");
    assert_eq!(reqs[0].body["session_id"], "tencent:sess-99");
    assert_eq!(reqs[0].body["pipeline_id"], "pipe-42");

    // Non-2xx fails explicitly
    mock.set_response(StatusCode::BAD_GATEWAY, "text/plain", "upstream down");
    let err = hook.send_test("ping").await.unwrap_err();
    assert!(
        err.message.contains("502") && err.message.contains("upstream down"),
        "expected 502 & upstream down: {}",
        err.message
    );

    srv.abort();
}

#[tokio::test]
async fn multi_channel_router_fanout_failure_isolation_and_split_ownership() {
    // Dedicated path-aware mock server so concurrent fan-out requests never race on a single next_response!
    #[derive(Default)]
    struct MultiState {
        hits: Mutex<Vec<String>>,
    }
    async fn route_handler(
        State(st): State<Arc<MultiState>>,
        AxumPath(path): AxumPath<String>,
        Json(_body): Json<Value>,
    ) -> (StatusCode, String) {
        let full = format!("/{path}");
        st.hits.lock().unwrap().push(full.clone());
        if full.starts_with("/fail") {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "downstream exploded".into(),
            )
        } else if full.starts_with("/bot") {
            (
                StatusCode::OK,
                r#"{"ok":true,"result":{"message_id":77}}"#.into(),
            )
        } else if full.starts_with("/discord") {
            (StatusCode::NO_CONTENT, String::new())
        } else if full.starts_with("/slack-hook") {
            (StatusCode::OK, "ok".into())
        } else if full.starts_with("/slack-api/") {
            (StatusCode::OK, r#"{"ok":true,"ts":"1700.1"}"#.into())
        } else if full.starts_with("/wecom") {
            (StatusCode::OK, r#"{"errcode":0,"errmsg":"ok"}"#.into())
        } else if full.starts_with("/onebot/") {
            (
                StatusCode::OK,
                r#"{"status":"ok","retcode":0,"data":{"message_id":555}}"#.into(),
            )
        } else {
            (StatusCode::OK, r#"{"received":true}"#.into())
        }
    }

    let st = Arc::new(MultiState::default());
    let app = Router::new()
        .route("/{*path}", post(route_handler))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let cfg = StarfaneConfig {
        notifiers: vec![
            NotifierConfig {
                id: "tg-hub".into(),
                kind: NotifierKind::Telegram,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                bot_token: "111:AAA".into(),
                chat_id: "123".into(),
                api_base_url: Some(format!("http://{addr}")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "discord-hub".into(),
                kind: NotifierKind::Discord,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                webhook_url: Some(format!("http://{addr}/discord/1/tok")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "slack-hook-hub".into(),
                kind: NotifierKind::Slack,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                webhook_url: Some(format!("http://{addr}/slack-hook")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "slack-bot-local".into(),
                kind: NotifierKind::Slack,
                enabled: true,
                ownership: NotifyOwnership::NodeLocal,
                bot_token: "xoxb-local".into(),
                channel: Some("C999".into()),
                api_base_url: Some(format!("http://{addr}/slack-api")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "wecom-hub".into(),
                kind: NotifierKind::Wecom,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                webhook_url: Some(format!("http://{addr}/wecom/send")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "qq-hub".into(),
                kind: NotifierKind::Qq,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                api_base_url: Some(format!("http://{addr}/onebot")),
                group_id: Some("123456789".into()),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "webhook-ok".into(),
                kind: NotifierKind::Webhook,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                url: format!("http://{addr}/hook"),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "webhook-broken".into(),
                kind: NotifierKind::Webhook,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                url: format!("http://{addr}/fail"),
                ..NotifierConfig::default()
            },
        ],
        notify_routes: vec![NotifyRouteConfig {
            notifiers: vec![
                "webhook-broken".into(),
                "tg-hub".into(),
                "discord-hub".into(),
                "slack-hook-hub".into(),
                "slack-bot-local".into(),
                "wecom-hub".into(),
                "qq-hub".into(),
                "webhook-ok".into(),
            ],
            ..NotifyRouteConfig::default()
        }],
        ..StarfaneConfig::default()
    };
    cfg.validate().unwrap();
    let router = NotifyRouter::from_config(&cfg);
    let notice = sample_notice(NoticeKind::TurnComplete, "Multi Fan-out", "All channels!");

    // 1. Local host receives ALL 8 targets (6 channel kinds + broken channel):
    //    the broken webhook fails, yet all 7 healthy channels succeed concurrently!
    let local_ctx = NotifyRouteContext {
        node: "local".into(),
        is_local_host: true,
        node_ownership: NotifyOwnership::NodeLocal,
        project: Some("workflow".into()),
        cwd: Some("/repo/workflow".into()),
        agent: Some(AgentKind::Claude),
        event_kind: NoticeKind::TurnComplete,
    };
    let report = router
        .dispatch_pluggable_report(&notice, &local_ctx, &[])
        .await;
    assert_eq!(report.len(), 8);
    let by_id: HashMap<String, Result<Option<String>, _>> = report.into_iter().collect();
    assert!(
        by_id["webhook-broken"].is_err(),
        "broken channel must report Err"
    );
    assert_eq!(by_id["tg-hub"].as_ref().unwrap().as_deref(), Some("77"));
    assert_eq!(by_id["discord-hub"].as_ref().unwrap().as_deref(), None);
    assert_eq!(by_id["slack-hook-hub"].as_ref().unwrap().as_deref(), None);
    assert_eq!(
        by_id["slack-bot-local"].as_ref().unwrap().as_deref(),
        Some("1700.1")
    );
    assert_eq!(by_id["wecom-hub"].as_ref().unwrap().as_deref(), None);
    assert_eq!(by_id["qq-hub"].as_ref().unwrap().as_deref(), Some("555"));
    assert!(by_id["webhook-ok"].is_ok());
    assert_eq!(st.hits.lock().unwrap().len(), 8);

    // 2. Remote SSH host (`Hub` ownership): `node_local` `slack-bot-local` is BLOCKED by Split Ownership!
    st.hits.lock().unwrap().clear();
    let ssh_ctx = NotifyRouteContext {
        node: "tencent".into(),
        is_local_host: false,
        node_ownership: NotifyOwnership::Hub,
        project: Some("workflow".into()),
        cwd: Some("/srv/workflow".into()),
        agent: Some(AgentKind::Codex),
        event_kind: NoticeKind::TurnComplete,
    };
    let team = TeamConfig::parse(
        r#"
[notify]
notifiers = ["discord-hub", "slack-bot-local", "tg-hub"]
"#,
    )
    .unwrap();
    let ssh_report = router
        .dispatch_pluggable_report(&notice, &ssh_ctx, &team.notify_routes)
        .await;
    let ssh_ids: Vec<String> = ssh_report.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        ssh_ids,
        vec!["discord-hub".to_string(), "tg-hub".to_string()],
        "Split Ownership must block node_local slack-bot-local on remote SSH host"
    );
    assert!(ssh_report.iter().all(|(_, r)| r.is_ok()));

    // 3. Remote Peer host (`NodeLocal` ownership): Hub dispatches ZERO notifications!
    st.hits.lock().unwrap().clear();
    let peer_ctx = NotifyRouteContext {
        node: "mac-work".into(),
        is_local_host: false,
        node_ownership: NotifyOwnership::NodeLocal,
        project: Some("workflow".into()),
        cwd: Some("/Users/dev/workflow".into()),
        agent: Some(AgentKind::Claude),
        event_kind: NoticeKind::TurnComplete,
    };
    let peer_report = router
        .dispatch_pluggable_report(&notice, &peer_ctx, &team.notify_routes)
        .await;
    assert!(peer_report.is_empty());
    assert!(st.hits.lock().unwrap().is_empty());

    srv.abort();
}

#[tokio::test]
async fn web_api_notifiers_list_and_test_endpoints() {
    let root = mk_dir("web-api");
    let (mock_addr, mock, mock_srv) = spawn_mock_server().await;

    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();

    let mut cfg = StarfaneConfig {
        web: WebConfig {
            bind: "127.0.0.1".into(),
            port: api_addr.port(),
            auth_enabled: false,
            ..WebConfig::default()
        },
        notifiers: vec![
            NotifierConfig {
                id: "discord-ops".into(),
                kind: NotifierKind::Discord,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                webhook_url: Some(format!("http://{mock_addr}/discord/secret-path")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "qq-ops".into(),
                kind: NotifierKind::Qq,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                api_base_url: Some(format!("http://{mock_addr}/onebot")),
                group_id: Some("123456".into()),
                bot_token: "qq-super-secret".into(),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "slack-bot".into(),
                kind: NotifierKind::Slack,
                enabled: true,
                ownership: NotifyOwnership::NodeLocal,
                bot_token: "xoxb-super-secret".into(),
                channel: Some("C12345".into()),
                api_base_url: Some(format!("http://{mock_addr}/slack")),
                ..NotifierConfig::default()
            },
            NotifierConfig {
                id: "wecom-ops".into(),
                kind: NotifierKind::Wecom,
                enabled: true,
                ownership: NotifyOwnership::Hub,
                webhook_url: Some(format!("http://{mock_addr}/wecom/secret-key")),
                ..NotifierConfig::default()
            },
        ],
        ..StarfaneConfig::default()
    };
    cfg.paths.tmux = "/usr/bin/false".into();
    cfg.zellij.enabled = false;
    cfg.validate().unwrap();

    let core = Core::new(&DaemonConfig {
        state_dir: root.join("state"),
        socket: root.join("state/starfaned.sock"),
        config: cfg,
        web_dir: None,
        foreground: true,
    })
    .unwrap();
    let app = starfane_daemon::web::router(core);
    let api_srv = tokio::spawn(async move {
        let _ = axum::serve(
            api_listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // 1. GET /api/notifiers returns IDs + secret-free details (kind + ownership).
    let (st_list, _, body_list) = raw_http(
        api_addr,
        &format!(
            "GET /api/notifiers HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            api_addr.port()
        ),
    )
    .await;
    assert_eq!(st_list, 200, "{body_list}");
    assert!(
        !body_list.contains("xoxb-super-secret"),
        "must never leak bot token"
    );
    assert!(
        !body_list.contains("qq-super-secret"),
        "must never leak qq token"
    );
    assert!(
        !body_list.contains("secret-path"),
        "must never leak webhook URL"
    );
    assert!(
        !body_list.contains("secret-key"),
        "must never leak wecom URL"
    );
    let v: Value = serde_json::from_str(&body_list).unwrap();
    assert_eq!(
        v["data"]["notifiers"],
        json!(["discord-ops", "qq-ops", "slack-bot", "wecom-ops"])
    );
    assert_eq!(v["data"]["details"][0]["kind"], "discord");
    assert_eq!(v["data"]["details"][0]["ownership"], "hub");
    assert_eq!(v["data"]["details"][1]["kind"], "qq");
    assert_eq!(v["data"]["details"][2]["kind"], "slack");
    assert_eq!(v["data"]["details"][2]["ownership"], "node_local");
    assert_eq!(v["data"]["details"][3]["kind"], "wecom");

    // 2. POST /api/notifiers/slack-bot/test delivers to mock Slack Bot API.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"ok":true,"ts":"1700999.001"}"#,
    );
    let test_body = r#"{"text":"hello from api"}"#;
    let (st_test, _, body_test) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/slack-bot/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            test_body.len(),
            test_body
        ),
    )
    .await;
    assert_eq!(st_test, 200, "{body_test}");
    let tv: Value = serde_json::from_str(&body_test).unwrap();
    assert_eq!(tv["data"]["id"], "slack-bot");
    assert_eq!(tv["data"]["kind"], "slack");
    assert_eq!(tv["data"]["result"], "1700999.001");

    // 3. POST /api/notifiers/discord-ops/test delivers to mock Discord webhook (204).
    mock.set_response(StatusCode::NO_CONTENT, "text/plain", "");
    let (st_dc, _, body_dc) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/discord-ops/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            test_body.len(),
            test_body
        ),
    )
    .await;
    assert_eq!(st_dc, 200, "{body_dc}");
    let dv: Value = serde_json::from_str(&body_dc).unwrap();
    assert_eq!(dv["data"]["kind"], "discord");
    assert_eq!(dv["data"]["result"], "ok");

    // 3b. POST /api/notifiers/wecom-ops/test & qq-ops/test deliver cleanly.
    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"errcode":0,"errmsg":"ok"}"#,
    );
    let (st_wc, _, body_wc) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/wecom-ops/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            test_body.len(),
            test_body
        ),
    )
    .await;
    assert_eq!(st_wc, 200, "{body_wc}");
    let wv: Value = serde_json::from_str(&body_wc).unwrap();
    assert_eq!(wv["data"]["kind"], "wecom");
    assert_eq!(wv["data"]["result"], "ok");

    mock.set_response(
        StatusCode::OK,
        "application/json",
        r#"{"status":"ok","retcode":0,"data":{"message_id":4242}}"#,
    );
    let (st_qq, _, body_qq) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/qq-ops/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            test_body.len(),
            test_body
        ),
    )
    .await;
    assert_eq!(st_qq, 200, "{body_qq}");
    let qv: Value = serde_json::from_str(&body_qq).unwrap();
    assert_eq!(qv["data"]["kind"], "qq");
    assert_eq!(qv["data"]["result"], "4242");

    // 4. Unknown notifier returns 404.
    let (st_404, _, _) = raw_http(
        api_addr,
        &format!(
            "POST /api/notifiers/does-not-exist/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            api_addr.port(),
            api_addr.port(),
            test_body.len(),
            test_body
        ),
    )
    .await;
    assert_eq!(st_404, 404);

    api_srv.abort();
    mock_srv.abort();
    let _ = std::fs::remove_dir_all(&root);
}
