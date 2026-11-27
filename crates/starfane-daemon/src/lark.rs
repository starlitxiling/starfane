//! Feishu (Lark) two-way adapter over the `lark-cli` subprocess (contracts §7.4).
//!
//! * Outbound: [`Notice`] → interactive card (Card 2.0) sent with
//!   `lark-cli im +messages-send`, or replied into the pipeline thread with
//!   `+messages-reply --reply-in-thread`.
//! * Inbound: one long-running `lark-cli event consume <key> --as bot` per
//!   event key (`im.message.receive_v1`, `card.action.trigger`), stdout parsed
//!   as NDJSON, restarted with exponential backoff.
//! * Security: operator allow-list, chat allow-list (p2p exempt), nonce /
//!   message dedupe and text sanitization before anything reaches a pane.
//!
//! Everything that can be pure (sanitizing, command parsing, card building,
//! event extraction) is a free function with unit tests; the async glue only
//! shuttles data between `lark-cli` and [`Core`].

use crate::config::{parse_duration_secs, FeishuConfig};
use crate::notice::{ActionStyle, BoxFuture, FeishuSender, Notice, NoticeKind};
use crate::Core;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use starfane_proto::{
    ApiError, CreatePipelineReq, DaemonCommand, DispatchTarget, ErrorCode, FeedbackLabel,
    FindingId, PaneId, PipelineAction, PipelineId, PipelineSource, PipelineSpec, PipelineStatus,
    PlaybookKind, ReviewId, SessionId, StageBinding, StageKind, StageStatus, StateSnapshot, Via,
};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// Event keys consumed via `lark-cli event consume`.
pub const MESSAGE_EVENT: &str = "im.message.receive_v1";
pub const CARD_EVENT: &str = "card.action.trigger";
/// stderr marker printed by `lark-cli event consume` once subscribed.
const READY_MARKER: &str = "[event] ready event_key=";

/// Remote text cap before it may enter Dispatch (contracts §7.4).
pub const MAX_TEXT_BYTES: usize = 2048;
/// `--idempotency-key` limit enforced by lark-cli.
pub const MAX_IDEMPOTENCY_KEY: usize = 50;
/// Card markdown body cap (card content must stay well below 30KB).
const MAX_CARD_BODY_BYTES: usize = 16 * 1024;
/// Reply markdown cap.
const MAX_REPLY_BYTES: usize = 8 * 1024;
/// Goal preview length in status listings.
const GOAL_PREVIEW_CHARS: usize = 40;
/// Number of nonces / message ids remembered for dedupe.
const DEDUPE_CAPACITY: usize = 4096;
/// Length of the random card-action nonce.
const NONCE_LEN: usize = 8;

const LARK_CMD_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A consumer that ran at least this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
const STDERR_TAIL_CHARS: usize = 500;
const AUDIT_SOURCE: &str = "feishu";
/// Prefix of the binding-form submit button name: `bc_<nonce>_<pipeline_id>`.
const FORM_CONFIRM_PREFIX: &str = "bc_";
/// Prefix of the session-reply form submit button name: `rs_<nonce>_<session_id>`.
const FORM_REPLY_PREFIX: &str = "rs_";

const HELP_TEXT: &str = "**starfane 飞书指令**（中英文等价）\n\
- `帮助` / `help`\n\
- `状态` / `ls` / `status [P-id]`\n\
- `新功能 <项目> <目标>[；绑定话术]` / `feature …`\n\
- `查错 <项目> <现象>[；绑定话术]` / `debug …`\n\
- `回复 [<目标>] <文本>` / `reply …`（目标：`%12`、`s:<session>`、`P-…/<stage>`；在 pipeline 话题内可省略）\n\
- `确认 [P-id]` / `confirm`\n\
- `拒绝 [P-id] [理由]` / `reject`\n\
- `停止 <P-id>` / `stop`\n\
- `静音 <30m|2h|2d|23:30>` / `mute`；`取消静音` / `unmute`";

// ===========================================================================
// Pure helpers
// ===========================================================================

/// Bidi / zero-width format characters that can disguise text.
fn is_invisible_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Strip control and invisible format characters, fold newlines/tabs into
/// single spaces, trim, and cap at [`MAX_TEXT_BYTES`] on a char boundary.
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT_BYTES));
    let mut pending_space = false;
    for ch in s.chars() {
        if ch == '\n' || ch == '\r' || ch == '\t' || ch == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if ch.is_control() || is_invisible_format(ch) {
            continue;
        }
        let extra = ch.len_utf8() + usize::from(pending_space);
        if out.len() + extra > MAX_TEXT_BYTES {
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// Truncate to at most `max` bytes on a char boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// `--idempotency-key` value (≤50 bytes).
pub fn idempotency_key(key: &str) -> String {
    truncate_bytes(key, MAX_IDEMPOTENCY_KEY).to_string()
}

/// lark-cli treats `--content/--markdown/--text` values starting with `@` as
/// a file path and `-` as stdin; neutralize those with a zero-width space.
pub fn flag_safe(s: &str) -> String {
    if s.starts_with('@') || s == "-" {
        format!("\u{200B}{s}")
    } else {
        s.to_string()
    }
}

pub fn is_muted(until_ms: Option<u64>, now_ms: u64) -> bool {
    until_ms.is_some_and(|u| u > now_ms)
}

pub fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(BACKOFF_MAX)
}

fn new_nonce() -> String {
    let id = starfane_proto::id::new_id("N");
    // The tail of the id is the random part.
    let start = id.len().saturating_sub(NONCE_LEN);
    id[start..].to_string()
}

fn snake<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(e) => format!("<{e}>"),
    }
}

fn is_pipeline_token(tok: &str) -> bool {
    tok.len() > 2
        && tok.starts_with("P-")
        && tok[2..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Split off the first whitespace-delimited token.
fn split_token(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    }
}

// ---------------------------------------------------------------------------
// Text commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyTarget {
    Pane(PaneId),
    Session(SessionId),
    Stage {
        pipeline_id: PipelineId,
        stage: StageKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextCommand {
    Help,
    Status {
        pipeline_id: Option<PipelineId>,
    },
    Start {
        playbook: PlaybookKind,
        project: String,
        goal: String,
        utterance: Option<String>,
    },
    Reply {
        target: Option<ReplyTarget>,
        text: String,
    },
    Confirm {
        pipeline_id: Option<PipelineId>,
    },
    Reject {
        pipeline_id: Option<PipelineId>,
        reason: Option<String>,
    },
    Stop {
        pipeline_id: PipelineId,
    },
    Mute {
        secs: u64,
    },
    MuteUntil {
        hour: u8,
        minute: u8,
    },
    Unmute,
}

const MAX_MUTE_SECS: u64 = 7 * 86_400;

/// Parse a wall-clock `HH:MM` (or `HH：MM`) time specification (`00:00`..`23:59`).
pub fn parse_hhmm(tok: &str) -> Option<(u8, u8)> {
    let t = tok.trim();
    let (h, m) = t.split_once(':').or_else(|| t.split_once('：'))?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    if !h.bytes().all(|b| b.is_ascii_digit()) || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

/// Resolve local wall-clock `hour:minute` to the next future UTC epoch-ms timestamp.
/// If `hour:minute` today has already passed (or equals now), rolls forward to tomorrow.
pub fn resolve_hhmm_until_ms(hour: u8, minute: u8, now_ms: u64, tz_offset_secs: i64) -> u64 {
    let now_secs = (now_ms / 1000) as i64;
    let local_secs = now_secs + tz_offset_secs;
    let day_start_local = local_secs.div_euclid(86_400) * 86_400;
    let mut target_local = day_start_local + i64::from(hour) * 3600 + i64::from(minute) * 60;
    if target_local <= local_secs {
        target_local += 86_400;
    }
    let target_utc_secs = target_local - tz_offset_secs;
    (target_utc_secs.max(0) as u64).saturating_mul(1000)
}

/// Parse a `date +%z` offset string (e.g. `"+0800"`, `"-0530"`) into signed seconds.
pub fn parse_tz_offset(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.len() != 5 {
        return None;
    }
    let sign: i64 = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh: i64 = s[1..3].parse().ok()?;
    let mm: i64 = s[3..5].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60))
}

fn local_tz_offset_secs() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_tz_offset(&s))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Help,
    Status,
    Feature,
    Debug,
    Reply,
    Confirm,
    Reject,
    Stop,
    Mute,
    Unmute,
}

/// CJK keywords match as prefixes (no space needed); longest first.
const CJK_KEYWORDS: [(&str, Keyword); 10] = [
    ("取消静音", Keyword::Unmute),
    ("新功能", Keyword::Feature),
    ("帮助", Keyword::Help),
    ("状态", Keyword::Status),
    ("查错", Keyword::Debug),
    ("回复", Keyword::Reply),
    ("确认", Keyword::Confirm),
    ("拒绝", Keyword::Reject),
    ("停止", Keyword::Stop),
    ("静音", Keyword::Mute),
];

/// English keywords match the whole first token, case-insensitively.
const EN_KEYWORDS: [(&str, Keyword); 11] = [
    ("help", Keyword::Help),
    ("status", Keyword::Status),
    ("ls", Keyword::Status),
    ("feature", Keyword::Feature),
    ("debug", Keyword::Debug),
    ("reply", Keyword::Reply),
    ("confirm", Keyword::Confirm),
    ("reject", Keyword::Reject),
    ("stop", Keyword::Stop),
    ("mute", Keyword::Mute),
    ("unmute", Keyword::Unmute),
];

fn split_keyword(text: &str) -> Option<(Keyword, &str)> {
    let t = text.trim();
    let t = t.strip_prefix('/').unwrap_or(t);
    for (kw, k) in CJK_KEYWORDS {
        if let Some(rest) = t.strip_prefix(kw) {
            // Require a word boundary so phrases like "帮助我排查" or "状态不对" are not hijacked as commands.
            let boundary = rest.chars().next().is_none_or(|c| {
                c.is_whitespace()
                    || matches!(
                        c,
                        ':' | '：' | '/' | ',' | '，' | '!' | '！' | '?' | '？' | '%'
                    )
                    || (c == 'P' && rest[1..].starts_with('-'))
                    || (c == 's' && rest[1..].starts_with(':'))
            });
            if boundary {
                return Some((k, rest.trim()));
            }
        }
    }
    let (first, rest) = split_token(t);
    let first = first.to_ascii_lowercase();
    EN_KEYWORDS
        .iter()
        .find(|(kw, _)| *kw == first)
        .map(|(_, k)| (*k, rest))
}

/// `%12` | `s:<session>` | `P-…/<stage>`; `Ok(None)` when the token is not a target.
pub fn parse_reply_target(tok: &str) -> Result<Option<ReplyTarget>, String> {
    if let Some(n) = tok.strip_prefix('%') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return Ok(Some(ReplyTarget::Pane(PaneId::new(tok))));
        }
        return Ok(None);
    }
    if let Some(id) = tok.strip_prefix("s:") {
        if id.is_empty() {
            return Err("`s:` 后需要 session id".into());
        }
        return Ok(Some(ReplyTarget::Session(SessionId::new(id))));
    }
    if let Some((pid, stage)) = tok.split_once('/') {
        if is_pipeline_token(pid) {
            let stage = StageKind::parse(stage).ok_or_else(|| {
                format!("未知阶段 `{stage}`（可选 plan/implement/test_review/report/triage/deep）")
            })?;
            return Ok(Some(ReplyTarget::Stage {
                pipeline_id: PipelineId::new(pid),
                stage,
            }));
        }
    }
    Ok(None)
}

fn optional_pipeline(rest: &str) -> (Option<PipelineId>, &str) {
    let (tok, after) = split_token(rest);
    if is_pipeline_token(tok) {
        (Some(PipelineId::new(tok)), after)
    } else {
        (None, rest.trim())
    }
}

/// Parse a sanitized text command (contracts §7.4 grammar).
pub fn parse_text_command(text: &str) -> Result<TextCommand, String> {
    let Some((kw, rest)) = split_keyword(text) else {
        return Err("无法识别的指令，发送「帮助」查看用法".into());
    };
    match kw {
        Keyword::Help => Ok(TextCommand::Help),
        Keyword::Status => Ok(TextCommand::Status {
            pipeline_id: optional_pipeline(rest).0,
        }),
        Keyword::Feature | Keyword::Debug => {
            let (playbook, usage) = if kw == Keyword::Feature {
                (
                    PlaybookKind::Feature,
                    "用法：新功能 <项目> <目标>[；绑定话术]",
                )
            } else {
                (PlaybookKind::Debug, "用法：查错 <项目> <现象>[；绑定话术]")
            };
            let (main, utterance) = match rest.find(['；', ';']) {
                Some(i) => {
                    let sep_len = rest[i..].chars().next().map_or(1, char::len_utf8);
                    (&rest[..i], Some(rest[i + sep_len..].trim()))
                }
                None => (rest, None),
            };
            let (project, goal) = split_token(main);
            if project.is_empty() || goal.is_empty() {
                return Err(usage.into());
            }
            Ok(TextCommand::Start {
                playbook,
                project: project.to_string(),
                goal: goal.to_string(),
                utterance: utterance.filter(|u| !u.is_empty()).map(str::to_string),
            })
        }
        Keyword::Reply => {
            let (tok, after) = split_token(rest);
            let (target, text) = match parse_reply_target(tok)? {
                Some(t) => (Some(t), after),
                None => (None, rest.trim()),
            };
            if text.is_empty() {
                return Err("用法：回复 [<目标>] <文本>".into());
            }
            Ok(TextCommand::Reply {
                target,
                text: text.to_string(),
            })
        }
        Keyword::Confirm => Ok(TextCommand::Confirm {
            pipeline_id: optional_pipeline(rest).0,
        }),
        Keyword::Reject => {
            let (pipeline_id, reason) = optional_pipeline(rest);
            Ok(TextCommand::Reject {
                pipeline_id,
                reason: (!reason.is_empty()).then(|| reason.to_string()),
            })
        }
        Keyword::Stop => match optional_pipeline(rest).0 {
            Some(pipeline_id) => Ok(TextCommand::Stop { pipeline_id }),
            None => Err("用法：停止 <P-id>".into()),
        },
        Keyword::Mute => {
            let (tok, _) = split_token(rest);
            if let Some((hour, minute)) = parse_hhmm(tok) {
                Ok(TextCommand::MuteUntil { hour, minute })
            } else {
                match parse_duration_secs(tok) {
                    Some(secs) if secs > MAX_MUTE_SECS => {
                        Err("静音时长不能超过 7 天（最大 7d / 10080m）".into())
                    }
                    Some(secs) if secs > 0 => Ok(TextCommand::Mute { secs }),
                    _ => Err("用法：静音 <30m|2h|2d|23:30>".into()),
                }
            }
        }
        Keyword::Unmute => Ok(TextCommand::Unmute),
    }
}

// ---------------------------------------------------------------------------
// Card builder
// ---------------------------------------------------------------------------

fn header_template(kind: NoticeKind) -> &'static str {
    match kind {
        NoticeKind::BindingConfirm => "blue",
        NoticeKind::PlanApproval => "wathet",
        NoticeKind::Blocked | NoticeKind::Error => "red",
        NoticeKind::TurnComplete => "turquoise",
        NoticeKind::TriageReady => "orange",
        NoticeKind::ReviewResult => "purple",
        NoticeKind::FinalReport => "green",
        NoticeKind::Progress => "grey",
        NoticeKind::DeliveryUnconfirmed => "yellow",
    }
}

fn button_type(style: ActionStyle) -> &'static str {
    match style {
        ActionStyle::Primary => "primary_filled",
        ActionStyle::Danger => "danger",
        ActionStyle::Default => "default",
    }
}

fn plain(text: &str) -> Value {
    json!({ "tag": "plain_text", "content": text })
}

/// Card-callback value (contracts §7.4).
pub fn action_value(op: &str, notice: &Notice, finding_id: Option<&str>, nonce: &str) -> Value {
    let mut v = Map::new();
    v.insert("starfane".into(), json!(1));
    v.insert("op".into(), json!(op));
    if let Some(p) = &notice.pipeline_id {
        v.insert("pipeline_id".into(), json!(p.as_str()));
    }
    if let Some(r) = &notice.review_id {
        v.insert("review_id".into(), json!(r.as_str()));
    }
    if let Some(f) = finding_id {
        v.insert("finding_id".into(), json!(f));
    }
    v.insert("nonce".into(), json!(nonce));
    Value::Object(v)
}

fn binding_choices(stage: &str, current: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![current.to_string()];
    let mut base = vec![
        "claude",
        "codex",
        "agy",
        "claude:opus@high",
        "codex@high",
        "agy /swarm",
    ];
    if stage == StageKind::Report.as_str() {
        base.push("same_as:plan");
    }
    for b in base {
        if !out.iter().any(|o| o == b) {
            out.push(b.to_string());
        }
    }
    out.retain(|o| !o.is_empty());
    out
}

/// Marker inserted by `core::router::build_notice` before the agent's last assistant output.
const LAST_OUTPUT_MARKER: &str = "\n**最后一轮输出**：\n";
/// Max characters for the 1-line inline preview shown above a collapsed panel.
const COLLAPSIBLE_PREVIEW_CHARS: usize = 80;
/// General notice body thresholds above which content folds into a `collapsible_panel`.
const LONG_NOTICE_FOLD_CHARS: usize = 240;
const LONG_NOTICE_FOLD_LINES: usize = 6;

fn build_collapsible_panel(header_title: &str, detail_md: &str) -> Value {
    json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "background_color": "grey-50",
        "border": {
            "color": "grey-200",
            "corner_radius": "6px"
        },
        "padding": "8px 12px 8px 12px",
        "vertical_spacing": "8px",
        "header": {
            "title": plain(header_title),
            "width": "fill",
            "vertical_align": "center"
        },
        "elements": [
            { "tag": "markdown", "content": detail_md }
        ]
    })
}

/// Extract a clean 1-line preview (≤ `max_chars`) from Markdown output for the compact card face.
fn first_line_preview(md: &str, max_chars: usize) -> Option<String> {
    for line in md.lines() {
        let stripped = line
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start_matches(['-', '*', '•'])
            .trim();
        if stripped.is_empty() || stripped.starts_with("```") {
            continue;
        }
        let cleaned = stripped
            .replace("**", "")
            .replace("__", "")
            .replace('`', "");
        let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            return Some(truncate_chars(&collapsed, max_chars));
        }
    }
    None
}

fn escape_feishu_font_text(s: &str) -> String {
    s.replace('<', "&lt;").replace('>', "&gt;")
}

/// Split `body_md` into `(compact_head_md, optional (panel_header_title, full_detail_md))`.
fn split_card_body(body_md: &str) -> (String, Option<(String, String)>) {
    if let Some((head, tail)) = body_md.split_once(LAST_OUTPUT_MARKER) {
        let (detail_raw, is_sub) = match tail.trim_end().strip_suffix("_来自子会话_") {
            Some(rest) => (rest.trim(), true),
            None => (tail.trim(), false),
        };
        let detail_clean = crate::core::transcript::strip_codex_internal_tags(detail_raw);
        let detail_raw = detail_clean.as_str();
        let mut compact = head.trim().to_string();
        if is_sub {
            if !compact.is_empty() {
                compact.push('\n');
            }
            compact.push_str("_来自子会话_");
        }
        if detail_raw.is_empty() {
            return (compact, None);
        }
        let truncated = truncate_bytes(detail_raw, MAX_CARD_BODY_BYTES);
        let detail = if truncated.len() < detail_raw.len() {
            format!("{truncated}\n…（已截断）")
        } else {
            truncated.to_string()
        };
        let char_count = detail_raw.chars().count();
        if let Some(preview) = first_line_preview(detail_raw, COLLAPSIBLE_PREVIEW_CHARS) {
            if !compact.is_empty() {
                compact.push('\n');
            }
            compact.push_str(&format!(
                "<font color='grey'>摘要：{}</font>",
                escape_feishu_font_text(&preview)
            ));
        }
        let panel_title = format!("展开最后一轮输出（{char_count} 字）");
        return (compact, Some((panel_title, detail)));
    }

    let trimmed = body_md.trim();
    let char_count = trimmed.chars().count();
    let line_count = trimmed.lines().count();
    if char_count > LONG_NOTICE_FOLD_CHARS || line_count > LONG_NOTICE_FOLD_LINES {
        let truncated = truncate_bytes(trimmed, MAX_CARD_BODY_BYTES);
        let detail = if truncated.len() < trimmed.len() {
            format!("{truncated}\n…（已截断）")
        } else {
            truncated.to_string()
        };
        let compact = first_line_preview(trimmed, COLLAPSIBLE_PREVIEW_CHARS)
            .map(|p| {
                format!(
                    "<font color='grey'>摘要：{}</font>",
                    escape_feishu_font_text(&p)
                )
            })
            .unwrap_or_default();
        let panel_title = format!("展开详细内容（{char_count} 字）");
        return (compact, Some((panel_title, detail)));
    }

    (trimmed.to_string(), None)
}

/// Build a compact Card 2.0 test card with a collapsed `collapsible_panel` (`expanded: false`)
/// so `POST /api/feishu/test` exercises the exact same Card 2.0 schema used by session notices.
pub fn build_test_card(text: &str) -> Value {
    let trimmed = text.trim();
    let detail = if trimmed.is_empty() {
        "Starfane 飞书折叠卡片连通性测试成功。\n\n- 通知统一仅发交互卡片，不再重复发送普通长消息\n- 长输出默认收纳在折叠面板内，点击即可原地展开阅读".to_string()
    } else {
        trimmed.to_string()
    };
    let n = Notice {
        key: format!("test-{}", starfane_proto::id::now_ms()),
        kind: NoticeKind::TurnComplete,
        title: "✅ starfane 飞书通知测试（折叠卡片）".into(),
        body_md: format!(
            "**窗口**：starfane-console\n**状态**：连接正常\n\n**最后一轮输出**：\n{detail}\n"
        ),
        pipeline_id: None,
        session_id: None,
        review_id: None,
        actions: Vec::new(),
        binding_form: Vec::new(),
        reply_session: None,
    };
    build_card(&n, &mut new_nonce)
}

/// Build a Card 2.0 interactive card for a notice.
pub fn build_card(notice: &Notice, nonce: &mut dyn FnMut() -> String) -> Value {
    let mut elements: Vec<Value> = Vec::new();
    let (compact_body, collapsible) = split_card_body(&notice.body_md);
    if !compact_body.trim().is_empty() {
        elements.push(json!({ "tag": "markdown", "content": compact_body }));
    }
    if let Some((panel_title, detail_md)) = collapsible {
        elements.push(build_collapsible_panel(&panel_title, &detail_md));
    }
    let mut meta = Vec::new();
    if let Some(p) = &notice.pipeline_id {
        meta.push(format!("`{p}`"));
    }
    if let Some(s) = &notice.session_id {
        meta.push(format!("session `{s}`"));
    }
    if !meta.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": format!("<font color='grey'>{}</font>", escape_feishu_font_text(&meta.join(" · ")))
        }));
    }

    let has_form = !notice.binding_form.is_empty();
    if has_form {
        let mut form_elements: Vec<Value> = Vec::new();
        for (stage, current) in &notice.binding_form {
            let options: Vec<Value> = binding_choices(stage, current)
                .iter()
                .map(|c| json!({ "text": plain(c), "value": c }))
                .collect();
            form_elements.push(json!({ "tag": "markdown", "content": format!("**{stage}**") }));
            let mut select = json!({
                "tag": "select_static",
                "name": stage,
                "placeholder": plain(stage),
                "options": options,
                "width": "fill"
            });
            if !current.is_empty() {
                select["initial_option"] = json!(current);
            }
            form_elements.push(select);
        }
        let label = notice
            .actions
            .iter()
            .find(|a| a.op == "confirm_pipeline")
            .map_or("确认绑定", |a| a.label.as_str());
        let pid = notice.pipeline_id.as_ref().map_or("", |p| p.as_str());
        form_elements.push(json!({
            "tag": "button",
            "name": format!("{FORM_CONFIRM_PREFIX}{}_{pid}", nonce()),
            "text": plain(label),
            "type": "primary_filled",
            "form_action_type": "submit"
        }));
        elements.push(json!({ "tag": "form", "name": "binding_form", "elements": form_elements }));
    } else if let Some(target) = &notice.reply_session {
        // Card 2.0 top-level `form` with `input` + `submit` button (per lark-im form.md & input.md).
        elements.push(json!({
            "tag": "form",
            "name": "reply_form",
            "elements": [
                {
                    "tag": "input",
                    "name": "reply_text",
                    "required": true,
                    "width": "fill",
                    "placeholder": plain("直接输入回复给 Agent...")
                },
                {
                    "tag": "button",
                    "name": format!("{FORM_REPLY_PREFIX}{}_{}", nonce(), target.session_id.as_str()),
                    "text": plain("发送给 Agent"),
                    "type": "primary_filled",
                    "form_action_type": "submit"
                }
            ]
        }));
        elements.push(json!({
            "tag": "markdown",
            "content": "<font color='grey'>引用回复或下方输入发给 Agent</font>"
        }));
    }

    let columns: Vec<Value> = notice
        .actions
        .iter()
        .filter(|a| !(has_form && a.op == "confirm_pipeline"))
        .map(|a| {
            let value = action_value(&a.op, notice, a.finding_id.as_deref(), &nonce());
            json!({
                "tag": "column",
                "width": "auto",
                "elements": [{
                    "tag": "button",
                    "text": plain(&a.label),
                    "type": button_type(a.style),
                    "behaviors": [{ "type": "callback", "value": value }]
                }]
            })
        })
        .collect();
    if !columns.is_empty() {
        elements.push(json!({
            "tag": "column_set",
            "flex_mode": "flow",
            "horizontal_spacing": "8px",
            "columns": columns
        }));
    }

    json!({
        "schema": "2.0",
        "config": { "update_multi": true },
        "header": {
            "title": plain(&notice.title),
            "template": header_template(notice.kind)
        },
        "body": { "elements": elements }
    })
}

// ---------------------------------------------------------------------------
// Event extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub event_id: Option<String>,
    pub message_id: String,
    pub chat_id: String,
    pub p2p: bool,
    pub sender_open_id: String,
    pub sender_type: Option<String>,
    pub message_type: String,
    /// Text with mention placeholders removed (not yet sanitized).
    pub text: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    /// `None` when the event carries no mention info.
    pub mentioned: Option<bool>,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().filter(|s| !s.is_empty())
}

fn owned(v: Option<&str>) -> Option<String> {
    v.map(str::to_string)
}

/// Extract readable text from Feishu `text` JSON (`{"text": "..."}`) or `post` rich-text JSON.
fn extract_post_node(node: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(title) = node
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(title.to_string());
    }
    if let Some(paragraphs) = node.get("content").and_then(Value::as_array) {
        for para in paragraphs {
            let Some(elems) = para.as_array() else {
                continue;
            };
            let mut line = String::new();
            for el in elems {
                let tag = el.get("tag").and_then(Value::as_str).unwrap_or("");
                match tag {
                    "text" | "a" => {
                        if let Some(t) = el.get("text").and_then(Value::as_str) {
                            line.push_str(t);
                        }
                    }
                    "at" => {
                        if let Some(n) = el.get("user_name").and_then(Value::as_str) {
                            line.push('@');
                            line.push_str(n);
                        }
                    }
                    _ => {}
                }
            }
            if !line.trim().is_empty() {
                parts.push(line.trim().to_string());
            }
        }
    }
    parts.join("\n")
}

/// Message content: plain text, `text` JSON payload, or `post` rich-text JSON payload.
fn content_text(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(m)) => {
            if let Some(t) = m.get("text").and_then(Value::as_str) {
                return t.to_string();
            }
            let direct = extract_post_node(&Value::Object(m.clone()));
            if !direct.is_empty() {
                return direct;
            }
            for v in m.values() {
                if v.is_object() {
                    let sub = extract_post_node(v);
                    if !sub.is_empty() {
                        return sub;
                    }
                }
            }
            raw.to_string()
        }
        _ => raw.to_string(),
    }
}

fn parse_mentions(v: Option<&Value>) -> Option<Vec<Mention>> {
    let arr = v?.as_array()?;
    Some(
        arr.iter()
            .map(|m| Mention {
                key: m
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect(),
    )
}

/// Remove mention placeholders (`@_user_1`) / `@name` and leading `@…` tokens.
pub fn strip_mentions(text: &str, mentions: &[Mention]) -> String {
    let mut t = text.to_string();
    for m in mentions {
        if !m.key.is_empty() {
            t = t.replace(&m.key, " ");
        }
        if !m.name.is_empty() {
            t = t.replace(&format!("@{}", m.name), " ");
        }
    }
    let mut rest = t.trim();
    while rest.starts_with('@') {
        rest = split_token(rest).1;
    }
    rest.to_string()
}

/// Extract an `im.message.receive_v1` event. Accepts lark-cli's flattened
/// output and the raw Open Platform envelope (`{header, event:{message,sender}}`).
pub fn extract_message(v: &Value) -> Option<InboundMessage> {
    let raw_msg = v.get("event").and_then(|e| e.get("message"));
    let (msg, event_id, sender_open_id, sender_type, parent_id) = match raw_msg {
        Some(m) => (
            m,
            owned(str_at(v, &["header", "event_id"])),
            str_at(v, &["event", "sender", "sender_id", "open_id"]),
            owned(str_at(v, &["event", "sender", "sender_type"])),
            owned(str_at(m, &["parent_id"])),
        ),
        None => (
            v,
            owned(str_at(v, &["event_id"])),
            str_at(v, &["sender_id"]).or_else(|| str_at(v, &["sender_id", "open_id"])),
            owned(str_at(v, &["sender_type"])),
            owned(str_at(v, &["reply_to"]).or_else(|| str_at(v, &["parent_id"]))),
        ),
    };
    let message_id = str_at(msg, &["message_id"]).or_else(|| str_at(msg, &["id"]))?;
    let chat_id = str_at(msg, &["chat_id"]).unwrap_or("");
    let mentions = parse_mentions(msg.get("mentions"));
    let text = content_text(str_at(msg, &["content"]).unwrap_or(""));
    let text = strip_mentions(&text, mentions.as_deref().unwrap_or(&[]));
    Some(InboundMessage {
        event_id,
        message_id: message_id.to_string(),
        chat_id: chat_id.to_string(),
        p2p: str_at(msg, &["chat_type"]) == Some("p2p"),
        sender_open_id: sender_open_id.unwrap_or("").to_string(),
        sender_type,
        message_type: str_at(msg, &["message_type"]).unwrap_or("text").to_string(),
        text,
        root_id: owned(str_at(msg, &["root_id"])),
        parent_id,
        mentioned: mentions.map(|m| !m.is_empty()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionValue {
    pub starfane: u32,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardAction {
    pub event_id: Option<String>,
    pub operator_open_id: String,
    pub chat_id: Option<String>,
    pub message_id: Option<String>,
    pub action_name: Option<String>,
    /// Parsed starfane action value (`None` if absent or not a starfane value).
    pub value: Option<ActionValue>,
    pub form_value: BTreeMap<String, String>,
}

/// JSON that may arrive either as an object or as a JSON-encoded string.
fn json_or_string(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => serde_json::from_str(s).ok(),
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn form_map(v: Option<Value>) -> BTreeMap<String, String> {
    let Some(Value::Object(m)) = v else {
        return BTreeMap::new();
    };
    m.into_iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            (k, s)
        })
        .collect()
}

/// Extract a `card.action.trigger` event (flattened lark-cli output or raw envelope).
pub fn extract_card_action(v: &Value) -> Option<CardAction> {
    let raw = v.get("event").filter(|e| e.get("action").is_some());
    let action = match raw {
        Some(e) => CardAction {
            event_id: owned(str_at(v, &["header", "event_id"])),
            operator_open_id: str_at(e, &["operator", "open_id"])
                .unwrap_or("")
                .to_string(),
            chat_id: owned(str_at(e, &["context", "open_chat_id"])),
            message_id: owned(str_at(e, &["context", "open_message_id"])),
            action_name: owned(str_at(e, &["action", "name"])),
            value: json_or_string(e.get("action").and_then(|a| a.get("value")))
                .and_then(|x| serde_json::from_value::<ActionValue>(x).ok()),
            form_value: form_map(json_or_string(
                e.get("action").and_then(|a| a.get("form_value")),
            )),
        },
        None => CardAction {
            event_id: owned(str_at(v, &["event_id"])),
            operator_open_id: str_at(v, &["operator_id"]).unwrap_or("").to_string(),
            chat_id: owned(str_at(v, &["chat_id"])),
            message_id: owned(str_at(v, &["message_id"])),
            action_name: owned(str_at(v, &["action_name"])),
            value: json_or_string(v.get("action_value"))
                .and_then(|x| serde_json::from_value::<ActionValue>(x).ok()),
            form_value: form_map(json_or_string(v.get("form_value"))),
        },
    };
    if action.operator_open_id.is_empty() && action.value.is_none() && action.action_name.is_none()
    {
        return None;
    }
    Some(action)
}

/// The effective starfane action: the callback value, or a form submit button whose
/// name encodes `bc_<nonce>_<pipeline_id>` or `rs_<nonce>_<session_id>`.
pub fn resolve_card_value(action: &CardAction) -> Option<ActionValue> {
    if let Some(v) = &action.value {
        return (v.starfane == 1).then(|| v.clone());
    }
    let name = action.action_name.as_deref()?;
    if let Some(rest) = name.strip_prefix(FORM_CONFIRM_PREFIX) {
        let (nonce, pid) = rest.split_once('_').unwrap_or((rest, ""));
        return Some(ActionValue {
            starfane: 1,
            op: "confirm_pipeline".into(),
            pipeline_id: (!pid.is_empty()).then(|| pid.to_string()),
            review_id: None,
            finding_id: None,
            session_id: None,
            pane_id: None,
            nonce: nonce.to_string(),
        });
    }
    if let Some(rest) = name.strip_prefix(FORM_REPLY_PREFIX) {
        let (nonce, sid) = rest.split_once('_').unwrap_or((rest, ""));
        return Some(ActionValue {
            starfane: 1,
            op: "reply_session".into(),
            pipeline_id: None,
            review_id: None,
            finding_id: None,
            session_id: (!sid.is_empty()).then(|| sid.to_string()),
            pane_id: None,
            nonce: nonce.to_string(),
        });
    }
    None
}

/// `{stage: shorthand}` form values → overrides (non-stage keys are ignored).
pub fn parse_overrides(
    form: &BTreeMap<String, String>,
) -> Result<BTreeMap<StageKind, StageBinding>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in form {
        let Some(stage) = StageKind::parse(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let binding = StageBinding::from_shorthand(value).map_err(|e| format!("{key}: {e}"))?;
        out.insert(stage, binding);
    }
    Ok(out)
}

/// Keep only overrides that differ from the pipeline's current bindings, so an
/// untouched select never clobbers fields the shorthand cannot express.
pub fn retain_changed(overrides: &mut BTreeMap<StageKind, StageBinding>, spec: &PipelineSpec) {
    overrides.retain(|stage, b| {
        spec.stages
            .iter()
            .find(|s| s.id == *stage)
            .is_none_or(|s| s.binding.to_shorthand() != b.to_shorthand())
    });
}

/// Map a card op onto a daemon command (contracts §7.4).
pub fn card_op_command(
    value: &ActionValue,
    pipeline_id: Option<PipelineId>,
    form_value: &BTreeMap<String, String>,
    operator: &str,
) -> Result<DaemonCommand, String> {
    let need_pid = || {
        pipeline_id
            .clone()
            .ok_or_else(|| format!("{}：缺少 pipeline_id", value.op))
    };
    let action = |a: PipelineAction| -> Result<DaemonCommand, String> {
        Ok(DaemonCommand::PipelineAction {
            pipeline_id: need_pid()?,
            action: a,
        })
    };
    let feedback = |label: FeedbackLabel| -> Result<DaemonCommand, String> {
        let review_id = value
            .review_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or("反馈缺少 review_id")?;
        let finding_id = value
            .finding_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or("反馈缺少 finding_id")?;
        Ok(DaemonCommand::ReviewFeedback {
            review_id: ReviewId::new(review_id),
            finding_id: FindingId::new(finding_id),
            label,
            note: None,
            by: operator.to_string(),
        })
    };
    match value.op.as_str() {
        "confirm_pipeline" => {
            let overrides = parse_overrides(form_value)?;
            Ok(DaemonCommand::ConfirmPipeline {
                pipeline_id: need_pid()?,
                overrides: (!overrides.is_empty()).then_some(overrides),
            })
        }
        "cancel_pipeline" => action(PipelineAction::Cancel),
        "approve_plan" => action(PipelineAction::ApprovePlan),
        "request_changes" => {
            let note = form_value
                .get("note")
                .map(|n| sanitize_text(n))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "飞书卡片：请修改计划".to_string());
            action(PipelineAction::RequestChanges { note })
        }
        "continue_deep" => action(PipelineAction::ContinueDeep),
        "stop_deep" => action(PipelineAction::StopDeep),
        "adopt_mitigation" => action(PipelineAction::AdoptMitigation),
        "feedback_valid" => feedback(FeedbackLabel::Valid),
        "feedback_fp" => feedback(FeedbackLabel::FalsePositive),
        "feedback_wontfix" => feedback(FeedbackLabel::Wontfix),
        "reply_session" => {
            let reply_text = form_value
                .get("reply_text")
                .map(|s| sanitize_text(s))
                .filter(|s| !s.is_empty())
                .ok_or("回复内容为空")?;
            let target = if let Some(sid) = value
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                DispatchTarget::Session {
                    id: SessionId::new(sid),
                }
            } else if let Some(pid) = value
                .pane_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                DispatchTarget::Pane {
                    id: PaneId::new(pid),
                }
            } else {
                return Err("reply_session：缺少目标会话".into());
            };
            Ok(DaemonCommand::Dispatch {
                target,
                text: reply_text,
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
        }
        other => Err(format!("未知卡片操作 `{other}`")),
    }
}

/// Operator / chat allow-list check. `chat_id = None` (card callbacks without
/// context) skips the chat check; the configured notice chat counts as allowed.
pub fn authorize(
    cfg: &FeishuConfig,
    open_id: &str,
    chat_id: Option<&str>,
    p2p: bool,
) -> Result<(), String> {
    if open_id.is_empty() || !cfg.allow_open_ids.iter().any(|a| a == open_id) {
        return Err(format!("operator `{open_id}` not in allow_open_ids"));
    }
    if p2p {
        return Ok(());
    }
    match chat_id {
        None => Ok(()),
        Some("") => Err("missing chat_id".into()),
        Some(c) if cfg.allow_chat_ids.iter().any(|a| a == c) || cfg.chat_id == c => Ok(()),
        Some(c) => Err(format!("chat `{c}` not in allow_chat_ids")),
    }
}

/// Bounded "seen" set for nonces and message ids.
#[derive(Debug)]
pub struct Dedupe {
    cap: usize,
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Dedupe {
    pub fn new(cap: usize) -> Self {
        Dedupe {
            cap: cap.max(1),
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// `true` if `key` was not seen before (and is now remembered).
    pub fn insert(&mut self, key: String) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        self.set.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// Find the message id in lark-cli JSON stdout.
pub fn extract_message_id(stdout: &str) -> Option<String> {
    fn find(v: &Value) -> Option<String> {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(s)) = m.get("message_id") {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
                m.values().find_map(find)
            }
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    if let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) {
        return find(&v);
    }
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find_map(|v| find(&v))
}

/// Stage a thread reply should go to: the active one, else the next pending.
pub fn current_stage(spec: &PipelineSpec) -> Option<StageKind> {
    let active = [
        StageStatus::Running,
        StageStatus::AwaitingHuman,
        StageStatus::Blocked,
    ];
    spec.stages
        .iter()
        .find(|s| active.contains(&s.status))
        .or_else(|| {
            spec.stages
                .iter()
                .find(|s| s.status == StageStatus::Pending)
        })
        .or_else(|| spec.stages.last())
        .map(|s| s.id)
}

fn is_finished(status: PipelineStatus) -> bool {
    matches!(
        status,
        PipelineStatus::Done | PipelineStatus::Cancelled | PipelineStatus::Failed
    )
}

pub fn format_pipeline(spec: &PipelineSpec) -> String {
    let mut out = format!(
        "**`{}`** {} · {}\n目标：{}\n项目：`{}`",
        spec.id,
        spec.playbook.as_str(),
        snake(&spec.status),
        truncate_chars(&spec.goal, 200),
        spec.project.root
    );
    for s in &spec.stages {
        out.push_str(&format!(
            "\n- {} · {} · `{}`",
            s.id,
            snake(&s.status),
            s.binding.to_shorthand()
        ));
        if let Some(sid) = &s.session_id {
            out.push_str(&format!(" · session `{sid}`"));
        }
        if s.round > 0 {
            out.push_str(&format!(" · round {}", s.round));
        }
    }
    out
}

pub fn format_state(snap: &StateSnapshot) -> String {
    let active: Vec<&PipelineSpec> = snap
        .pipelines
        .iter()
        .filter(|p| !is_finished(p.status))
        .collect();
    let mut out = format!("**Pipelines**（进行中 {}）", active.len());
    for p in &active {
        let stage = current_stage(p).map_or("-", StageKind::as_str);
        out.push_str(&format!(
            "\n- `{}` {} · {} · {} — {}",
            p.id,
            p.playbook.as_str(),
            snake(&p.status),
            stage,
            truncate_chars(&p.goal, GOAL_PREVIEW_CHARS)
        ));
    }
    let mut by_state: BTreeMap<String, usize> = BTreeMap::new();
    for s in snap.sessions.iter().filter(|s| !s.is_subagent) {
        *by_state.entry(snake(&s.state)).or_default() += 1;
    }
    let total: usize = by_state.values().sum();
    let breakdown = by_state
        .iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(" · ");
    out.push_str(&format!("\n**Sessions** {total}：{breakdown}"));
    if !snap.attention.is_empty() {
        out.push_str(&format!("\n**待处理** {}", snap.attention.len()));
        for a in &snap.attention {
            out.push_str(&format!(
                "\n- [{}] {}",
                a.kind,
                truncate_chars(&a.title, 80)
            ));
        }
    }
    out
}

fn binding_summary(bindings: &BTreeMap<StageKind, StageBinding>) -> String {
    bindings
        .iter()
        .map(|(k, b)| format!("{k}=`{}`", b.to_shorthand()))
        .collect::<Vec<_>>()
        .join(" · ")
}

// ===========================================================================
// lark-cli wrapper
// ===========================================================================

/// Thin async wrapper over the `lark-cli` binary.
#[derive(Debug, Clone)]
pub struct LarkCli {
    bin: String,
    chat_id: String,
}

fn is_transient_cli_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("eof")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("timed out")
        || lower.contains("timeout")
}

fn is_missing_root_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("withdrawn")
        || lower.contains("recalled")
        || lower.contains("deleted")
        || lower.contains("not found")
        || lower.contains("not exist")
        || lower.contains("230002")
        || lower.contains("231003")
}

impl LarkCli {
    pub fn new(cfg: &FeishuConfig) -> Self {
        let bin = cfg.lark_cli.trim();
        LarkCli {
            bin: if bin.is_empty() {
                "lark-cli".into()
            } else {
                bin.to_string()
            },
            chat_id: cfg.chat_id.trim().to_string(),
        }
    }

    async fn exec_once(&self, args: &[String]) -> Result<String, String> {
        let verb = args.get(1).map_or("", String::as_str);
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(LARK_CMD_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("failed to run {} {verb}: {e}", self.bin)),
            Err(_) => {
                return Err(format!(
                    "{} {verb} timed out after {LARK_CMD_TIMEOUT:?}",
                    self.bin
                ))
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr
                .chars()
                .rev()
                .take(STDERR_TAIL_CHARS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(format!(
                "{} {verb} exited with {}: {} {}",
                self.bin,
                output.status,
                tail.trim(),
                truncate_bytes(stdout.trim(), STDERR_TAIL_CHARS)
            ));
        }
        Ok(stdout)
    }

    /// Run `lark-cli <args>` and return stdout; retries once after 300ms on transient transport errors.
    async fn exec(&self, args: &[String]) -> Result<String, String> {
        match self.exec_once(args).await {
            Ok(out) => Ok(out),
            Err(first) if is_transient_cli_error(&first) => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                self.exec_once(args).await
            }
            Err(e) => Err(e),
        }
    }

    async fn exec_for_id(&self, args: Vec<String>) -> Result<String, String> {
        let stdout = self.exec(&args).await?;
        extract_message_id(&stdout).ok_or_else(|| {
            format!(
                "no message_id in lark-cli output: {}",
                truncate_bytes(stdout.trim(), 200)
            )
        })
    }

    fn base(verb: &str) -> Vec<String> {
        vec!["im".into(), verb.into(), "--as".into(), "bot".into()]
    }

    pub async fn send_card(
        &self,
        chat_id: &str,
        card: &Value,
        key: &str,
    ) -> Result<String, String> {
        let mut a = Self::base("+messages-send");
        a.extend([
            "--chat-id".into(),
            chat_id.to_string(),
            "--msg-type".into(),
            "interactive".into(),
            "--content".into(),
            card.to_string(),
            "--idempotency-key".into(),
            idempotency_key(key),
        ]);
        self.exec_for_id(a).await
    }

    pub async fn reply_card(&self, root: &str, card: &Value, key: &str) -> Result<String, String> {
        let mut a = Self::base("+messages-reply");
        a.extend([
            "--message-id".into(),
            root.to_string(),
            "--reply-in-thread".into(),
            "--msg-type".into(),
            "interactive".into(),
            "--content".into(),
            card.to_string(),
            "--idempotency-key".into(),
            idempotency_key(key),
        ]);
        self.exec_for_id(a).await
    }

    pub async fn reply_markdown(
        &self,
        message_id: &str,
        text: &str,
        in_thread: bool,
    ) -> Result<String, String> {
        let mut a = Self::base("+messages-reply");
        a.extend(["--message-id".into(), message_id.to_string()]);
        if in_thread {
            a.push("--reply-in-thread".into());
        }
        a.extend([
            "--markdown".into(),
            flag_safe(truncate_bytes(text, MAX_REPLY_BYTES)),
        ]);
        self.exec_for_id(a).await
    }

    pub async fn send_markdown(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let mut a = Self::base("+messages-send");
        a.extend([
            "--chat-id".into(),
            chat_id.to_string(),
            "--markdown".into(),
            flag_safe(truncate_bytes(text, MAX_REPLY_BYTES)),
        ]);
        self.exec_for_id(a).await
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let mut a = Self::base("+messages-send");
        a.extend([
            "--chat-id".into(),
            chat_id.to_string(),
            "--text".into(),
            flag_safe(truncate_bytes(text, MAX_REPLY_BYTES)),
        ]);
        self.exec_for_id(a).await
    }
}

impl FeishuSender for LarkCli {
    fn send_test<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<String, ApiError>> {
        Box::pin(async move {
            if self.chat_id.is_empty() {
                return Err(ApiError::bad_request("feishu.chat_id is not configured"));
            }
            let card = build_test_card(text);
            let key = format!("test-{}", starfane_proto::id::now_ms());
            self.send_card(&self.chat_id, &card, &key)
                .await
                .map_err(|e| ApiError::new(ErrorCode::Unavailable, e))
        })
    }
}

// ===========================================================================
// Inbound consumers
// ===========================================================================

#[derive(Debug)]
struct Inbound {
    key: &'static str,
    payload: Value,
}

/// Sends SIGTERM to the child on drop unless disarmed (tokio's `start_kill`
/// is SIGKILL, which would not let lark-cli unsubscribe cleanly).
struct TermGuard {
    pid: Option<u32>,
}

impl TermGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => log::debug!("feishu: sent SIGTERM to lark-cli pid {pid}"),
            Ok(s) => log::warn!("feishu: kill -TERM {pid} exited with {s}"),
            Err(e) => log::warn!("feishu: kill -TERM {pid} failed: {e}"),
        }
    }
}

async fn forward_stderr(
    key: &'static str,
    stderr: tokio::process::ChildStderr,
    ready: oneshot::Sender<()>,
) {
    let mut ready = Some(ready);
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.contains(READY_MARKER) {
                    if let Some(tx) = ready.take() {
                        // The receiver may have given up waiting; nothing to do then.
                        if tx.send(()).is_err() {
                            log::debug!("feishu: `{key}` ready after waiter left");
                        }
                    }
                }
                log::debug!("feishu: lark-cli[{key}] {line}");
            }
            Ok(None) => return,
            Err(e) => {
                log::warn!("feishu: reading lark-cli[{key}] stderr failed: {e}");
                return;
            }
        }
    }
}

/// Run one `lark-cli event consume` process until it exits or `tx` closes.
async fn consume_once(
    bin: &str,
    key: &'static str,
    tx: &mpsc::UnboundedSender<Inbound>,
) -> Result<(), String> {
    let mut child = Command::new(bin)
        .args(["event", "consume", key, "--as", "bot"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
    let mut guard = TermGuard { pid: child.id() };
    // Kept open for the whole run: lark-cli stops consuming on stdin EOF.
    let _stdin = child.stdin.take().ok_or("child stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("child stderr unavailable")?;
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(forward_stderr(key, stderr, ready_tx));

    match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
        Ok(Ok(())) => log::info!("feishu: consuming `{key}`"),
        Ok(Err(_)) => {
            let status = child.wait().await.map_err(|e| e.to_string())?;
            guard.disarm();
            return Err(format!("exited before ready ({status})"));
        }
        Err(_) => return Err(format!("not ready within {READY_TIMEOUT:?}")),
    }

    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(payload) => {
                        if tx.send(Inbound { key, payload }).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => log::warn!(
                        "feishu: bad NDJSON from `{key}`: {e}: {}",
                        truncate_bytes(line, 200)
                    ),
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("reading stdout failed: {e}")),
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    guard.disarm();
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

async fn consume_forever(bin: String, key: &'static str, tx: mpsc::UnboundedSender<Inbound>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        match consume_once(&bin, key, &tx).await {
            Ok(()) => log::warn!("feishu: `{key}` consumer exited"),
            Err(e) => log::warn!("feishu: `{key}` consumer failed: {e}"),
        }
        if tx.is_closed() {
            return;
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = BACKOFF_MIN;
        }
        log::info!("feishu: restarting `{key}` consumer in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

// ===========================================================================
// Adapter
// ===========================================================================

struct Adapter {
    core: Core,
    lark: Arc<LarkCli>,
    cfg: FeishuConfig,
    seen: Dedupe,
}

fn api_msg(e: ApiError) -> String {
    e.message
}

impl Adapter {
    async fn on_notice(&mut self, notice: Notice) {
        let now = starfane_proto::id::now_ms();
        if is_muted(self.core.feishu_muted_until(), now) {
            log::debug!("feishu: muted, skipping notice {}", notice.key);
            return;
        }
        let card = build_card(&notice, &mut new_nonce);
        let key = idempotency_key(&notice.key);
        let thread = notice
            .pipeline_id
            .as_ref()
            .and_then(|p| self.core.feishu_thread(p));
        let mut replaced_pipeline_root = false;
        if let Some(t) = &thread {
            match self.lark.reply_card(&t.root_message_id, &card, &key).await {
                Ok(mid) => {
                    self.core
                        .record_feishu_message(notice.pipeline_id.as_ref(), &t.chat_id, &mid);
                    if let Some(target) = notice.reply_session {
                        self.core.record_feishu_session_message(&mid, target);
                    }
                    return;
                }
                Err(e) if is_missing_root_error(&e) => {
                    log::warn!(
                        "feishu: pipeline root {} missing for {}, replacing thread root: {e}",
                        t.root_message_id,
                        notice.key
                    );
                    replaced_pipeline_root = true;
                }
                Err(e) => {
                    log::warn!(
                        "feishu: thread reply for notice {} failed transiently (preserving thread): {e}",
                        notice.key
                    );
                    return;
                }
            }
        } else if let Some(target) = notice.reply_session.as_ref() {
            if let Some(root_om) = self.core.feishu_session_thread(&target.session_id) {
                match self.lark.reply_card(&root_om, &card, &key).await {
                    Ok(mid) => {
                        self.core
                            .record_feishu_session_message(&mid, target.clone());
                        // Keep the thread root mapped so any message inside the thread routes here.
                        self.core
                            .record_feishu_session_message(&root_om, target.clone());
                        return;
                    }
                    Err(e) if is_missing_root_error(&e) => log::warn!(
                        "feishu: session thread root {root_om} deleted for {}, sending new top-level card: {e}",
                        target.session_id
                    ),
                    Err(e) => {
                        log::warn!(
                            "feishu: session thread reply for {} (root {root_om}) failed transiently (skipping chat spam): {e}",
                            target.session_id
                        );
                        return;
                    }
                }
            }
        }
        if self.cfg.chat_id.trim().is_empty() {
            log::warn!(
                "feishu: chat_id not configured; dropping notice {}",
                notice.key
            );
            return;
        }
        let chat = self.cfg.chat_id.trim().to_string();
        match self.lark.send_card(&chat, &card, &key).await {
            Ok(mid) => {
                if replaced_pipeline_root {
                    if let Some(p) = notice.pipeline_id.as_ref() {
                        self.core.replace_feishu_thread(p, &chat, &mid);
                    }
                } else {
                    self.core
                        .record_feishu_message(notice.pipeline_id.as_ref(), &chat, &mid);
                }
                if let Some(target) = notice.reply_session {
                    if notice.pipeline_id.is_none() {
                        self.core
                            .record_feishu_session_thread(&target.session_id, &mid);
                    }
                    self.core.record_feishu_session_message(&mid, target);
                }
            }
            Err(e) => log::warn!("feishu: failed to send notice {}: {e}", notice.key),
        }
    }

    async fn on_event(&mut self, ev: Inbound) {
        match ev.key {
            MESSAGE_EVENT => self.on_message(&ev.payload).await,
            CARD_EVENT => self.on_card(&ev.payload).await,
            other => log::warn!("feishu: unexpected event key {other}"),
        }
    }

    fn reject(&self, kind: &str, reason: &str, detail: Value) {
        log::warn!("feishu: rejected {kind}: {reason}");
        self.core.audit(
            AUDIT_SOURCE,
            json!({ "event": format!("{kind}_rejected"), "reason": reason, "detail": detail }),
        );
    }

    async fn on_message(&mut self, payload: &Value) {
        let Some(msg) = extract_message(payload) else {
            log::debug!("feishu: unparseable message event");
            return;
        };
        if msg.sender_type.as_deref() == Some("app") {
            return;
        }
        if !self.seen.insert(format!("m:{}", msg.message_id)) {
            log::debug!("feishu: duplicate message {}", msg.message_id);
            return;
        }
        let detail = json!({
            "open_id": msg.sender_open_id, "chat_id": msg.chat_id,
            "message_id": msg.message_id, "p2p": msg.p2p
        });
        if let Err(reason) = authorize(&self.cfg, &msg.sender_open_id, Some(&msg.chat_id), msg.p2p)
        {
            self.reject("message", &reason, detail);
            return;
        }
        let ctx_session = [msg.parent_id.as_deref(), msg.root_id.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|id| self.core.session_for_feishu_message(id));
        if !msg.p2p && msg.mentioned == Some(false) && ctx_session.is_none() {
            log::debug!("feishu: group message without @bot ignored");
            return;
        }
        if msg.message_type != "text" && msg.message_type != "post" {
            log::debug!("feishu: ignoring {} message", msg.message_type);
            return;
        }
        let text = sanitize_text(&msg.text);
        self.core.audit(
            AUDIT_SOURCE,
            json!({ "event": "message", "text": text, "detail": detail }),
        );
        let ctx_pipeline = [msg.root_id.as_deref(), msg.parent_id.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|id| self.core.pipeline_for_message(id));
        let is_quote_reply = msg.root_id.is_some() || msg.parent_id.is_some();
        let explicit_slash = text.trim_start().starts_with('/');
        let parsed = parse_text_command(&text);
        let starts_thread = matches!(parsed, Ok(TextCommand::Start { .. }));
        let reply = match (parsed, ctx_session) {
            // In quote-replies to a session card, forward natural text unless prefixed with `/`.
            (Ok(cmd), Some(target)) if is_quote_reply && !explicit_slash => {
                if !text.is_empty() {
                    self.forward_quote_reply(&target, text).await
                } else {
                    self.exec_text(cmd, &msg, ctx_pipeline).await
                }
            }
            (Ok(cmd), _) => self.exec_text(cmd, &msg, ctx_pipeline).await,
            (Err(_), Some(target)) if !text.is_empty() => {
                self.forward_quote_reply(&target, text).await
            }
            (Err(e), _) => Err(e),
        };
        let body = match reply {
            Ok(s) => s,
            Err(e) => format!("❌ {e}"),
        };
        let in_thread = msg.root_id.is_some() || !msg.p2p || starts_thread;
        if let Err(e) = self
            .lark
            .reply_markdown(&msg.message_id, &body, in_thread)
            .await
        {
            log::warn!("feishu: reply to {} failed: {e}", msg.message_id);
        }
    }

    async fn get_pipeline(&self, pid: &PipelineId) -> Result<PipelineSpec, String> {
        let v = self
            .core
            .handle(DaemonCommand::GetPipeline {
                pipeline_id: pid.clone(),
            })
            .await
            .map_err(api_msg)?;
        serde_json::from_value(v).map_err(|e| format!("bad pipeline data: {e}"))
    }

    async fn run_cmd(&self, cmd: DaemonCommand) -> Result<Value, String> {
        self.core.handle(cmd).await.map_err(api_msg)
    }

    async fn forward_quote_reply(
        &self,
        target: &crate::notice::SessionReplyTarget,
        text: String,
    ) -> Result<String, String> {
        let dispatch_target = DispatchTarget::Session {
            id: target.session_id.clone(),
        };
        let res = self
            .run_cmd(DaemonCommand::Dispatch {
                target: dispatch_target,
                text: text.clone(),
                attachments: Vec::new(),
                allow: Vec::new(),
                interactive: true,
            })
            .await;
        // If the session id was upgraded/rotated after card creation, fall back to its pane id.
        if res.is_err() {
            if let Some(pane) = &target.pane_id {
                self.run_cmd(DaemonCommand::Dispatch {
                    target: DispatchTarget::Pane { id: pane.clone() },
                    text,
                    attachments: Vec::new(),
                    allow: Vec::new(),
                    interactive: true,
                })
                .await?;
            } else {
                res?;
            }
        }
        Ok(format_forward_confirmation(target))
    }

    async fn exec_text(
        &mut self,
        cmd: TextCommand,
        msg: &InboundMessage,
        ctx: Option<PipelineId>,
    ) -> Result<String, String> {
        let pick = |explicit: Option<PipelineId>| {
            explicit
                .or_else(|| ctx.clone())
                .ok_or_else(|| "请指定 P-id，或在 pipeline 话题内回复".to_string())
        };
        match cmd {
            TextCommand::Help => Ok(HELP_TEXT.to_string()),
            TextCommand::Status { pipeline_id: None } => {
                let v = self.run_cmd(DaemonCommand::GetState).await?;
                let snap: StateSnapshot =
                    serde_json::from_value(v).map_err(|e| format!("bad state data: {e}"))?;
                Ok(format_state(&snap))
            }
            TextCommand::Status {
                pipeline_id: Some(pid),
            } => Ok(format_pipeline(&self.get_pipeline(&pid).await?)),
            TextCommand::Start {
                playbook,
                project,
                goal,
                utterance,
            } => {
                self.start_pipeline(playbook, &project, &goal, utterance, msg)
                    .await
            }
            TextCommand::Reply { target, text } => {
                let target = match target {
                    Some(ReplyTarget::Pane(id)) => DispatchTarget::Pane { id },
                    Some(ReplyTarget::Session(id)) => DispatchTarget::Session { id },
                    Some(ReplyTarget::Stage { pipeline_id, stage }) => {
                        DispatchTarget::Stage { pipeline_id, stage }
                    }
                    None => {
                        let pid = ctx.clone().ok_or(
                            "请指定目标：`%12`、`s:<session>` 或 `P-…/<stage>`，或在 pipeline 话题内回复",
                        )?;
                        let spec = self.get_pipeline(&pid).await?;
                        let stage = current_stage(&spec).ok_or("该 pipeline 没有可投递的阶段")?;
                        DispatchTarget::Stage {
                            pipeline_id: pid,
                            stage,
                        }
                    }
                };
                let report = self
                    .run_cmd(DaemonCommand::Dispatch {
                        target,
                        text,
                        attachments: Vec::new(),
                        allow: Vec::new(),
                        interactive: false,
                    })
                    .await?;
                Ok(format!(
                    "📨 已投递：{} · {}",
                    report["level"].as_str().unwrap_or("?"),
                    report["status"].as_str().unwrap_or("?")
                ))
            }
            TextCommand::Confirm { pipeline_id } => {
                let pid = pick(pipeline_id)?;
                let spec = self.get_pipeline(&pid).await?;
                let stage_waiting = |k: StageKind| {
                    spec.stages
                        .iter()
                        .any(|s| s.id == k && s.status == StageStatus::AwaitingHuman)
                };
                let (cmd, done) = match spec.status {
                    PipelineStatus::AwaitingBindingConfirm => (
                        DaemonCommand::ConfirmPipeline {
                            pipeline_id: pid.clone(),
                            overrides: None,
                        },
                        "已确认绑定，开始运行",
                    ),
                    PipelineStatus::AwaitingHuman if stage_waiting(StageKind::Plan) => (
                        DaemonCommand::PipelineAction {
                            pipeline_id: pid.clone(),
                            action: PipelineAction::ApprovePlan,
                        },
                        "已批准计划",
                    ),
                    PipelineStatus::AwaitingHuman if stage_waiting(StageKind::Triage) => (
                        DaemonCommand::PipelineAction {
                            pipeline_id: pid.clone(),
                            action: PipelineAction::ContinueDeep,
                        },
                        "已继续深挖",
                    ),
                    other => {
                        return Err(format!(
                            "`{pid}` 当前状态为 {}，没有待确认的事项",
                            snake(&other)
                        ))
                    }
                };
                self.run_cmd(cmd).await?;
                Ok(format!("✅ `{pid}` {done}"))
            }
            TextCommand::Reject {
                pipeline_id,
                reason,
            } => {
                let pid = pick(pipeline_id)?;
                let spec = self.get_pipeline(&pid).await?;
                let (action, done) = match spec.status {
                    PipelineStatus::AwaitingBindingConfirm => (PipelineAction::Cancel, "已取消"),
                    PipelineStatus::AwaitingHuman => (
                        PipelineAction::RequestChanges {
                            note: reason.unwrap_or_else(|| "飞书拒绝（未注明理由）".into()),
                        },
                        "已要求修改",
                    ),
                    other => {
                        return Err(format!(
                            "`{pid}` 当前状态为 {}，没有可拒绝的事项",
                            snake(&other)
                        ))
                    }
                };
                self.run_cmd(DaemonCommand::PipelineAction {
                    pipeline_id: pid.clone(),
                    action,
                })
                .await?;
                Ok(format!("✅ `{pid}` {done}"))
            }
            TextCommand::Stop { pipeline_id } => {
                self.run_cmd(DaemonCommand::PipelineAction {
                    pipeline_id: pipeline_id.clone(),
                    action: PipelineAction::Cancel,
                })
                .await?;
                Ok(format!("🛑 `{pipeline_id}` 已停止"))
            }
            TextCommand::Mute { secs } => {
                let until = starfane_proto::id::now_ms().saturating_add(secs.saturating_mul(1000));
                self.core.set_feishu_mute(Some(until));
                Ok(format!(
                    "🔕 飞书推送已静音 {} 分钟（Web 不受影响）",
                    secs.div_ceil(60)
                ))
            }
            TextCommand::MuteUntil { hour, minute } => {
                let now_ms = starfane_proto::id::now_ms();
                let until = resolve_hhmm_until_ms(hour, minute, now_ms, local_tz_offset_secs());
                self.core.set_feishu_mute(Some(until));
                let mins = until.saturating_sub(now_ms).div_ceil(60_000);
                Ok(format!(
                    "🔕 飞书推送已静音至 {:02}:{:02}（约 {mins} 分钟，Web 不受影响）",
                    hour, minute
                ))
            }
            TextCommand::Unmute => {
                self.core.set_feishu_mute(None);
                Ok("🔔 已取消静音".into())
            }
        }
    }

    async fn start_pipeline(
        &mut self,
        playbook: PlaybookKind,
        project: &str,
        goal: &str,
        utterance: Option<String>,
        msg: &InboundMessage,
    ) -> Result<String, String> {
        let root = self.core.find_project(project).map_err(api_msg)?;
        let text = match &utterance {
            Some(u) => format!("{goal}；{u}"),
            None => goal.to_string(),
        };
        let parsed = self
            .run_cmd(DaemonCommand::ParseUtterance {
                text: text.clone(),
                playbook: Some(playbook),
                project: Some(root.clone()),
            })
            .await?;
        let mut bindings: BTreeMap<StageKind, StageBinding> = match parsed.get("resolved") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| format!("bad parse_utterance result: {e}"))?,
        };
        bindings.retain(|stage, _| stage.playbook() == playbook);
        let warnings = parsed
            .get("warnings")
            .and_then(Value::as_array)
            .map(|w| {
                w.iter()
                    .map(|x| {
                        x.get("warning")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let summary = binding_summary(&bindings);
        let req = CreatePipelineReq {
            playbook,
            project_root: root.clone(),
            goal: goal.to_string(),
            bindings,
            options: None,
            source: PipelineSource {
                via: Via::Feishu,
                utterance: Some(text),
                operator: Some(msg.sender_open_id.clone()),
            },
        };
        let spec = self.run_cmd(DaemonCommand::CreatePipeline { req }).await?;
        let pid = spec
            .get("id")
            .and_then(Value::as_str)
            .ok_or("create_pipeline returned no id")?;
        let pid = PipelineId::new(pid);
        // The user's message becomes the thread root for this pipeline.
        self.core
            .record_feishu_message(Some(&pid), &msg.chat_id, &msg.message_id);
        let mut out = format!(
            "🆕 已创建 `{pid}`（{}）\n项目：`{root}`\n绑定：{summary}\n请在确认卡片中确认，或回复「确认」。",
            playbook.as_str()
        );
        if !warnings.is_empty() {
            out.push_str(&format!("\n⚠️ 绑定警告：{}", warnings.join("、")));
        }
        Ok(out)
    }

    async fn on_card(&mut self, payload: &Value) {
        let Some(action) = extract_card_action(payload) else {
            log::debug!("feishu: unparseable card action");
            return;
        };
        let Some(value) = resolve_card_value(&action) else {
            log::debug!("feishu: card action without a starfane value ignored");
            return;
        };
        let detail = json!({
            "open_id": action.operator_open_id, "chat_id": action.chat_id,
            "message_id": action.message_id, "op": value.op, "nonce": value.nonce
        });
        if let Err(reason) = authorize(
            &self.cfg,
            &action.operator_open_id,
            action.chat_id.as_deref(),
            false,
        ) {
            self.reject("card_action", &reason, detail);
            return;
        }
        if value.nonce.is_empty() {
            self.reject("card_action", "missing nonce", detail);
            return;
        }
        let dedupe_key = if value.op == "reply_session" {
            if let Some(ev_id) = action.event_id.as_deref().filter(|s| !s.is_empty()) {
                format!("e:{ev_id}")
            } else {
                let txt = action
                    .form_value
                    .get("reply_text")
                    .map(String::as_str)
                    .unwrap_or("");
                format!("rs:{}:{txt}", value.nonce)
            }
        } else {
            format!("n:{}", value.nonce)
        };
        if !self.seen.insert(dedupe_key) {
            log::info!("feishu: duplicate card nonce {} ignored", value.nonce);
            return;
        }
        self.core.audit(
            AUDIT_SOURCE,
            json!({ "event": "card_action", "detail": detail }),
        );
        let pipeline_id = value
            .pipeline_id
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(PipelineId::new)
            .or_else(|| {
                action
                    .message_id
                    .as_deref()
                    .and_then(|m| self.core.pipeline_for_message(m))
            });
        let result = match card_op_command(
            &value,
            pipeline_id.clone(),
            &action.form_value,
            &action.operator_open_id,
        ) {
            Ok(cmd) => {
                let cmd = self.narrow_overrides(cmd).await;
                self.run_cmd(cmd).await
            }
            Err(e) => Err(e),
        };
        let pid_text = pipeline_id
            .as_ref()
            .map_or(String::new(), |p| format!(" `{p}`"));
        let text = match result {
            Ok(_) if value.op == "reply_session" => {
                if let Some(target) = action
                    .message_id
                    .as_deref()
                    .and_then(|m| self.core.session_for_feishu_message(m))
                {
                    format_forward_confirmation(&target)
                } else {
                    "✅ 已转发给 Agent".to_string()
                }
            }
            Ok(_) => format!("✅ {}{pid_text}", value.op),
            Err(e) => format!("❌ {}{pid_text}：{e}", value.op),
        };
        self.reply_for_card(&action, pipeline_id.as_ref(), &text)
            .await;
    }

    /// For ConfirmPipeline, drop overrides equal to the current bindings.
    async fn narrow_overrides(&self, cmd: DaemonCommand) -> DaemonCommand {
        let DaemonCommand::ConfirmPipeline {
            pipeline_id,
            overrides: Some(mut overrides),
        } = cmd
        else {
            return cmd;
        };
        match self.get_pipeline(&pipeline_id).await {
            Ok(spec) => retain_changed(&mut overrides, &spec),
            Err(e) => log::warn!("feishu: cannot diff overrides for {pipeline_id}: {e}"),
        }
        DaemonCommand::ConfirmPipeline {
            pipeline_id,
            overrides: (!overrides.is_empty()).then_some(overrides),
        }
    }

    async fn reply_for_card(&self, action: &CardAction, pid: Option<&PipelineId>, text: &str) {
        let result = if let Some(mid) = &action.message_id {
            self.lark.reply_markdown(mid, text, true).await
        } else if let Some(t) = pid.and_then(|p| self.core.feishu_thread(p)) {
            self.lark
                .reply_markdown(&t.root_message_id, text, true)
                .await
        } else if !self.cfg.chat_id.trim().is_empty() {
            self.lark.send_markdown(self.cfg.chat_id.trim(), text).await
        } else {
            Err("no reply target".to_string())
        };
        if let Err(e) = result {
            log::warn!("feishu: card action reply failed: {e}");
        }
    }
}

fn format_forward_confirmation(target: &crate::notice::SessionReplyTarget) -> String {
    let lbl = target
        .tmux_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pane = target.pane_id.as_ref().map(|p| p.as_str());
    match (lbl, pane) {
        (Some(l), Some(p)) => format!("✅ 已转发给 {} · {l} ({p})", target.agent),
        (Some(l), None) => format!("✅ 已转发给 {} · {l}", target.agent),
        (None, Some(p)) => format!("✅ 已转发给 {}@{p}", target.agent),
        (None, None) => format!("✅ 已转发给 {} ({})", target.agent, target.session_id),
    }
}

async fn recv_notice(rx: &mut Option<mpsc::UnboundedReceiver<Notice>>) -> Option<Notice> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Run the Feishu adapter. Returns immediately when `[feishu].enabled` is false.
pub async fn run(core: Core) {
    let cfg = core.config().feishu.clone();
    if !cfg.enabled {
        log::info!("feishu: disabled");
        return;
    }
    if cfg.allow_open_ids.is_empty() {
        log::warn!("feishu: allow_open_ids is empty; every inbound command will be rejected");
    }
    let lark = Arc::new(LarkCli::new(&cfg));
    core.set_feishu(lark.clone());
    let mut notices = core.take_notice_rx();
    if notices.is_none() {
        log::warn!("feishu: notice receiver already taken; outbound cards disabled");
    }
    let (tx, mut events) = mpsc::unbounded_channel::<Inbound>();
    for key in [MESSAGE_EVENT, CARD_EVENT] {
        tokio::spawn(consume_forever(lark.bin.clone(), key, tx.clone()));
    }
    drop(tx);
    let mut adapter = Adapter {
        core,
        lark,
        cfg,
        seen: Dedupe::new(DEDUPE_CAPACITY),
    };
    let mut events_open = true;
    loop {
        // Notices and inbound events are handled sequentially so a pipeline
        // created from Feishu records its thread before its first card is sent.
        tokio::select! {
            n = recv_notice(&mut notices) => match n {
                Some(n) => adapter.on_notice(n).await,
                None => {
                    log::warn!("feishu: notice channel closed");
                    notices = None;
                }
            },
            ev = events.recv(), if events_open => match ev {
                Some(ev) => adapter.on_event(ev).await,
                None => {
                    log::warn!("feishu: all event consumers stopped");
                    events_open = false;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notice::NoticeAction;

    fn notice(kind: NoticeKind) -> Notice {
        Notice {
            key: "P-1-binding_confirm".into(),
            kind,
            title: "确认绑定".into(),
            body_md: "**goal**: add button".into(),
            pipeline_id: Some(PipelineId::new("P-1")),
            session_id: None,
            review_id: None,
            actions: vec![],
            binding_form: vec![],
            reply_session: None,
        }
    }

    fn act(op: &str, style: ActionStyle) -> NoticeAction {
        NoticeAction {
            op: op.into(),
            label: op.into(),
            style,
            finding_id: None,
        }
    }

    fn counter() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("NONCE{n:03}")
        }
    }

    fn feishu_cfg() -> FeishuConfig {
        FeishuConfig {
            enabled: true,
            lark_cli: "lark-cli".into(),
            chat_id: "oc_main".into(),
            allow_open_ids: vec!["ou_me".into()],
            allow_chat_ids: vec!["oc_team".into()],
        }
    }

    #[test]
    fn sanitize_strips_controls_and_folds_newlines() {
        assert_eq!(
            sanitize_text("  hi\x1b[31m\nthere\r\n\tyou  "),
            "hi[31m there you"
        );
        assert_eq!(sanitize_text("a\u{202E}b\u{200B}c\u{7}"), "abc");
        assert_eq!(sanitize_text("\n\n"), "");
    }

    #[test]
    fn sanitize_caps_at_2kb_on_char_boundary() {
        let long = "汉".repeat(2000);
        let s = sanitize_text(&long);
        assert!(s.len() <= MAX_TEXT_BYTES);
        assert_eq!(s.len() % 3, 0);
        assert_eq!(sanitize_text(&"a".repeat(5000)).len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn idempotency_key_and_flag_safety() {
        let k = idempotency_key(&"x".repeat(80));
        assert_eq!(k.len(), MAX_IDEMPOTENCY_KEY);
        assert_eq!(idempotency_key("short"), "short");
        assert!(idempotency_key(&"键".repeat(30)).len() <= MAX_IDEMPOTENCY_KEY);
        assert!(flag_safe("@/etc/passwd").starts_with('\u{200B}'));
        assert!(flag_safe("-").starts_with('\u{200B}'));
        assert_eq!(flag_safe("ok"), "ok");
    }

    #[test]
    fn parse_help_status_and_case() {
        assert_eq!(parse_text_command("帮助"), Ok(TextCommand::Help));
        assert_eq!(parse_text_command("HELP"), Ok(TextCommand::Help));
        assert_eq!(parse_text_command("/help"), Ok(TextCommand::Help));
        assert_eq!(
            parse_text_command("ls"),
            Ok(TextCommand::Status { pipeline_id: None })
        );
        assert_eq!(
            parse_text_command("状态 P-01ABC"),
            Ok(TextCommand::Status {
                pipeline_id: Some(PipelineId::new("P-01ABC"))
            })
        );
        assert!(parse_text_command("hello there").is_err());
    }

    #[test]
    fn parse_feature_with_utterance() {
        assert_eq!(
            parse_text_command("新功能 starfane 加一个导出按钮；计划用 claude，实现用 codex"),
            Ok(TextCommand::Start {
                playbook: PlaybookKind::Feature,
                project: "starfane".into(),
                goal: "加一个导出按钮".into(),
                utterance: Some("计划用 claude，实现用 codex".into()),
            })
        );
        assert_eq!(
            parse_text_command("debug web login returns 500; triage with codex"),
            Ok(TextCommand::Start {
                playbook: PlaybookKind::Debug,
                project: "web".into(),
                goal: "login returns 500".into(),
                utterance: Some("triage with codex".into()),
            })
        );
        assert!(parse_text_command("新功能 starfane").is_err());
        assert!(parse_text_command("feature").is_err());
    }

    #[test]
    fn parse_reply_targets() {
        assert_eq!(
            parse_text_command("回复 %12 继续"),
            Ok(TextCommand::Reply {
                target: Some(ReplyTarget::Pane(PaneId::new("%12"))),
                text: "继续".into()
            })
        );
        assert_eq!(
            parse_text_command("reply s:abc go on"),
            Ok(TextCommand::Reply {
                target: Some(ReplyTarget::Session(SessionId::new("abc"))),
                text: "go on".into()
            })
        );
        assert_eq!(
            parse_text_command("reply P-1/implement fix the test"),
            Ok(TextCommand::Reply {
                target: Some(ReplyTarget::Stage {
                    pipeline_id: PipelineId::new("P-1"),
                    stage: StageKind::Implement
                }),
                text: "fix the test".into()
            })
        );
        assert_eq!(
            parse_text_command("回复 请先跑测试"),
            Ok(TextCommand::Reply {
                target: None,
                text: "请先跑测试".into()
            })
        );
        assert!(parse_text_command("reply P-1/bogus x").is_err());
        assert!(parse_text_command("reply %12").is_err());
    }

    #[test]
    fn parse_confirm_reject_stop() {
        assert_eq!(
            parse_text_command("确认"),
            Ok(TextCommand::Confirm { pipeline_id: None })
        );
        assert_eq!(
            parse_text_command("reject P-9 需要先写测试"),
            Ok(TextCommand::Reject {
                pipeline_id: Some(PipelineId::new("P-9")),
                reason: Some("需要先写测试".into())
            })
        );
        assert_eq!(
            parse_text_command("拒绝 太复杂"),
            Ok(TextCommand::Reject {
                pipeline_id: None,
                reason: Some("太复杂".into())
            })
        );
        assert_eq!(
            parse_text_command("停止 P-3"),
            Ok(TextCommand::Stop {
                pipeline_id: PipelineId::new("P-3")
            })
        );
        assert!(parse_text_command("stop").is_err());
    }

    #[test]
    fn parse_mute_unmute() {
        assert_eq!(
            parse_text_command("静音 30m"),
            Ok(TextCommand::Mute { secs: 1800 })
        );
        assert_eq!(
            parse_text_command("mute 2h"),
            Ok(TextCommand::Mute { secs: 7200 })
        );
        assert_eq!(parse_text_command("取消静音"), Ok(TextCommand::Unmute));
        assert_eq!(parse_text_command("unmute"), Ok(TextCommand::Unmute));
        assert!(parse_text_command("静音").is_err());
        assert!(parse_text_command("mute 0").is_err());
    }

    #[test]
    fn card_buttons_carry_starfane_values() {
        let mut n = notice(NoticeKind::PlanApproval);
        n.review_id = Some(ReviewId::new("R-1"));
        n.actions = vec![
            act("approve_plan", ActionStyle::Primary),
            NoticeAction {
                finding_id: Some("F-1".into()),
                ..act("feedback_fp", ActionStyle::Danger)
            },
        ];
        let card = build_card(&n, &mut counter());
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["header"]["template"], "wathet");
        assert_eq!(card["header"]["title"]["content"], "确认绑定");
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"], "markdown");
        let cols = elements.iter().find(|e| e["tag"] == "column_set").unwrap()["columns"]
            .as_array()
            .unwrap();
        assert_eq!(cols.len(), 2);
        let v0 = &cols[0]["elements"][0]["behaviors"][0]["value"];
        assert_eq!(
            v0,
            &json!({"starfane":1,"op":"approve_plan","pipeline_id":"P-1","review_id":"R-1","nonce":"NONCE001"})
        );
        let b1 = &cols[1]["elements"][0];
        assert_eq!(b1["type"], "danger");
        assert_eq!(b1["behaviors"][0]["value"]["finding_id"], "F-1");
        assert_eq!(b1["behaviors"][0]["value"]["nonce"], "NONCE002");
    }

    #[test]
    fn binding_form_becomes_selects_with_submit() {
        let mut n = notice(NoticeKind::BindingConfirm);
        n.binding_form = vec![
            ("plan".into(), "claude:opus@high".into()),
            ("report".into(), "same_as:plan".into()),
        ];
        n.actions = vec![
            act("confirm_pipeline", ActionStyle::Primary),
            act("cancel_pipeline", ActionStyle::Danger),
        ];
        let card = build_card(&n, &mut counter());
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements.iter().find(|e| e["tag"] == "form").unwrap();
        let selects: Vec<&Value> = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "select_static")
            .collect();
        assert_eq!(selects.len(), 2);
        assert_eq!(selects[0]["name"], "plan");
        assert_eq!(selects[0]["initial_option"], "claude:opus@high");
        let values: Vec<&str> = selects[1]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(values[0], "same_as:plan");
        assert_eq!(values.iter().filter(|v| **v == "same_as:plan").count(), 1);
        let submit = form["elements"].as_array().unwrap().last().unwrap();
        assert_eq!(submit["form_action_type"], "submit");
        assert_eq!(submit["name"], "bc_NONCE001_P-1");
        assert!(submit.get("behaviors").is_none());
        // confirm is the form submit, so only cancel remains as a button.
        let cols = elements.iter().find(|e| e["tag"] == "column_set").unwrap()["columns"]
            .as_array()
            .unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(
            cols[0]["elements"][0]["behaviors"][0]["value"]["op"],
            "cancel_pipeline"
        );
    }

    #[test]
    fn card_body_is_truncated_and_folded_into_collapsible_panel() {
        let mut n = notice(NoticeKind::Progress);
        n.body_md = "x".repeat(MAX_CARD_BODY_BYTES * 2);
        let card = build_card(&n, &mut counter());
        let elements = card["body"]["elements"].as_array().unwrap();
        // Top-level summary stays ultra-compact (≤ 80 char preview).
        let summary = elements[0]["content"].as_str().unwrap();
        assert!(summary.contains("摘要："), "{summary}");
        assert!(summary.chars().count() < 160, "{summary}");
        // Full content is folded into a collapsed `collapsible_panel` and truncated cleanly.
        let panel = elements
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("long notice should include collapsible_panel");
        assert_eq!(panel["expanded"], false);
        let title = panel["header"]["title"]["content"].as_str().unwrap();
        assert!(title.starts_with("展开详细内容（"), "{title}");
        let body = panel["elements"][0]["content"].as_str().unwrap();
        assert!(body.len() < MAX_CARD_BODY_BYTES + 64);
        assert!(body.ends_with("（已截断）"));
    }

    #[test]
    fn extract_flattened_message() {
        let v = json!({
            "type": "im.message.receive_v1", "event_id": "ev1", "message_id": "om_1",
            "chat_id": "oc_team", "chat_type": "group", "message_type": "text",
            "content": "@_user_1 回复 继续\n下一步", "sender_id": "ou_me", "sender_type": "user",
            "root_id": "om_root", "reply_to": "om_parent",
            "mentions": [{"id": "ou_bot", "key": "@_user_1", "name": "starfane"}]
        });
        let m = extract_message(&v).unwrap();
        assert_eq!(m.message_id, "om_1");
        assert_eq!(m.chat_id, "oc_team");
        assert!(!m.p2p);
        assert_eq!(m.sender_open_id, "ou_me");
        assert_eq!(m.text, "回复 继续\n下一步");
        assert_eq!(m.root_id.as_deref(), Some("om_root"));
        assert_eq!(m.parent_id.as_deref(), Some("om_parent"));
        assert_eq!(m.mentioned, Some(true));
        assert_eq!(m.event_id.as_deref(), Some("ev1"));
    }

    #[test]
    fn extract_raw_envelope_message() {
        let v = json!({
            "schema": "2.0", "header": {"event_id": "ev2", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_me"}, "sender_type": "user"},
                "message": {"message_id": "om_2", "chat_id": "oc_dm", "chat_type": "p2p",
                            "message_type": "text", "content": "{\"text\":\"help\"}"}
            }
        });
        let m = extract_message(&v).unwrap();
        assert!(m.p2p);
        assert_eq!(m.text, "help");
        assert_eq!(m.sender_open_id, "ou_me");
        assert_eq!(m.mentioned, None);
        assert!(extract_message(&json!({"foo": 1})).is_none());
    }

    #[test]
    fn extract_card_action_flattened_and_form_submit() {
        let v = json!({
            "type": "card.action.trigger", "event_id": "ev3", "operator_id": "ou_me",
            "chat_id": "oc_main", "message_id": "om_card", "action_tag": "button",
            "action_value": "{\"starfane\":1,\"op\":\"approve_plan\",\"pipeline_id\":\"P-1\",\"nonce\":\"abcd1234\"}"
        });
        let a = extract_card_action(&v).unwrap();
        let val = resolve_card_value(&a).unwrap();
        assert_eq!(val.op, "approve_plan");
        assert_eq!(val.pipeline_id.as_deref(), Some("P-1"));
        assert_eq!(val.nonce, "abcd1234");

        let v = json!({
            "operator_id": "ou_me", "message_id": "om_card", "action_tag": "button",
            "action_name": "bc_NONCE001_P-7",
            "form_value": "{\"plan\":\"claude\",\"implement\":\"codex@high\"}"
        });
        let a = extract_card_action(&v).unwrap();
        let val = resolve_card_value(&a).unwrap();
        assert_eq!(val.op, "confirm_pipeline");
        assert_eq!(val.nonce, "NONCE001");
        assert_eq!(val.pipeline_id.as_deref(), Some("P-7"));
        assert_eq!(a.form_value["implement"], "codex@high");

        // Raw envelope with a non-starfane value is ignored.
        let v = json!({"header": {"event_id": "e"}, "event": {
            "operator": {"open_id": "ou_me"}, "action": {"value": {"foo": "bar"}},
            "context": {"open_message_id": "om_x", "open_chat_id": "oc_main"}}});
        let a = extract_card_action(&v).unwrap();
        assert_eq!(a.chat_id.as_deref(), Some("oc_main"));
        assert!(resolve_card_value(&a).is_none());
    }

    #[test]
    fn card_ops_map_to_commands() {
        let value = |op: &str| ActionValue {
            starfane: 1,
            op: op.into(),
            pipeline_id: Some("P-1".into()),
            review_id: Some("R-1".into()),
            finding_id: Some("F-1".into()),
            session_id: None,
            pane_id: None,
            nonce: "n".into(),
        };
        let pid = Some(PipelineId::new("P-1"));
        let empty = BTreeMap::new();
        let mut form = BTreeMap::new();
        form.insert("implement".to_string(), "codex@high".to_string());
        form.insert("note".to_string(), "ignored".to_string());
        match card_op_command(&value("confirm_pipeline"), pid.clone(), &form, "ou_me").unwrap() {
            DaemonCommand::ConfirmPipeline {
                overrides: Some(o), ..
            } => {
                assert_eq!(o.len(), 1);
                assert_eq!(o[&StageKind::Implement].to_shorthand(), "codex@high");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            card_op_command(&value("confirm_pipeline"), pid.clone(), &empty, "ou_me"),
            Ok(DaemonCommand::ConfirmPipeline {
                overrides: None,
                ..
            })
        ));
        assert!(matches!(
            card_op_command(&value("stop_deep"), pid.clone(), &empty, "ou_me"),
            Ok(DaemonCommand::PipelineAction {
                action: PipelineAction::StopDeep,
                ..
            })
        ));
        match card_op_command(&value("feedback_fp"), pid.clone(), &empty, "ou_me").unwrap() {
            DaemonCommand::ReviewFeedback {
                label,
                by,
                finding_id,
                ..
            } => {
                assert_eq!(label, FeedbackLabel::FalsePositive);
                assert_eq!(by, "ou_me");
                assert_eq!(finding_id.as_str(), "F-1");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(card_op_command(&value("approve_plan"), None, &empty, "ou_me").is_err());
        assert!(card_op_command(&value("nuke"), pid.clone(), &empty, "ou_me").is_err());
        let mut bad = BTreeMap::new();
        bad.insert("plan".to_string(), "".to_string());
        bad.insert("deep".to_string(), "not an agent!!".to_string());
        assert!(card_op_command(&value("confirm_pipeline"), pid, &bad, "ou_me").is_err());
    }

    #[test]
    fn authorization_rules() {
        let cfg = feishu_cfg();
        assert!(authorize(&cfg, "ou_me", Some("oc_team"), false).is_ok());
        assert!(authorize(&cfg, "ou_me", Some("oc_main"), false).is_ok());
        assert!(authorize(&cfg, "ou_me", Some("oc_random"), true).is_ok());
        assert!(authorize(&cfg, "ou_me", None, false).is_ok());
        assert!(authorize(&cfg, "ou_me", Some("oc_random"), false).is_err());
        assert!(authorize(&cfg, "ou_me", Some(""), false).is_err());
        assert!(authorize(&cfg, "ou_evil", Some("oc_team"), false).is_err());
        assert!(authorize(&cfg, "", None, true).is_err());
    }

    #[test]
    fn dedupe_is_bounded() {
        let mut d = Dedupe::new(2);
        assert!(d.insert("a".into()));
        assert!(!d.insert("a".into()));
        assert!(d.insert("b".into()));
        assert!(d.insert("c".into()));
        // "a" was evicted.
        assert!(d.insert("a".into()));
        assert!(!d.insert("c".into()));
    }

    #[test]
    fn message_id_from_lark_output() {
        assert_eq!(
            extract_message_id(r#"{"message_id":"om_1","chat_id":"oc"}"#).as_deref(),
            Some("om_1")
        );
        assert_eq!(
            extract_message_id(r#"{"ok":true,"data":{"message_id":"om_2"}}"#).as_deref(),
            Some("om_2")
        );
        assert_eq!(
            extract_message_id("warning: x\n{\"data\":{\"message_id\":\"om_3\"}}\n").as_deref(),
            Some("om_3")
        );
        assert_eq!(extract_message_id("garbage"), None);
    }

    #[test]
    fn mention_stripping_and_mute_backoff() {
        let ms = vec![Mention {
            key: "@_user_1".into(),
            name: "starfane".into(),
        }];
        assert_eq!(strip_mentions("@_user_1 status", &ms), "status");
        assert_eq!(strip_mentions("@starfane help", &ms), "help");
        assert_eq!(strip_mentions("@someone @other ls", &[]), "ls");
        assert!(is_muted(Some(100), 50));
        assert!(!is_muted(Some(100), 100));
        assert!(!is_muted(None, 0));
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(40)), BACKOFF_MAX);
        assert_eq!(new_nonce().len(), NONCE_LEN);
    }

    fn spec_json(statuses: &[(&str, &str)]) -> PipelineSpec {
        let stages: Vec<Value> = statuses
            .iter()
            .map(|(id, status)| {
                json!({"id": id, "binding": {"agent": "codex"}, "artifact": "plan", "status": status})
            })
            .collect();
        serde_json::from_value(json!({
            "v": 1, "id": "P-1", "playbook": "feature", "goal": "g", "status": "running",
            "project": {"root": "/r", "worktree": "none"},
            "source": {"via": "feishu"},
            "options": {"review_depth": "D1", "max_review_rounds": 2, "triage_timebox_secs": 600, "auto_deep": true},
            "stages": stages, "created_ms": 0, "updated_ms": 0
        }))
        .expect("pipeline spec fixture")
    }

    #[test]
    fn session_notice_card_uses_tmux_label_and_never_renders_question_mark() {
        use starfane_proto::*;
        let sess = SessionRecord {
            session_id: SessionId::new("agy-42"),
            agent: AgentKind::Agy,
            pane: Some(PaneId::new("%42")),
            pane_resolution: PaneResolution::Env,
            cwd: std::env::var("HOME").ok(),
            git_root: None,
            model: None,
            effort: None,
            state: SessionState::Idle,
            state_since_ms: 1000,
            last_event_id: None,
            last_event_ms: 1000,
            last_message: Some("已完成巡检".into()),
            last_error: None,
            parent_session_id: None,
            is_subagent: false,
            subagent_count: 0,
            active_subagents: 0,
            stage: None,
            unread_inbox: 0,
            confidence: Confidence::Exact,
            tmux_target: Some("learning:5.0".into()),
            tmux_label: Some("learning-workflow".into()),
            discovered_only: false,
            transcript_path: None,
            metrics: Some(SessionMetrics {
                context_used_tokens: Some(120_000),
                context_window_tokens: Some(200_000),
                context_used_pct: Some(60.0),
                tps: Some(72.3),
                ttft_ms: None,
                avg_ttft_ms: None,
                last_output_tokens: Some(500),
                last_duration_ms: Some(6915),
                total_input_tokens: None,
                total_output_tokens: None,
                recent_turns: Vec::new(),
                source: "transcript".into(),
                updated_ms: 1000,
            }),
            host_id: None,
            host_display_name: None,
            mux_kind: None,
        };
        let ev = NormalizedEvent {
            v: PROTO_VERSION,
            id: EventId::new("E-1"),
            ts_ms: 1000,
            agent: AgentKind::Agy,
            kind: EventKind::TurnComplete,
            confidence: Confidence::Exact,
            session_id: sess.session_id.clone(),
            subagent_id: None,
            pane: sess.pane.clone(),
            cwd: sess.cwd.clone(),
            model: None,
            native_event: "AfterAgent".into(),
            transcript_path: None,
            bind_token: None,
            detail: EventDetail {
                last_message: Some("已完成巡检".into()),
                ..EventDetail::default()
            },
        };
        let notice = crate::core::router::build_notice(NoticeKind::TurnComplete, &sess, &ev);
        let mut nonce = || "nonce-1".to_string();
        let card = build_card(&notice, &mut nonce);
        let elements = card["body"]["elements"].as_array().unwrap();
        let dumped = card.to_string();
        assert!(dumped.contains("learning-workflow"), "{dumped}");
        assert!(dumped.contains("已完成巡检"), "{dumped}");
        assert!(dumped.contains("reply_form"), "{dumped}");
        assert!(dumped.contains("reply_text"), "{dumped}");
        assert!(dumped.contains("rs_nonce-1_agy-42"), "{dumped}");
        assert!(dumped.contains("引用回复或下方输入发给 Agent"), "{dumped}");
        assert!(!dumped.contains("项目：?"), "{dumped}");
        assert!(!dumped.contains("项目：？"), "{dumped}");
        // Per user preference (#76), Feishu cards stay clean without CTX/TPS noise.
        assert!(!dumped.contains("tok/s"), "{dumped}");

        // Session last output MUST live inside a collapsed `collapsible_panel` (`expanded: false`).
        let panel = elements
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("session card must contain collapsible_panel");
        assert_eq!(panel["expanded"], false);
        assert_eq!(
            panel["header"]["title"]["content"].as_str(),
            Some("展开最后一轮输出（5 字）")
        );
        assert_eq!(panel["elements"][0]["content"].as_str(), Some("已完成巡检"));

        // Long multi-paragraph output stays completely inside the collapsed panel while
        // the top-level markdown stays ultra-compact (≤ 80-char 1-line summary).
        let long_output = format!(
            "## 巡检总结\n\n第一段已完成全部检查并修复 3 处边界问题。\n\n{}",
            "详细日志行内容\n".repeat(40)
        );
        let mut ev_long = ev.clone();
        ev_long.detail.last_message = Some(long_output.clone());
        let notice_long =
            crate::core::router::build_notice(NoticeKind::TurnComplete, &sess, &ev_long);
        let card_long = build_card(&notice_long, &mut nonce);
        let elems_long = card_long["body"]["elements"].as_array().unwrap();
        let top_md = elems_long[0]["content"].as_str().unwrap();
        assert!(top_md.contains("**窗口**：learning-workflow"), "{top_md}");
        assert!(top_md.contains("摘要：巡检总结"), "{top_md}");
        assert!(
            !top_md.contains("详细日志行内容"),
            "long output leaked outside panel: {top_md}"
        );
        let panel_long = elems_long
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("long session card must contain collapsible_panel");
        assert_eq!(panel_long["expanded"], false);
        assert!(panel_long["elements"][0]["content"]
            .as_str()
            .unwrap()
            .contains("详细日志行内容"));
        // Form remains top-level (never nested inside collapsible_panel per Card 2.0 spec).
        assert!(elems_long.iter().any(|e| e["tag"] == "form"));
        assert!(!panel_long.to_string().contains("\"tag\":\"form\""));

        // `build_test_card` exercises the exact same Card 2.0 `collapsible_panel` schema.
        let test_card = build_test_card("自定义测试内容\n第二行详细说明");
        assert_eq!(test_card["schema"], "2.0");
        let test_panel = test_card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("test card must include collapsible_panel");
        assert_eq!(test_panel["expanded"], false);
        assert!(test_panel["elements"][0]["content"]
            .as_str()
            .unwrap()
            .contains("自定义测试内容"));
    }

    #[test]
    fn card_reply_session_form_and_quote_reply_confirmation() {
        let payload = json!({
            "event": {
                "operator": { "open_id": "ou_user" },
                "context": { "open_chat_id": "oc_chat", "open_message_id": "om_card_1" },
                "action": {
                    "name": "rs_abc12345_agy-42",
                    "form_value": { "reply_text": "请继续检查单测" }
                }
            }
        });
        let act = extract_card_action(&payload).expect("extract_card_action");
        let val = resolve_card_value(&act).expect("resolve_card_value");
        assert_eq!(val.op, "reply_session");
        assert_eq!(val.nonce, "abc12345");
        assert_eq!(val.session_id.as_deref(), Some("agy-42"));

        let cmd = card_op_command(&val, None, &act.form_value, "ou_user").expect("card_op_command");
        match cmd {
            DaemonCommand::Dispatch {
                target,
                text,
                interactive,
                ..
            } => {
                assert_eq!(
                    target,
                    DispatchTarget::Session {
                        id: SessionId::new("agy-42")
                    }
                );
                assert_eq!(text, "请继续检查单测");
                assert!(interactive);
            }
            other => panic!("unexpected cmd: {other:?}"),
        }

        let target = crate::notice::SessionReplyTarget {
            session_id: SessionId::new("agy-42"),
            pane_id: Some(PaneId::new("%42")),
            tmux_label: Some("learning-cloud-server".into()),
            agent: starfane_proto::AgentKind::Agy,
        };
        assert_eq!(
            format_forward_confirmation(&target),
            "✅ 已转发给 agy · learning-cloud-server (%42)"
        );
    }

    #[test]
    fn current_stage_and_override_diff() {
        let spec = spec_json(&[
            ("plan", "done"),
            ("implement", "running"),
            ("test_review", "pending"),
        ]);
        assert_eq!(current_stage(&spec), Some(StageKind::Implement));
        let spec2 = spec_json(&[("plan", "done"), ("implement", "pending")]);
        assert_eq!(current_stage(&spec2), Some(StageKind::Implement));
        let mut o = BTreeMap::new();
        o.insert(
            StageKind::Plan,
            StageBinding::from_shorthand("codex").unwrap(),
        );
        o.insert(
            StageKind::Implement,
            StageBinding::from_shorthand("claude").unwrap(),
        );
        retain_changed(&mut o, &spec);
        assert_eq!(o.len(), 1);
        assert!(o.contains_key(&StageKind::Implement));
        let text = format_pipeline(&spec);
        assert!(text.contains("implement · running · `codex`"), "{text}");
    }

    /// Fake `lark-cli event consume`: prints the ready marker, reports whether
    /// stdin stays open (`read` blocks until its timeout), then waits for SIGTERM.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consumer_keeps_stdin_open_parses_ndjson_and_sigterms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("starfane-lark-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let term_file = dir.join("term");
        let script = dir.join("fake-lark-cli");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\ntrap 'echo term > {term}; exit 0' TERM\n\
                 echo \"[event] ready event_key=$3\" >&2\n\
                 t0=$(date +%s); read -t 2 x; rc=$(( $(date +%s) - t0 ))\n\
                 echo 'not json'\n\
                 echo \"{{\\\"rc\\\":$rc,\\\"args\\\":\\\"$*\\\"}}\"\n\
                 while :; do sleep 0.1; done\n",
                term = term_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let bin = script.to_string_lossy().into_owned();
        let task = tokio::spawn(async move { consume_once(&bin, CARD_EVENT, &tx).await });
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("event in time")
            .expect("channel open");
        assert_eq!(ev.key, CARD_EVENT);
        // rc = seconds `read` blocked: 0 means stdin hit EOF (closed).
        assert!(ev.payload["rc"].as_i64().unwrap() >= 1, "{}", ev.payload);
        assert_eq!(
            ev.payload["args"],
            "event consume card.action.trigger --as bot"
        );
        task.abort();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !term_file.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(term_file.exists(), "child did not receive SIGTERM");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_custom_mute_durations_and_wall_clock_hhmm() {
        assert_eq!(
            parse_text_command("静音 45m").unwrap(),
            TextCommand::Mute { secs: 45 * 60 }
        );
        assert_eq!(
            parse_text_command("静音 3h").unwrap(),
            TextCommand::Mute { secs: 3 * 3600 }
        );
        assert_eq!(
            parse_text_command("静音 2d").unwrap(),
            TextCommand::Mute { secs: 2 * 86_400 }
        );
        assert!(parse_text_command("静音 8d").is_err());
        assert_eq!(
            parse_text_command("静音 23:30").unwrap(),
            TextCommand::MuteUntil {
                hour: 23,
                minute: 30
            }
        );
        assert_eq!(
            parse_text_command("mute 9：05").unwrap(),
            TextCommand::MuteUntil { hour: 9, minute: 5 }
        );
        assert!(parse_text_command("静音 24:00").is_err());
        assert!(parse_text_command("静音 12:60").is_err());

        // Timezone offset parsing
        assert_eq!(parse_tz_offset("+0800"), Some(8 * 3600));
        assert_eq!(parse_tz_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_tz_offset("bad"), None);

        // Wall-clock resolution in UTC+8:
        // 2026-01-01 12:00:00 UTC == 2026-01-01 20:00:00 +08:00
        let now_ms = 1_767_268_800_000u64;
        let tz = 8 * 3600;
        // 23:30 local is 3h30m in the future (same day).
        let same_day = resolve_hhmm_until_ms(23, 30, now_ms, tz);
        assert_eq!(same_day - now_ms, (3 * 3600 + 30 * 60) * 1000);
        // 08:30 local has already passed today (20:00), so it rolls to tomorrow (+12h30m).
        let next_day = resolve_hhmm_until_ms(8, 30, now_ms, tz);
        assert_eq!(next_day - now_ms, (12 * 3600 + 30 * 60) * 1000);
    }

    #[tokio::test]
    async fn same_session_notices_group_into_continuous_feishu_thread_and_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "starfane-lark-thread-{}",
            starfane_proto::id::new_id("T")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("calls.log");
        let count_file = dir.join("count");
        let fail_reply_flag = dir.join("fail_reply");
        let script = dir.join("fake-lark-cli");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/bash
set -e
LOG="{log}"
CNT="{cnt}"
FAIL="{fail}"
n=0
if [ -f "$CNT" ]; then n=$(cat "$CNT"); fi
n=$((n + 1))
echo "$n" > "$CNT"
echo "$*" >> "$LOG"
if [ "$2" = "+messages-reply" ] && [ -f "$FAIL" ]; then
  echo "root card deleted" >&2
  exit 1
fi
echo "{{\"data\":{{\"message_id\":\"om_msg_${{n}}\"}}}}"
"#,
                log = log_file.display(),
                cnt = count_file.display(),
                fail = fail_reply_flag.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = crate::config::StarfaneConfig {
            feishu: FeishuConfig {
                enabled: true,
                lark_cli: script.to_string_lossy().into_owned(),
                chat_id: "oc_main".into(),
                allow_open_ids: vec!["ou_me".into()],
                allow_chat_ids: vec![],
            },
            ..Default::default()
        };
        let core = Core::new(&crate::DaemonConfig {
            socket: dir.join("starfane.sock"),
            state_dir: dir.join("state"),
            web_dir: None,
            config: cfg.clone(),
            foreground: true,
        })
        .unwrap();
        let lark = Arc::new(LarkCli::new(&cfg.feishu));
        let mut adapter = Adapter {
            core: core.clone(),
            lark,
            cfg: cfg.feishu.clone(),
            seen: Dedupe::new(64),
        };

        let target = crate::notice::SessionReplyTarget {
            session_id: SessionId::new("sess-thread-1"),
            pane_id: Some(PaneId::new("%11")),
            tmux_label: Some("main-dev".into()),
            agent: starfane_proto::AgentKind::Claude,
        };
        let mk_notice = |key: &str| Notice {
            key: key.into(),
            kind: NoticeKind::TurnComplete,
            title: "完成".into(),
            body_md: "ok".into(),
            pipeline_id: None,
            session_id: Some(SessionId::new("sess-thread-1")),
            review_id: None,
            actions: vec![],
            binding_form: vec![],
            reply_session: Some(target.clone()),
        };

        // 1st notice: no existing thread → sends top-level card (om_msg_1) and records thread root.
        adapter.on_notice(mk_notice("n1")).await;
        assert_eq!(
            core.feishu_session_thread(&SessionId::new("sess-thread-1"))
                .as_deref(),
            Some("om_msg_1")
        );
        assert_eq!(
            core.session_for_feishu_message("om_msg_1")
                .map(|t| t.session_id),
            Some(SessionId::new("sess-thread-1"))
        );

        // 2nd notice: existing root om_msg_1 → replies in thread (+messages-reply --reply-in-thread).
        adapter.on_notice(mk_notice("n2")).await;
        assert_eq!(
            core.feishu_session_thread(&SessionId::new("sess-thread-1"))
                .as_deref(),
            Some("om_msg_1")
        );
        assert_eq!(
            core.session_for_feishu_message("om_msg_2")
                .map(|t| t.session_id),
            Some(SessionId::new("sess-thread-1"))
        );

        // 3rd notice: simulate deleted root card → reply fails, falls back to top-level send_card
        // and refreshes thread root to the newly created top-level message id.
        std::fs::write(&fail_reply_flag, "1").unwrap();
        adapter.on_notice(mk_notice("n3")).await;
        assert_eq!(
            core.feishu_session_thread(&SessionId::new("sess-thread-1"))
                .as_deref(),
            Some("om_msg_4")
        );

        let calls = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 4, "calls:\n{calls}");
        assert!(lines[0].contains("+messages-send"), "{}", lines[0]);
        assert!(
            lines[1].contains("+messages-reply")
                && lines[1].contains("--message-id om_msg_1")
                && lines[1].contains("--reply-in-thread"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("+messages-reply"), "{}", lines[2]);
        assert!(lines[3].contains("+messages-send"), "{}", lines[3]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
