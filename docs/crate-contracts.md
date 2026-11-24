# starfane Crate 接口契约（M0 冻结版 · 更名同步）

> 状态：**v0.1 FROZEN**。本文是 Wave 2 并行编码的唯一接口来源。改动 `starfane-proto` 或任何跨 crate 签名，必须先在 board 上 @architect 提出变更，**不得各自修改**。
> 上游设计：[architecture.md](./architecture.md) v2.1。原项目名 `baton`，现已更名为 `starfane`（守护进程 `starfaned`）。

---

## 0. 全局约定

| 项 | 约定 |
|---|---|
| 命名 | 二进制 `starfane`；守护进程 = `starfane daemon`（文中称 starfaned）；crate 名 `starfane-*`，Rust 中写作 `starfane_*` |
| 状态目录 | `$STARFANE_STATE_DIR`，默认 `~/.local/state/starfane/` |
| Socket | `$STARFANE_SOCKET`，默认 `<state>/starfaned.sock` |
| 用户配置 | `$STARFANE_CONFIG`，默认 `~/.config/starfane/config.toml` |
| 项目目录 | `<git_root>/.starfane/`（`team.toml`、`pipelines/`、`artifacts/`、`handoffs/`、`reviews/`、`feedback.jsonl`） |
| 环境变量 | `STARFANE_STATE_DIR`、`STARFANE_SOCKET`、`STARFANE_CONFIG`、`STARFANE_BIND`（L4 spawn 的一次性绑定 token）、`STARFANE_LOG`（`error`/`warn`/`info`/`debug`）、`STARFANE_DISABLE=1`（emit 直接 no-op） |
| 时间 | `ts_ms: u64`，表示 Unix 毫秒（UTC）；不引入 chrono |
| ID | `starfane_proto::id::new_id(prefix)`，生成 `<prefix>-<26 位 Crockford base32>`：48 位毫秒时间 + 80 位随机（随机数来自 `/dev/urandom`）。前缀：`E` 事件、`P` pipeline、`H` handoff、`A` artifact、`R` review、`F` finding、`B` bind token |
| serde | 枚举统一 `#[serde(rename_all = "snake_case")]`（`ReviewDepth` 除外，见 §1.6）；`Option` 字段加 `#[serde(default, skip_serializing_if = "Option::is_none")]`；`Vec` 字段加 `#[serde(default)]`；**不使用 `deny_unknown_fields`**，向前兼容，必填检查交给 G1 |
| 版本 | 每个顶层线缆 / 工件结构都带 `v: u32`，当前全部为 `1`（`starfane_proto::PROTO_VERSION`） |
| 错误 | 库 crate 各自定义 `pub enum Error`，手写 `Display` 和 `std::error::Error`；**不用 anyhow / thiserror** |
| 依赖分层 | 见 §0.1；`starfane-emit` **禁止**依赖 tokio、axum 和 `starfane-daemon` |

### 0.1 依赖图（只允许箭头方向）

```
starfane-proto  ← serde, serde_json
   ↑
   ├── starfane-emit       （proto, serde_json）                 std-only 热路径
   ├── starfane-adapters   （proto, serde_json, toml）
   ├── starfane-tmux       （proto）                               std::process 同步实现
   ├── starfane-evidence   （proto）                               调用 git 子进程
   ├── starfane-binding    （proto）                               纯函数
   └── starfane-daemon     （上述全部 + tokio + axum + toml + log）
          ↑
       bin/starfane        （全部 + clap）
tools/fake-agent        （proto, serde_json）
```

---

## 1. `starfane-proto`

模块划分：`id`、`paths`、`agent`、`event`、`session`、`pipeline`、`artifact`（含 `handoff`、`plan`、`review`、`triage`、`rca`、`report`）、`wire`、`api`、`sse`。

### 1.1 基础类型

```rust
pub const PROTO_VERSION: u32 = 1;

macro_rules! id_newtype { ($($n:ident),*) => { $(
  #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
  #[serde(transparent)] pub struct $n(pub String);
)* } }
id_newtype!(EventId, SessionId, PaneId, PipelineId, HandoffId, ArtifactId, ReviewId, FindingId, BindToken);

pub mod id { pub fn new_id(prefix: &str) -> String; }
pub mod paths {
  pub fn state_dir() -> PathBuf;  pub fn socket_path() -> PathBuf;  pub fn spool_dir() -> PathBuf;
  pub fn events_dir() -> PathBuf; pub fn snapshot_path() -> PathBuf; pub fn backups_dir() -> PathBuf;
  pub fn config_path() -> PathBuf; pub fn project_dir(git_root: &Path) -> PathBuf; // <root>/.starfane
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind { Claude, Codex, Agy }
impl AgentKind {
  pub fn as_str(self) -> &'static str;               // "claude" | "codex" | "agy"
  pub fn parse(s: &str) -> Option<Self>;             // 大小写不敏感
  pub const ALL: [AgentKind; 3];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort { Low, Medium, High, Xhigh, Max }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMode { Swarm, Boost, Teamwork, Subagents }
impl NativeMode {
  pub fn owner(self) -> Option<AgentKind>;  // Swarm / Boost / Teamwork 归 Agy；Subagents 返回 None（三家通用）
  pub fn slash(self) -> Option<&'static str>; // "/swarm" | "/boost" | "/teamwork" | None
}
```

### 1.2 事件

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
  SessionStart, Working, TurnComplete, NeedsPermission, NeedsInput, Error, Interrupted, SessionEnd,
  SubagentStart, SubagentEnd,
  ToolStart, ToolEnd,      // 仅用于 agy 审批启发式，不推送通知
  Ignored,                 // 已识别但无语义（如 claude Notification:auth_success）
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence { Exact, Heuristic }

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvCtx {
  pub tmux_pane: Option<String>,   // $TMUX_PANE
  pub tmux: Option<String>,        // $TMUX
  pub pid: u32,                    // emit 进程自身的 pid
  pub ppid: u32,                   // 父进程（hook shell）
  pub cwd: String,                 // emit 进程的 cwd（agy 下是 hooks.json 所在目录，不可信）
  pub bind_token: Option<BindToken>, // $STARFANE_BIND
}
impl EnvCtx { pub fn capture() -> Self; }

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventDetail {
  pub tool_name: Option<String>,
  pub tool_summary: Option<String>,        // tool_input 的单行摘要，最多 200 字符
  pub last_message: Option<String>,        // last_assistant_message，最多 500 字符
  pub notification_type: Option<String>,   // claude
  pub termination_reason: Option<String>,  // agy
  pub fully_idle: Option<bool>,            // agy
  pub continuation_active: Option<bool>,   // agy
  pub error: Option<String>,
  pub agent_type: Option<String>,          // claude / codex 子会话
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
  pub v: u32,
  pub id: EventId,
  pub ts_ms: u64,
  pub agent: AgentKind,
  pub kind: EventKind,
  pub confidence: Confidence,
  pub session_id: SessionId,            // 根会话：claude/codex 用 session_id，agy 用 conversationId
  pub subagent_id: Option<String>,      // claude/codex 的 agent_id；agy 恒为 None（折叠由 daemon 推断）
  pub pane: Option<PaneId>,             // 来自 env.tmux_pane（daemon 可能会修正）
  pub cwd: Option<String>,              // 来自 payload 的 cwd（不取 env.cwd）
  pub model: Option<String>,            // claude SessionStart.model / codex model / agy modelName
  pub native_event: String,             // 原始 hook 事件名，例如 "Stop"
  pub transcript_path: Option<String>,
  pub bind_token: Option<BindToken>,
  pub detail: EventDetail,
}
```

### 1.3 会话

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState { Unknown, Starting, Working, Idle, NeedsPermission, NeedsInput, Error, Ended }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResolution { BindToken, Env, PidTree, CwdMatch, Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRef { pub pipeline_id: PipelineId, pub stage: StageKind }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
  pub session_id: SessionId,             // 子会话为 "<root>/<agent_id>"（claude/codex），agy 折叠子会话用其自身的 conversationId
  pub agent: AgentKind,
  pub pane: Option<PaneId>,
  pub pane_resolution: PaneResolution,
  pub cwd: Option<String>,
  pub git_root: Option<String>,
  pub model: Option<String>,
  pub state: SessionState,
  pub state_since_ms: u64,
  pub last_event_id: Option<EventId>,
  pub last_event_ms: u64,
  pub last_message: Option<String>,
  pub parent_session_id: Option<SessionId>,
  pub is_subagent: bool,
  pub subagent_count: u32,               // 累计数
  pub active_subagents: u32,             // 当前未结束的数量
  pub stage: Option<StageRef>,           // 被 pipeline 认领或拉起时设置
  pub unread_inbox: u32,
  pub confidence: Confidence,            // agy 启发式折叠出来的子会话为 Heuristic
}
```

**状态转移**（daemon 实现，`EventKind` → `SessionState`）：SessionStart→Starting；Working/ToolStart/ToolEnd/SubagentStart→Working；TurnComplete→Idle；NeedsPermission→NeedsPermission；NeedsInput→NeedsInput；Error→Error；Interrupted→Idle；SessionEnd→Ended；Ignored→不变。子会话事件只更新子记录和父记录的计数，**不改变父会话的 state**。唯一例外：子会话的 NeedsPermission 会把父会话置为 NeedsPermission。

### 1.4 Pipeline（`pipeline.v1`，存储为 TOML）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybookKind { Feature, Debug }
impl PlaybookKind { pub fn stages(self) -> &'static [StageKind]; }
// Feature => [Plan, Implement, TestReview, Report]；Debug => [Triage, Deep]

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind { Plan, Implement, TestReview, Report, Triage, Deep }

/// 序列化为字符串："auto" | "spawn" | "%12"（pane）| "session:<id>"
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SessionTarget { #[default] Auto, Spawn, Pane(PaneId), Session(SessionId) }

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageBinding {
  pub agent: Option<AgentKind>,          // 设置了 same_as 时为 None
  pub model: Option<String>,
  pub effort: Option<Effort>,
  pub native: Option<NativeMode>,
  #[serde(default)] pub session: SessionTarget,
  pub depth: Option<ReviewDepth>,        // 只对 TestReview 有意义
  pub same_as: Option<StageKind>,
}
impl StageBinding {
  /// 简写语法：<agent>[:<model>][@<effort>][ /<native>]，或 "same_as:<stage>"
  pub fn from_shorthand(s: &str) -> Result<Self, ProtoError>;
  pub fn to_shorthand(&self) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind { HumanConfirm, ReviewGates, EvidenceG2, SchemaG1 }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind { Plan, Handoff, Review, Triage, Rca, Report }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus { Pending, Running, AwaitingHuman, Blocked, Done, Skipped, Failed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus { AwaitingBindingConfirm, Running, AwaitingHuman, Blocked, Done, Cancelled, Failed }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopSpec { pub back_to: StageKind, pub max_rounds: u32 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageSpec {
  pub id: StageKind,
  pub binding: StageBinding,
  pub gate: Option<GateKind>,
  pub artifact: ArtifactKind,
  #[serde(rename = "loop")] pub loop_spec: Option<LoopSpec>,
  pub status: StageStatus,
  pub session_id: Option<SessionId>,
  pub pane: Option<PaneId>,
  #[serde(default)] pub round: u32,
  #[serde(default)] pub artifacts: Vec<ArtifactId>,
  pub started_ms: Option<u64>,
  pub ended_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via { Web, Feishu, Cli, Mcp }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineSource { pub via: Via, pub utterance: Option<String>, pub operator: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRef { pub root: String, pub base_branch: Option<String>, pub worktree: WorktreeMode }

/// 序列化："auto" | "none" | 其他字符串一律视为路径
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum WorktreeMode { Auto, None, Path(String) }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineOptions {
  pub review_depth: ReviewDepth,         // 默认 D1
  pub max_review_rounds: u32,            // 默认 2
  pub triage_timebox_secs: u64,          // 默认 600
  pub auto_deep: bool,                   // 默认 true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeishuThreadRef { pub chat_id: String, pub root_message_id: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineSpec {
  pub v: u32,
  pub id: PipelineId,
  pub playbook: PlaybookKind,
  pub goal: String,
  pub status: PipelineStatus,
  pub project: ProjectRef,
  pub source: PipelineSource,
  pub options: PipelineOptions,
  pub stages: Vec<StageSpec>,            // 顺序与 playbook.stages() 一致
  pub feishu_thread: Option<FeishuThreadRef>,
  pub created_ms: u64,
  pub updated_ms: u64,
}
```

**阶段完成规则**（什么事件推进什么阶段）：

| 阶段 | 完成信号 | 门禁 | 下一步 |
|---|---|---|---|
| Plan | MCP `report_milestone{kind:"plan"}` | G1 → HumanConfirm（`PipelineAction::ApprovePlan`） | Implement |
| Implement | MCP `create_handoff{intent:"review"}`，由该阶段的会话发出 | G1（`self_test` 必填） | TestReview |
| TestReview | MCP `submit_review`，加上所有 G3 裁决完成 | G1 → G2 → G3 | 存在 confirmed 或 needs_human 级别的 blocker/major，且 `round < max_review_rounds` 时回到 Implement（round+1）；超出轮次则 AwaitingHuman；否则进入 Report |
| Report | MCP `report_milestone{kind:"final_report"}` | G1 | Done |
| Triage | MCP `submit_triage` | G1 → G2 | 若 `auto_deep` 则进入 Deep，否则 AwaitingHuman（等待 `ContinueDeep`） |
| Deep | MCP `report_milestone{kind:"rca"}` | G1 → G2 | 可选：修复走一轮 TestReview（D1），然后 Done |

### 1.5 工件公共部分

工件的**规范存储格式为 JSON**：`<root>/.starfane/artifacts/<P-id>/<A-id>.json`。另由 daemon 渲染出 `<A-id>.md` 供人阅读。这是对 architecture §4 「TOML 前置元数据」的简化：MCP 提交本来就是 JSON，统一用 JSON 可以省掉嵌套 TOML 的复杂度。

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorRef { pub agent: AgentKind, pub session_id: SessionId, pub model: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactHeader {
  pub v: u32,
  pub id: ArtifactId,
  pub kind: ArtifactKind,
  pub pipeline_id: Option<PipelineId>,
  pub stage: Option<StageKind>,
  pub author: AuthorRef,
  pub created_ms: u64,
  pub commit: Option<String>,            // 产出时的 HEAD
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeRef { pub file: String, pub line: u32, pub note: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceQuote { pub file: String, pub line_start: u32, pub line_end: u32, pub quote: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceRef {
  Code(EvidenceQuote),
  Log { path: String, line: Option<u32>, quote: String },  // path 相对仓库根目录，或使用绝对路径
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome { Pass, Fail, NotRun }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRun { pub cmd: String, pub outcome: TestOutcome, pub output_tail: Option<String> } // tail 最多 2000 字符
```

### 1.6 各工件

```rust
// ---- handoff.v1 ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum HandoffIntent { Review, Implement, Continue, Ask, Fyi }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum AccessMode { ReadOnly, Write }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum HandoffStatus { Pending, Delivered, Acked, Done }
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HandoffTarget { pub session_id: Option<SessionId>, pub pane: Option<PaneId>, pub agent: Option<AgentKind>, pub stage: Option<StageRef> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoState { pub root: String, pub branch: Option<String>, pub base: Option<String>, pub head: Option<String>, #[serde(default)] pub dirty: Vec<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffPacket {
  pub v: u32, pub id: HandoffId, pub pipeline_id: Option<PipelineId>,
  pub from: AuthorRef, pub to: HandoffTarget,
  pub intent: HandoffIntent, pub mode: AccessMode,
  pub repo: Option<RepoState>,           // 由 daemon 用 git 自动填充
  pub summary: String,                   // G1：非空，最多 500 字符
  pub body_md: String,
  pub self_test: Option<TestRun>,        // intent=Review 时 G1 要求必填
  #[serde(default)] pub todo: Vec<String>,
  #[serde(default)] pub refs: Vec<String>,
  pub status: HandoffStatus,
  pub created_ms: u64,
}

// ---- plan.v1 ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTask { pub id: String, pub title: String, #[serde(default)] pub files: Vec<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanDoc {
  pub header: ArtifactHeader, pub goal: String, pub design_md: String,
  pub tasks: Vec<PlanTask>,              // G1：≥1
  pub acceptance: Vec<String>,           // G1：≥1
  pub test_plan: Vec<String>,            // G1：≥1
  #[serde(default)] pub risks: Vec<String>,
  pub native_strategy: Option<NativeMode>,
}

// ---- review.v1 ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReviewDepth { D0, D1, D2, D3 }   // 序列化为 "D0".."D3"
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Severity { Blocker, Major, Minor, Nit }   // 排序：Blocker 最高
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Category { Correctness, Concurrency, Security, Perf, Api, Style }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum VerificationMethod { CodeTrace, StaticCheck, ReproTest }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum ReproKind { Test, Input, None }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repro { pub kind: ReproKind, pub content: Option<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocationRange { pub file: String, pub line_start: u32, pub line_end: u32, pub symbol: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
  pub id: FindingId,                     // reviewer 可以留空字符串，由 daemon 分配
  pub category: Category, pub severity: Severity, pub confidence: f32,   // 取值 0.0..=1.0
  pub claim: String,
  pub location: LocationRange,
  pub evidence: Vec<EvidenceQuote>,      // G1：≥1
  #[serde(default)] pub trigger_path: Vec<CodeRef>,
  pub preconditions: Option<String>,
  #[serde(default)] pub checked_guards: Vec<CodeRef>,
  pub observable_impact: Option<String>,
  pub verification_method: VerificationMethod,
  pub repro: Option<Repro>,
  pub suggested_fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
  pub header: ArtifactHeader,
  pub depth: ReviewDepth,
  pub base: String,                      // 被审查范围的 base commit
  pub head: String,                      // 被审查的 commit（G2 以此为准）
  #[serde(default)] pub scope_files: Vec<String>,
  #[serde(default)] pub checked_areas: Vec<String>,   // findings 为空时 G1 要求 ≥1
  #[serde(default)] pub findings: Vec<ReviewFinding>,
  pub summary: Option<String>,
}

// ---- 门禁结果（daemon / evidence 填写）----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct G1Issue { pub path: String, pub reason: String }   // path 形如 "findings[2].trigger_path"
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct G1Result { pub report: Vec<G1Issue>, pub findings: BTreeMap<FindingId, Vec<G1Issue>> }
impl G1Result { pub fn ok(&self) -> bool; }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum EvidenceIssue {
  FileMissing { file: String },
  LineOutOfRange { file: String, line: u32, line_count: u32 },
  QuoteNotFound { file: String, line_start: u32 },
  QuoteRelocated { file: String, claimed: u32, actual: u32 },   // 超出 ±tol，但在文件中唯一命中
  QuoteAmbiguous { file: String, occurrences: u32 },            // 超出 ±tol，且在文件中多处命中
  QuoteTooShort { file: String },
  SymbolNotFound { symbol: String },
  LogMissing { path: String },
  LogQuoteNotFound { path: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum EvidenceVerdict { Verified, Relocated, Fabricated }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct G2Outcome { pub verdict: EvidenceVerdict, #[serde(default)] pub issues: Vec<EvidenceIssue> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FalsifierVerdict { Confirmed, NeedsHuman, Downgraded { to: Severity }, Disproved }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FalsifierSubmission {
  pub review_id: ReviewId, pub finding_id: FindingId,
  #[serde(flatten)] pub verdict: FalsifierVerdict,
  pub rationale: String,
  #[serde(default)] pub counter_evidence: Vec<EvidenceQuote>,   // Disproved 时 G1 要求 ≥1，且必须通过 G2，否则降级为 NeedsHuman
  pub falsifier: AuthorRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Disposition { ReturnedG1, Fabricated, PendingFalsifier, Delivered, Folded, Dropped }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum FeedbackLabel { Valid, FalsePositive, Wontfix }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackRecord { pub label: FeedbackLabel, pub note: Option<String>, pub by: String, pub ts_ms: u64 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdjudicatedFinding {
  pub finding: ReviewFinding,
  pub g1: Vec<G1Issue>,
  pub g2: Option<G2Outcome>,
  pub falsifier: Option<FalsifierSubmission>,
  pub disposition: Disposition,
  pub feedback: Option<FeedbackRecord>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRecord {               // 存储在 <root>/.starfane/reviews/<R-id>.json
  pub v: u32, pub id: ReviewId, pub report: ReviewReport,
  pub report_g1: Vec<G1Issue>,
  pub findings: Vec<AdjudicatedFinding>,
  pub returned_once: bool,              // G1 退回最多 1 次
  pub complete: bool,                   // 所有 PendingFalsifier 都已裁决
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrecisionStat {
  pub agent: AgentKind, pub model: Option<String>, pub depth: ReviewDepth, pub category: Category,
  pub delivered: u32, pub valid: u32, pub false_positive: u32, pub wontfix: u32,
  pub fabricated: u32, pub disproved: u32,
  pub precision: Option<f32>,           // valid / (valid + false_positive)；分母为 0 时为 None
}

// ---- triage.v1 ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum HypothesisKind { Hypothesis }  // 固定值：Phase A 的结论永远不是「已确认根因」
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
  pub kind: HypothesisKind, pub claim: String, pub confidence: f32,
  pub evidence_refs: Vec<EvidenceRef>,   // G1：≥1
  pub how_to_confirm: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageReport {
  pub header: ArtifactHeader, pub symptom: String,
  pub hypotheses: Vec<Hypothesis>,       // G1：1..=2
  pub mitigation: Option<String>,
  #[serde(default)] pub probes: Vec<String>,
  pub elapsed_secs: Option<u64>,
}

// ---- rca.v1 ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootCause { pub claim: String, pub trigger_path: Vec<CodeRef>, pub evidence: Vec<EvidenceRef> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixRef { pub branch: String, pub commit: Option<String>, pub files: Vec<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuledOut { pub hypothesis: String, pub evidence: Vec<EvidenceRef> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RcaReport {
  pub header: ArtifactHeader, pub triage_id: Option<ArtifactId>,
  pub root_cause: RootCause,
  pub repro_before: TestRun,             // G1：outcome 必须为 Fail，除非 repro_note 说明了无法复现的原因
  pub repro_note: Option<String>,
  pub fix: Option<FixRef>,
  pub verification_after: Option<TestRun>, // fix 存在时 G1 要求必填，且 outcome 必须为 Pass
  #[serde(default)] pub regression: Vec<TestRun>,
  #[serde(default)] pub ruled_out: Vec<RuledOut>,
}

// ---- report.v1 ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalReport {
  pub header: ArtifactHeader, pub summary_md: String,
  #[serde(default)] pub changes: Vec<String>, #[serde(default)] pub decisions: Vec<String>,
  #[serde(default)] pub follow_ups: Vec<String>,
  pub doc_path: Option<String>,          // daemon 写入的位置，默认 docs/changes/<P-id>.md
}
```

### 1.7 UDS 线缆协议（`wire`）

传输方式：Unix stream socket，**每行一个 JSON**（`\n` 结尾）。

- **emit**：建立连接 → 写入一行 `ClientFrame::Emit` → 关闭连接，不读取回复。
- **请求**：建立连接 → 写入一行或多行 `ClientFrame::Req` → 每个请求读到一行对应的 `DaemonResponse`。同一连接可复用（MCP 进程会保持长连接）。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitRequest {
  pub v: u32,
  pub agent: AgentKind,
  pub hook_event: Option<String>,        // 来自 --event 参数；缺省时由 daemon 从 payload 中的 hook_event_name / hookEventName 读取
  pub payload: serde_json::Value,        // 原始 stdin；解析失败时为 {"_raw": "<字符串>"}
  pub env: EnvCtx,
  pub recv_ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
  Emit(EmitRequest),
  Req { id: u64, cmd: DaemonCommand },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
  pub id: u64,
  pub ok: bool,
  pub data: Option<serde_json::Value>,   // ok=true 时为该命令的返回结构（见 §1.8 的返回值注释）
  pub error: Option<ApiError>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum ErrorCode { BadRequest, NotFound, Conflict, GateRejected, Unavailable, Internal }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError { pub code: ErrorCode, pub message: String }
```

**Spool**：emit 连接失败时，以追加方式写入 `<state>/spool/<pid>-<ts_ms>.jsonl`，每行一个 `EmitRequest`。daemon 启动时以及之后每 5 秒扫描一次，处理完后删除文件。

### 1.8 `DaemonCommand`（UDS、REST、飞书、CLI 共用同一套语义）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DaemonCommand {
  Ping,                                                               // -> {"pong":true,"version":String}
  GetState,                                                           // -> StateSnapshot
  ListSessions { project: Option<String>, #[serde(default)] include_subagents: bool },  // -> Vec<SessionRecord>
  ResolveIdentity { agent: AgentKind, cwd: Option<String>, pane: Option<PaneId>,
                    bind_token: Option<BindToken>, session_hint: Option<SessionId> },   // -> IdentityResolution
  ReadInbox { session_id: SessionId, #[serde(default = "default_true")] mark_delivered: bool },  // -> Vec<InboxItem>（proto 中提供 fn default_true() -> bool { true }）
  CreateHandoff { draft: HandoffDraft },                              // -> {handoff_id, delivery: DeliveryReport}
  AckHandoff { session_id: SessionId, handoff_id: HandoffId, status: HandoffStatus, note: Option<String> }, // -> {}
  SubmitArtifact { session_id: SessionId, artifact: ArtifactPayload }, // -> SubmitOutcome
  SubmitFalsifierVerdict { session_id: SessionId, submission: FalsifierSubmission },   // -> {accepted: bool, g2: Option<G2Outcome>, effective: FalsifierVerdict}
  ReportProgress { session_id: SessionId, text: String, #[serde(default)] notify_human: bool }, // -> {}
  ParseUtterance { text: String, playbook: Option<PlaybookKind>, project: Option<String> },  // -> ParsedBinding（已补全默认值）
  CreatePipeline { req: CreatePipelineReq },                          // -> PipelineSpec（状态为 AwaitingBindingConfirm）
  ConfirmPipeline { pipeline_id: PipelineId,
                    overrides: Option<BTreeMap<StageKind, StageBinding>> }, // -> PipelineSpec（状态为 Running）
  PipelineAction { pipeline_id: PipelineId, action: PipelineAction }, // -> PipelineSpec
  ListPipelines { project: Option<String>, #[serde(default)] include_done: bool },      // -> Vec<PipelineSpec>
  GetPipeline { pipeline_id: PipelineId },                            // -> PipelineSpec
  GetArtifact { id: ArtifactId },                                     // -> ArtifactPayload
  ListHandoffs { session_id: Option<SessionId>, pipeline_id: Option<PipelineId> },     // -> Vec<HandoffPacket>
  GetReview { review_id: ReviewId },                                  // -> ReviewRecord
  ListReviews { pipeline_id: Option<PipelineId> },                    // -> Vec<ReviewRecord>
  ReviewFeedback { review_id: ReviewId, finding_id: FindingId, label: FeedbackLabel, note: Option<String>, by: String }, // -> {}
  HumanVerdict { review_id: ReviewId, finding_id: FindingId, verdict: FalsifierVerdict, note: Option<String>, by: String }, // -> AdjudicatedFinding
  GetPrecision { agent: Option<AgentKind>, model: Option<String> },   // -> Vec<PrecisionStat>
  Dispatch { target: DispatchTarget, text: String,
             #[serde(default)] allow: Vec<DeliveryLevel> },           // -> DeliveryReport（allow 为空 = [L0, L1, L2]）
  FeishuTest { text: String },                                        // -> {message_id: String}
  Shutdown,                                                           // -> {}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPayload { Plan(PlanDoc), Review(ReviewReport), Triage(TriageReport), Rca(RcaReport), Report(FinalReport) }
// 提交时 header.id / created_ms / author 可以留空，由 daemon 覆写

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitOutcome {
  pub artifact_id: ArtifactId,
  pub accepted: bool,                    // G1 通过（review 则是报告级 G1 通过）
  pub g1: G1Result,
  pub g2: BTreeMap<String, G2Outcome>,   // review 以 finding_id 为键；triage/rca 以 "hypotheses[0]" / "root_cause" 为键
  pub review_id: Option<ReviewId>,
  pub pending_falsifier: Vec<FindingId>,
  pub next: Option<String>,              // 给 agent 的下一步提示，例如 "等待人工批准计划"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffDraft {
  pub from_session: SessionId,
  pub to: HandoffTarget,
  pub intent: HandoffIntent, pub mode: AccessMode,
  pub summary: String, pub body_md: String,
  #[serde(default)] pub todo: Vec<String>, #[serde(default)] pub refs: Vec<String>,
  pub self_test: Option<TestRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IdentityResolution {
  Resolved { session_id: SessionId, via: PaneResolution },
  Ambiguous { candidates: Vec<SessionRecord> },
  Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum InboxKind { Task, Handoff, Message, ReviewFindings, FalsifyRequest }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
  pub id: String, pub kind: InboxKind, pub from: Option<AuthorRef>, pub created_ms: u64,
  pub title: String, pub body_md: String,          // 正文已经渲染好，可直接作为 agent 的任务说明
  pub stage: Option<StageRef>, pub artifact: Option<ArtifactId>,
  pub handoff_id: Option<HandoffId>, pub review_id: Option<ReviewId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePipelineReq {
  pub playbook: PlaybookKind, pub project_root: String, pub goal: String,
  #[serde(default)] pub bindings: BTreeMap<StageKind, StageBinding>,   // 未给出的阶段使用 team.toml 中的默认值
  pub options: Option<PipelineOptions>,
  pub source: PipelineSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PipelineAction {
  ApprovePlan, RequestChanges { note: String }, Cancel,
  ContinueDeep, StopDeep, AdoptMitigation,
  RetryStage { stage: StageKind }, SkipStage { stage: StageKind },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DispatchTarget { Session { id: SessionId }, Pane { id: PaneId }, Stage { pipeline_id: PipelineId, stage: StageKind } }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum DeliveryLevel { L0Inbox, L1CodexQueue, L2Inject, L4Spawn }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum DeliveryStatus { Delivered, Queued, Unconfirmed, Failed }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum GateCheck { PaneAlive, NotInMode, NotSynchronized, AgentIdle, UserInactive, InputEmpty, Ack }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport { pub passed: bool, pub checks: Vec<(GateCheck, bool)>, pub detail: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReport { pub level: DeliveryLevel, pub status: DeliveryStatus, pub gate: Option<GateReport>, pub pane: Option<PaneId> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionItem {
  pub kind: String,                      // "binding_confirm" | "plan_approval" | "blocked" | "needs_human_finding" | "triage_ready" | "delivery_unconfirmed"
  pub title: String, pub pipeline_id: Option<PipelineId>, pub session_id: Option<SessionId>,
  pub review_id: Option<ReviewId>, pub since_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
  pub v: u32, pub version: String, pub started_ms: u64,
  pub sessions: Vec<SessionRecord>, pub pipelines: Vec<PipelineSpec>,
  pub attention: Vec<AttentionItem>, pub event_seq: u64,
}
```

### 1.9 SSE 推送事件（`sse`）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
  Event { seq: u64, event: NormalizedEvent },
  Session { seq: u64, session: SessionRecord },
  Pipeline { seq: u64, pipeline: PipelineSpec },
  Artifact { seq: u64, id: ArtifactId, kind: ArtifactKind, pipeline_id: Option<PipelineId> },
  Review { seq: u64, review_id: ReviewId },
  Delivery { seq: u64, report: DeliveryReport, target: DispatchTarget },
  Attention { seq: u64, items: Vec<AttentionItem> },
}
```

SSE 帧格式：`id: <seq>\nevent: <type>\ndata: <json>\n\n`。客户端通过 `Last-Event-ID` 断线重连，daemon 在内存环形缓冲中保留最近 1000 条。

---

## 2. `starfane-emit`

```rust
/// hook 热路径。不构造 tokio，不访问网络，不 panic（用 catch_unwind 包住）。
/// 返回值永远是 0。agy 会向 stdout 写 "{}\n"，claude 和 codex 不写任何内容。
pub fn run_emit<R: Read, W: Write>(agent: AgentKind, event: Option<&str>, stdin: R, stdout: W) -> i32;

/// 测试注入点：run_emit 等价于 run_emit_with(..., EnvCtx::capture(), &paths::socket_path(), &paths::spool_dir())
pub fn run_emit_with<R: Read, W: Write>(agent: AgentKind, event: Option<&str>, env: EnvCtx,
                                        socket: &Path, spool: &Path, stdin: R, stdout: W) -> i32;

pub fn hook_reply(agent: AgentKind) -> &'static str;   // Agy => "{}\n"，其余 => ""
```

行为规范：
1. 若 `STARFANE_DISABLE=1`：写入 `hook_reply` 后返回 0。
2. 读取 stdin，上限 1 MiB；能解析成 JSON 就保留，否则写入 `{"_raw": ...}`。
3. 用 `UnixStream::connect` 连接 socket，并设置 50ms 的 write timeout，写入一行 `ClientFrame::Emit`。
4. 第 3 步失败时：追加写入 spool；然后 `Command::new(current_exe).args(["daemon","--detach"])` 拉起 daemon，stdin/stdout/stderr 全部指向 null 且不等待子进程。拉起频率由 `<state>/spawn.lock` 的 mtime 控制，2 秒内只拉起一次。
5. 写入 `hook_reply` 后返回 0。**总耗时目标：p99 < 10ms（daemon 在线时）**。

---

## 3. `starfane-adapters`

```rust
pub fn normalize_hook(agent: AgentKind, event_name: &str, payload: &serde_json::Value,
                      env: &EnvCtx, recv_ts_ms: u64) -> Result<NormalizedEvent, AdapterError>;
pub fn normalize_request(req: &EmitRequest) -> Result<NormalizedEvent, AdapterError>; // 封装 normalize_hook

#[derive(Debug, Clone)] pub enum Scope { User, Project(PathBuf) }
#[derive(Debug, Clone)]
pub struct InstallOptions {
  pub scope: Scope, pub dry_run: bool,
  pub starfane_bin: PathBuf,                // 写入 hook 命令中的绝对路径
  pub agy_permission_heuristic: bool,    // 默认 false
  pub register_mcp: bool,                // 默认 true
}
#[derive(Debug, Clone, Serialize)]
pub struct FileChange { pub path: PathBuf, pub before: Option<String>, pub after: Option<String> }
#[derive(Debug, Clone, Serialize)]
pub struct InstallPlan {
  pub agent: AgentKind,
  pub file_changes: Vec<FileChange>,
  pub commands: Vec<Vec<String>>,        // 官方 CLI 命令（MCP 注册 / 注销）
  pub notes: Vec<String>,                // 例如 codex trust 指引
}
pub fn install_plan(agent: AgentKind, home: &Path, opts: &InstallOptions) -> Result<InstallPlan, AdapterError>;
pub fn uninstall_plan(agent: AgentKind, home: &Path, scope: &Scope, starfane_bin: &Path) -> Result<InstallPlan, AdapterError>;
/// 先把每个将被改动的文件原样复制到 <state>/backups/<ts_ms>/<相对 home 的路径>，再执行原子写入；
/// 写完回读并重新解析，失败就回滚。commands 按顺序执行，任一失败即中止并报告。
pub fn apply_plan(plan: &InstallPlan, backups_root: &Path) -> Result<PathBuf /*本次备份目录*/, AdapterError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)] #[serde(rename_all = "snake_case")]
pub enum Level { Ok, Warn, Error }
#[derive(Debug, Clone, Serialize)]
pub struct DoctorFinding { pub id: String, pub agent: Option<AgentKind>, pub level: Level, pub message: String, pub fix_hint: Option<String> }
#[derive(Debug, Clone)]
pub struct DoctorEnv { pub home: PathBuf, pub path_var: String, pub starfane_bin: PathBuf }
pub fn doctor_checks(env: &DoctorEnv) -> Vec<DoctorFinding>;
pub fn resolve_agent_bin(agent: AgentKind, env: &DoctorEnv) -> Option<PathBuf>;   // 结果缓存到 config 的 [paths]

#[derive(Debug, Clone)]
pub struct SpawnRequest {
  pub agent: AgentKind, pub bin: PathBuf, pub cwd: PathBuf,
  pub model: Option<String>, pub effort: Option<Effort>, pub name: Option<String>,
  pub prompt: String,                    // 单行指针 prompt（可带 "/swarm " 前缀）
  pub extra_args: Vec<String>,           // 来自 team.toml [spawn]
}
/// 返回 tmux `--` 之后的 argv：
/// Claude: [bin, "--model", m, "--effort", e, "-n", name, ...extra, prompt]
/// Codex:  [bin, "-m", m, "-c", "model_reasoning_effort=\"<e>\"", "-C", cwd, ...extra, prompt]
///         （effort 映射：Low→low，Medium→medium，High→high，Xhigh / Max→xhigh；配置键 model_reasoning_effort 已在本机 codex 0.167.2 的二进制中确认存在）
/// Agy:    [bin, "--model", m, "--effort", e, ...extra, "-i", prompt]
/// 未设置的可选项不输出对应参数。
pub fn build_l4_spawn_cmd(req: &SpawnRequest) -> Result<Vec<String>, AdapterError>;
```

### 3.1 安装的 hook（默认集）与标记

| agent | 文件 | 事件 | 条目形态 |
|---|---|---|---|
| claude | `~/.claude/settings.json`（项目级：`.claude/settings.local.json`） | SessionStart、UserPromptSubmit、PermissionRequest、Notification、Stop、StopFailure、SubagentStart、SubagentStop、SessionEnd | `hooks.<E>[] += {"hooks":[{"type":"command","command":"<bin> emit --agent claude --event <E>","async":true,"timeout":5}]}` |
| codex | `~/.codex/hooks.json`（**绝不碰 `config.toml` 的 `notify`**） | SessionStart、UserPromptSubmit、PermissionRequest、Stop、Interrupt、SubagentStart、SubagentStop、SessionEnd | 与 claude 同构；条目带 `"async":true,"timeout":5`；notes 中提示用户在 codex 内信任新的 hook 条目 |
| agy | `~/.gemini/config/hooks.json`（项目级：`.agents/hooks.json`） | SessionStart、PreInvocation、Stop；可选 PreToolUse / PostToolUse（matcher `run_command\|write_file\|edit_file`） | 顶层命名组 `"starfane": {"SessionStart":[{"type":"command","command":"<bin> emit --agent agy --event SessionStart","timeout":5}], …}` |

- **标记规则**：claude 和 codex 中，`command` 以 `<starfane_bin> emit --agent ` 开头的条目视为 starfane 所有；agy 中，顶层 key `"starfane"` 视为 starfane 所有。卸载时只删除这些条目；删除后若数组为空就删掉该事件的 key，若对象为空就删掉对象。其余内容逐字节保留，JSON 用 2 空格缩进重排。
- **MCP 注册**（走官方 CLI，不直接改文件）：
  - claude：`claude mcp add -s user starfane -- <bin> mcp --agent claude`，注销用 `claude mcp remove -s user starfane`；
  - codex：`codex mcp add starfane -- <bin> mcp --agent codex`，注销用 `codex mcp remove starfane`；
  - agy：`agy mcp add starfane <bin> mcp --agent agy`，注销用 `agy mcp remove starfane`。

### 3.2 归一化映射（规范性要求，契约测试必须全部覆盖）

| agent | 原生事件（及条件） | EventKind | Confidence |
|---|---|---|---|
| claude | SessionStart | SessionStart（model 取自 `model`） | Exact |
| claude | UserPromptSubmit | Working | Exact |
| claude | PermissionRequest；Notification 且 `notification_type=permission_prompt` | NeedsPermission | Exact |
| claude | Notification 且类型为 `idle_prompt` / `elicitation_dialog` | NeedsInput | Exact |
| claude | Notification 且类型为 `auth_success` 或其他 | Ignored | Exact |
| claude | Stop / StopFailure / SessionEnd | TurnComplete / Error / SessionEnd | Exact |
| claude | SubagentStart / SubagentStop / TeammateIdle | SubagentStart / SubagentEnd / SubagentEnd | Exact |
| claude、codex | **任意事件只要 payload 中有 `agent_id`** | 设置 `subagent_id = agent_id` | — |
| codex | SessionStart / UserPromptSubmit / PermissionRequest / Stop / Interrupt / SessionEnd | SessionStart / Working / NeedsPermission / TurnComplete / Interrupted / SessionEnd | Exact |
| codex | SubagentStart / SubagentStop | SubagentStart / SubagentEnd | Exact |
| codex、agy | PreToolUse / PostToolUse | ToolStart / ToolEnd | Exact |
| agy | SessionStart / PreInvocation | SessionStart / Working | Exact |
| agy | PostInvocation | Ignored | Exact |
| agy | Stop 且 `terminationReason ∈ {ERROR, MAX_INVOCATIONS, MAX_TOKEN_BUDGET_EXCEEDED}` | Error | Exact |
| agy | Stop 且 `terminationReason = USER_CANCELED` | Interrupted | Exact |
| agy | Stop 且 `fullyIdle == true && continuationActive != true` | TurnComplete | Exact |
| agy | Stop，其余情况 | Working | Exact |
| 任意 | 未知事件名 | Ignored（`native_event` 保留原名） | Exact |

字段来源：claude 和 codex 使用 `session_id`、`cwd`、`transcript_path`、`model`、`last_assistant_message`、`tool_name`、`tool_input`、`agent_id`、`agent_type`；agy 使用 `conversationId`、`cwd`、`transcriptPath`、`modelName`、`terminationReason`、`fullyIdle`、`continuationActive`、`error`。必需的 session 字段缺失时返回 `AdapterError::MissingField`；daemon 记录日志后丢弃该事件。

---

## 4. `starfane-tmux`

```rust
#[derive(Debug, Clone)]
pub struct Tmux { pub bin: PathBuf, pub socket_name: Option<String> }   // socket_name 对应 tmux -L（测试用 "starfane-test"）
impl Tmux {
  pub fn new(bin: PathBuf, socket_name: Option<String>) -> Self;
  pub fn available(&self) -> bool;
  pub fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError>;     // list-panes -a -F，字段以 \x1f 分隔
  pub fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError>;
  pub fn cursor_line(&self, pane: &PaneId, cursor_y: u32) -> Result<String, TmuxError>; // capture-pane -p -t P -S y -E y
  pub fn inject_pointer(&self, pane: &PaneId, text: &str, cfg: &GateConfig) -> Result<(), TmuxError>;
  pub fn spawn_window(&self, req: &SpawnWindow) -> Result<PaneId, TmuxError>;
  pub fn set_status(&self, pane: &PaneId, st: &PaneStatus) -> Result<(), TmuxError>;
  pub fn switch_to(&self, pane: &PaneId) -> Result<(), TmuxError>;  // switch-client -t P（作用于最近活跃的 client）
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneInfo {
  pub pane_id: PaneId, pub session_name: String, pub window_id: String, pub window_index: u32, pub pane_index: u32,
  pub pane_pid: u32, pub current_command: String, pub current_path: String,
  pub pane_active: bool, pub window_active: bool, pub session_attached: u32,
  pub in_mode: bool, pub dead: bool, pub synchronized: bool, pub alternate_on: bool,
  pub cursor_x: u32, pub cursor_y: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientInfo { pub tty: String, pub session_name: String, pub activity_s: u64, pub flags: String }

#[derive(Debug, Clone)]
pub struct PaneHint { pub agent: AgentKind, pub env_pane: Option<PaneId>, pub pid_chain: Vec<u32>, pub cwd: Option<String> }
pub fn pid_ancestors(pid: u32, max_depth: usize) -> Vec<u32>;        // 通过 `ps -o ppid= -p <pid>` 逐级上溯
/// 优先级：env_pane 存在且 list 中有 → Env；pid_chain 与某个 pane_pid 相交 → PidTree；
/// cwd 相同且同类 agent 只有一个候选 → CwdMatch；否则 → Unknown。
pub fn resolve_pane(panes: &[PaneInfo], hint: &PaneHint) -> (Option<PaneId>, PaneResolution);

/// 纯函数：pane 可见（session_attached>0 && window_active && pane_active），
/// 且该 session 所属 client 中最新的 activity 距今 < watch_window_secs
pub fn is_user_watching(pane: &PaneInfo, clients: &[ClientInfo], now_s: u64, watch_window_secs: u64) -> bool;

#[derive(Debug, Clone)]
pub struct GateConfig {
  pub user_inactive_secs: u64,           // 默认 15
  pub paste_settle_ms: u64,              // 默认 250
  pub ack_timeout_ms: u64,               // 默认 4000（ACK 由 daemon 等待，不在本 crate 中）
  pub max_pointer_len: usize,            // 默认 300
  pub idle_prompts: BTreeMap<AgentKind, Vec<String>>, // 光标行的空闲前缀，M2 实测后校准；默认值：claude ["> ", "│ > "]，codex ["› "]，agy ["> "]
}
/// 纯函数：依次检查 PaneAlive、NotInMode、NotSynchronized、AgentIdle（state ∈ {Idle}）、
/// UserInactive、InputEmpty（光标行去掉匹配的空闲前缀后为空白；无法判断视为不通过）。
/// 不包含 Ack。
pub fn check_injection_gate(pane: &PaneInfo, clients: &[ClientInfo], state: SessionState,
                            agent: AgentKind, cursor_line: &str, cfg: &GateConfig, now_s: u64) -> GateReport;

#[derive(Debug, Clone)]
pub struct SpawnWindow {
  pub target_session: Option<String>,    // None 时使用（或新建）detached session "starfane"
  pub window_name: String, pub cwd: PathBuf,
  pub env: Vec<(String, String)>,        // 例如 ("STARFANE_BIND", "B-…")
  pub argv: Vec<String>,                 // 来自 adapters::build_l4_spawn_cmd
}
#[derive(Debug, Clone, Serialize)]
pub struct PaneStatus { pub state: SessionState, pub role: Option<StageKind>, pub pipeline: Option<PipelineId> }
```

- `inject_pointer` 的执行顺序：校验 text 为单行且不超过 `max_pointer_len`，否则返回 `TmuxError::InvalidText` → `load-buffer -b starfane_<rand> -`（text 从 stdin 传入）→ `paste-buffer -d -p -b starfane_<rand> -t <pane>` → `sleep(paste_settle_ms)` → `send-keys -t <pane> C-m`。**本函数不做门禁检查，调用方必须先调用 `check_injection_gate`。**
- `spawn_window` 的执行顺序：`new-window -d -P -F "#{pane_id}" -n <name> -c <cwd> [-e K=V]... -t <session>: -- <argv...>` → `set-option -p -t <pane> remain-on-exit failed` → 返回 pane。如果目标 session 不存在，先执行 `new-session -d -s agent-session -c <cwd>`。
- `set_status` 写入的 pane 选项：`@starfane_state`、`@starfane_role`、`@starfane_pipeline`。

---

## 5. `starfane-evidence`

```rust
pub trait SourceProvider {
  /// 返回 commit 下的文件内容；文件不存在时返回 Ok(None)
  fn read_at(&self, commit: &str, path: &str) -> Result<Option<String>, EvidenceError>;
}
pub struct GitSource { pub repo_dir: PathBuf }                 // 实现：git -C <repo> show <commit>:<path>
impl SourceProvider for GitSource { … }

#[derive(Debug, Clone)]
pub struct G2Config { pub drift_tolerance: u32 /*3*/, pub min_quote_chars: usize /*8*/, pub check_symbols: bool /*true*/ }

pub fn validate_g1_plan(doc: &PlanDoc) -> G1Result;
pub fn validate_g1_handoff(p: &HandoffPacket) -> G1Result;
pub fn validate_g1_review(r: &ReviewReport) -> G1Result;
pub fn validate_g1_triage(t: &TriageReport) -> G1Result;
pub fn validate_g1_rca(r: &RcaReport) -> G1Result;
pub fn validate_g1_report(r: &FinalReport) -> G1Result;
pub fn validate_g1_falsifier(s: &FalsifierSubmission) -> Vec<G1Issue>;

pub fn verify_g2_review(repo_dir: &Path, report: &ReviewReport, cfg: &G2Config)
    -> Result<BTreeMap<FindingId, G2Outcome>, EvidenceError>;
pub fn verify_g2_quotes(src: &dyn SourceProvider, commit: &str, quotes: &[EvidenceQuote],
                        refs: &[CodeRef], symbol: Option<&str>, cfg: &G2Config) -> G2Outcome;
pub fn verify_g2_log_refs(refs: &[EvidenceRef], base_dir: &Path, cfg: &G2Config) -> G2Outcome; // 只处理 Log 变体
pub fn verify_g2_evidence(src: &dyn SourceProvider, commit: &str, base_dir: &Path,
                          refs: &[EvidenceRef], code_refs: &[CodeRef], cfg: &G2Config) -> G2Outcome; // triage / rca 用
pub fn normalize_ws(s: &str) -> String;   // 每行 trim，行内连续空白折叠为一个空格，丢弃空行
```

**G1 必填规则**（按 depth 递增）：

| 规则 | 适用 |
|---|---|
| `claim` 非空；`evidence` 至少 1 条；每条 quote 的非空白字符数 ≥ `min_quote_chars`；`line_start ≤ line_end`；`confidence ∈ [0, 1]` | 全部 |
| `trigger_path` 至少 1 条 | D0+ |
| `checked_guards` 至少 1 条，`observable_impact` 非空 | D1+ |
| `trigger_path` 至少 2 条，且 `preconditions` 非空 | D2+ |
| severity 为 blocker 时，`repro.kind != None` 或 `verification_method == ReproTest` | D3 |
| findings 数量 ≤ 上限（D0:5 / D1:8 / D2:8 / D3:10）；D0 只允许 blocker / major | 报告级 |
| `findings` 为空时 `checked_areas` 至少 1 条 | 报告级 |

**G2 算法**：
1. 读取 `report.head` 对应的文件；文件不存在 → `FileMissing`。
2. quote 经 `normalize_ws` 后得到 k 行，与文件按行 normalize 后的序列做**连续子序列**匹配：
   - 在 `[line_start − tol, line_start + tol]` 内命中 → 无 issue；
   - 窗口外唯一命中 → `QuoteRelocated`（判为 Relocated，仍接受）；
   - 窗口外多处命中 → `QuoteAmbiguous`（判为 Fabricated）；
   - 完全没有命中 → `QuoteNotFound`（判为 Fabricated）。
3. `trigger_path` 和 `checked_guards` 中的每个 CodeRef：文件必须存在，且 `line ≤ line_count`，否则记 `LineOutOfRange` / `FileMissing`（判为 Fabricated）。
4. 若 `check_symbols` 为真且有 `symbol`：在 `location.file` 中按单词边界搜索 symbol，找不到记 `SymbolNotFound`（判为 Fabricated）。
5. Log 引用：文件必须存在；quote 经 normalize 后必须是文件中某一行（或连续多行）的子串，给了 `line` 时同样适用 ±tol 规则。

最终 verdict：只要有一条 issue 会导致 Fabricated，结果就是 `Fabricated`；否则只要出现过 `QuoteRelocated`，结果就是 `Relocated`；都没有则为 `Verified`。

---

## 6. `starfane-binding`

```rust
#[derive(Debug, Clone, Default)]
pub struct BindingContext {
  pub aliases: BTreeMap<String, String>,                  // 别名 → 简写，例如 "opus" => "claude:opus"、"neon" => "agy:gemini-4-neon-high"
  pub known_models: BTreeMap<AgentKind, Vec<String>>,     // doctor 缓存的可用模型列表（例如 agy models 的输出）
}
#[derive(Debug, Clone, Default)]
pub struct PlaybookDefaults { pub feature: BTreeMap<StageKind, StageBinding>, pub debug: BTreeMap<StageKind, StageBinding> }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "warning", rename_all = "snake_case")]
pub enum BindingWarning {
  SameModelReviewer { model: String },                    // TestReview 与 Implement 解析后使用同一个 agent 和 model
  NativeAgentMismatch { stage: StageKind, native: NativeMode, agent: AgentKind },
  UnknownModel { stage: StageKind, agent: AgentKind, model: String },
  StageNotInPlaybook { stage: StageKind, playbook: PlaybookKind },
  Unrecognized { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedBinding {
  pub playbook: PlaybookKind,
  pub playbook_explicit: bool,
  pub goal: Option<String>,
  pub bindings: BTreeMap<StageKind, StageBinding>,        // 只包含话术中显式出现的阶段
  pub resolved: BTreeMap<StageKind, StageBinding>,        // 补全默认值、展开 same_as 之后的完整结果
  pub warnings: Vec<BindingWarning>,
}

pub fn parse_binding_utterance(text: &str, default_playbook: Option<PlaybookKind>, ctx: &BindingContext) -> ParsedBinding;
pub fn apply_defaults(parsed: &mut ParsedBinding, defaults: &PlaybookDefaults);
pub fn validate(parsed: &ParsedBinding, ctx: &BindingContext) -> Vec<BindingWarning>;
```

**词表（规范性要求，大小写不敏感，中文按子串匹配，英文按单词边界匹配）**：

| 类别 | 词 → 值 |
|---|---|
| playbook | 新功能、功能、需求、feature、feat → Feature；查错、排查、排障、调试、debug、bug、报错、故障、500、崩溃 → Debug |
| 阶段（Feature） | 计划、规划、设计、方案、plan、design → Plan；实现、写代码、开发、编码、implement、build、code → Implement；测试、评审、审查、review、test、qa、cr → TestReview；总结、汇报、文档、report、summary → Report |
| 阶段（Debug） | 快筛、快速、初查、初步、定位、triage、fast → Triage；深挖、深入、根因、修复、deep、rca → Deep |
| agent | claude、cc、opus、sonnet、haiku → Claude（opus / sonnet / haiku 同时作为 model）；codex、gpt* → Codex（`gpt*` 作为 model）；agy、antigravity、gemini* → Agy（`gemini*` 作为 model） |
| effort | 低、low → Low；中、medium → Medium；高、高强度、high → High；xhigh、超高 → Xhigh；max、最高 → Max |
| native | /swarm、swarm → Swarm；/boost、boost → Boost；/teamwork、teamwork → Teamwork；subagent、子代理、并行 → Subagents |
| 连接词 | 「都用」「也用」：一个执行者覆盖同一子句内的多个阶段；「其余」「其他」「剩下」：填充所有未绑定的阶段 |

**算法**：
1. 判定 playbook（显式参数优先，其次看词表，默认 Feature）；
2. 按 `，,；;。\n` 以及「然后 / 再 / 最后」切分子句；
3. 在每个子句中收集阶段词集合 S 和执行者 X（agent、model、effort、native；遇到别名先展开）；
4. S 非空且 X 非空 → 为 S 中的每个阶段写入 X；
5. S 为空、X 也为空，且该子句是第一个子句 → 将其作为 goal；
6. 其余无法配对的子句 → 记 `Unrecognized` 警告；
7. 调用 `apply_defaults`，再调用 `validate`。

**验收**：M3 要求 ≥ 30 条表驱动样例；至少包括 architecture §3.4 中的 5 条样例，以及纯英文、中英混合、只有 goal、`其余用 codex`、`/swarm` 配错 agent 等情况。

---

## 7. `starfane-daemon`

```rust
pub struct DaemonConfig {
  pub state_dir: PathBuf, pub socket: PathBuf, pub config: StarfaneConfig,
  pub web_dir: Option<PathBuf>,          // 开发热重载；None 时使用嵌入资产
  pub foreground: bool,
}
pub async fn run(cfg: DaemonConfig) -> Result<(), DaemonError>;   // 绑定 socket（独占 pid 文件锁）→ 回放 spool → 启动各子系统
pub struct Core { /* Arc 内部状态 */ }
impl Core { pub async fn handle(&self, cmd: DaemonCommand) -> Result<serde_json::Value, ApiError>; }

pub mod client {   // 同步实现，供 CLI 和 MCP 使用
  pub struct DaemonClient { /* UnixStream */ }
  impl DaemonClient {
    pub fn connect(socket: &Path, autostart: bool) -> Result<Self, ClientError>; // autostart=true 时拉起 daemon，并以 50ms 间隔重试，最多 2 秒
    pub fn call(&mut self, cmd: DaemonCommand) -> Result<serde_json::Value, ClientError>;
  }
}
pub mod mcp { pub fn run_stdio(agent: AgentKind, socket: &Path) -> i32; }   // 同步实现，不使用 tokio
```

内部模块（daemon 作者自行实现，跨模块不需要冻结）：`ingest`、`registry`、`router`、`pipeline`、`delivery`、`store`、`web`、`lark`、`config`。

### 7.1 用户配置 `~/.config/starfane/config.toml`

```toml
[web]
bind = "127.0.0.1"
port = 7878

[notify]
watch_window_secs = 90
escalate_after_secs = 180
away_digest_after_secs = 600
cooldown_secs = 120

[gate]
user_inactive_secs = 15
paste_settle_ms = 250
ack_timeout_ms = 4000
max_pointer_len = 300

[feishu]
enabled = false
lark_cli = "lark-cli"
chat_id = ""
allow_open_ids = []
allow_chat_ids = []

[paths]          # 由 doctor 填写绝对路径
claude = ""
codex = ""
agy = ""
tmux = ""

[discovery]
include_shell_panes = false

[tmux]
socket_name = ""   # 测试时设为 "starfane-test"

[tmux.backup]
enabled = false
interval_secs = 300
max_snapshots = 20
auto_restore_on_start = false
restore_agents = true
restore_bypass_permissions = false
exclude_sessions = []
```

项目级 `.starfane/team.toml` 沿用 architecture §3.4（`[playbooks.feature]`、`[playbooks.debug]`、`[models.aliases]`、`[spawn]`），其中的值都使用 `StageBinding` 简写语法。

### 7.2 REST + SSE

鉴权：`Cookie: starfane_token=<t>` 或请求头 `X-Starfane-Token: <t>`。token 每次 daemon 启动时随机生成，写入 `<state>/web.token`（权限 0600）。所有请求都要校验 `Host ∈ {127.0.0.1:<port>, localhost:<port>}`；POST 请求还要校验 `Origin` 与 Host 同源。响应体统一为 `DaemonResponse` 去掉 `id` 后的结构 `{ok, data, error}`；HTTP 状态码映射：BadRequest→400、NotFound→404、Conflict→409、GateRejected→422、Unavailable→503、Internal→500。

| 方法 | 路径 | DaemonCommand / 说明 |
|---|---|---|
| GET | `/` 、`/assets/*` | 嵌入的静态资源（不需要 token，页面加载后读取 cookie） |
| GET | `/auth?token=<t>` | 设置 cookie 后 302 跳转到 `/` |
| GET | `/api/health` | 返回 `{ok, version, uptime_ms}`（不需要 token） |
| GET | `/api/state` | GetState |
| GET | `/api/events` | SSE 推送（§1.9），支持 `Last-Event-ID` |
| GET | `/api/sessions?project=&subagents=1` | ListSessions |
| POST | `/api/pipelines/parse` | ParseUtterance `{text, playbook?, project?}` |
| GET / POST | `/api/pipelines` | ListPipelines / CreatePipeline `{req}` |
| GET | `/api/pipelines/{id}` | GetPipeline |
| POST | `/api/pipelines/{id}/confirm` | ConfirmPipeline `{overrides?}` |
| POST | `/api/pipelines/{id}/action` | PipelineAction `{action, …}` |
| POST | `/api/debug` | 快捷入口：`{project_root, symptom, utterance?}` → ParseUtterance(Debug) + CreatePipeline |
| POST | `/api/debug/{id}/continue` 、`/stop` 、`/mitigate` | PipelineAction 的 ContinueDeep / StopDeep / AdoptMitigation |
| GET / POST | `/api/handoffs` | ListHandoffs / CreateHandoff `{draft}`（人工发起交接） |
| GET | `/api/artifacts/{id}` | GetArtifact |
| GET | `/api/reviews?pipeline=` 、`/api/reviews/{id}` | ListReviews / GetReview |
| POST | `/api/reviews/{id}/findings/{fid}/feedback` | ReviewFeedback `{label, note?}` |
| POST | `/api/reviews/{id}/findings/{fid}/verdict` | HumanVerdict `{verdict, note?}` |
| GET | `/api/reviews/precision?agent=&model=` | GetPrecision |
| POST | `/api/dispatch` | Dispatch `{target, text, allow?}` |
| POST | `/api/sessions/{id}/focus` | 调用 tmux `switch_to` |
| POST | `/api/feishu/test` | FeishuTest `{text}` |

### 7.3 MCP（stdio，手写实现）

- 方法：`initialize`（如果客户端请求的 `protocolVersion` 在 `["2024-11-05","2025-03-26","2025-06-18"]` 中就原样回显，否则返回 `"2025-06-18"`；`capabilities: {tools: {}}`；`serverInfo: {name: "starfane", version}`）、`notifications/initialized`、`ping`、`tools/list`、`tools/call`。
- `tools/call` 的返回：`{content: [{type: "text", text: <data 的 JSON 字符串>}], structuredContent: <data>, isError: <!ok>}`。
- 身份：启动时调用 `ResolveIdentity`（参数来自 `$STARFANE_BIND`、`$TMUX_PANE`、cwd 和 `--agent`）；结果为 Resolved 就缓存。每个工具都接受可选的 `from_session` 参数，它的优先级高于缓存；Ambiguous 或 Unknown 时，工具返回 `isError: true` 并附上候选列表，提示 agent 带上 `from_session` 重试。

| # | 工具 | inputSchema（JSON Schema 摘要） | 返回 `data` |
|---|---|---|---|
| 1 | `list_sessions` | `{project?: string, include_subagents?: boolean}` | `{sessions: SessionRecord[]}` |
| 2 | `create_handoff` | `{to: {session_id?, pane?, agent?, stage?: {pipeline_id, stage}}, intent: enum, mode: enum, summary: string(≤500), body_md: string, todo?: string[], refs?: string[], self_test?: {cmd, outcome: "pass"\|"fail"\|"not_run", output_tail?}, from_session?: string}`，必填：`to, intent, mode, summary, body_md` | `{handoff_id, delivery: DeliveryReport}` |
| 3 | `read_inbox` | `{mark_delivered?: boolean = true, from_session?: string}` | `{items: InboxItem[]}` |
| 4 | `ack_handoff` | `{handoff_id: string, status: "acked"\|"done", note?: string, from_session?: string}` | `{}` |
| 5 | `submit_review` | `{review: ReviewReport（header 可省略）, from_session?: string}` | `SubmitOutcome` |
| 6 | `submit_falsifier_verdict` | `{review_id, finding_id, verdict: "confirmed"\|"needs_human"\|"downgraded"\|"disproved", to?: severity（downgraded 时必填）, rationale: string, counter_evidence?: EvidenceQuote[], from_session?}` | `{accepted, g2, effective}` |
| 7 | `submit_triage` | `{triage: TriageReport（header 可省略）, from_session?}` | `SubmitOutcome` |
| 8 | `report_milestone` | `{kind: "plan"\|"rca"\|"final_report"\|"progress", plan?: PlanDoc, rca?: RcaReport, report?: FinalReport, text?: string, notify_human?: boolean, from_session?}`；kind 为 plan / rca / final_report 时，对应字段必填；kind 为 progress 时 `text` 必填 | plan / rca / final_report 返回 `SubmitOutcome`；progress 返回 `{}` |
| 9 | `start_pipeline` | `{playbook?: "feature"\|"debug", goal: string, utterance?: string, bindings?: {[stage]: shorthand}, project_root?: string, from_session?}` | `{pipeline: PipelineSpec, parsed: ParsedBinding, needs_confirmation: true}` |

> 说明：第 9 个工具 `start_pipeline` 是在 coordinator 给出的 8 个工具之外补充的。它支撑 architecture §3.4 中「在对话里下达，由 agent 自己解析话术」的入口。没有它，「在对话里说一句就启动流程」无法实现。所有 inputSchema 的完整 JSON 由 `starfane-proto` 中的 `mcp_tool_schemas() -> serde_json::Value` 统一生成，并做快照测试。

工具与 `DaemonCommand` 的映射：
- 1 → ListSessions
- 2 → CreateHandoff
- 3 → ReadInbox
- 4 → AckHandoff
- 5 / 7 / 8（plan、rca、final_report）→ SubmitArtifact
- 8（progress）→ ReportProgress
- 6 → SubmitFalsifierVerdict
- 9 → ParseUtterance + CreatePipeline

### 7.4 飞书双向

**出站**（daemon `lark` 模块；参数均来自 `lark-cli --help` 实测）：

| 用途 | 命令 |
|---|---|
| 新卡片 | `lark-cli im +messages-send --as bot --chat-id <oc> --msg-type interactive --content <card_json> --idempotency-key <≤50>` |
| 话题内回复 | `lark-cli im +messages-reply --as bot --message-id <root_om> --reply-in-thread --msg-type interactive --content <json>`（或用 `--markdown`） |
| 更新卡片 | `lark-cli im messages patch --as bot --message-id <om> --data '{"content":"<card_json 字符串>"}'`（仅限 14 天内发出的卡片；content ≤ 30KB） |

幂等键为 `<event_id 或 pipeline_id>-<kind>`，截断到 50 个字符。每个 pipeline 的第一张卡片作为 `feishu_thread.root_message_id`，后续卡片都在该话题内回复。

**入站**：daemon 为 `im.message.receive_v1` 和 `card.action.trigger` 各启动一个 `lark-cli event consume <key> --as bot` 子进程。stdin 使用 piped 且**一直保持打开**；读 stderr 直到出现 `[event] ready event_key=<key>`；stdout 按 NDJSON 解析；退出时发送 SIGTERM；子进程异常退出后按指数退避重启（1 秒起，最长 60 秒）。

**文本指令语法**（私聊 bot 或在白名单群中 @bot；中英文等价；`<项目>` 按已注册的 git root basename 唯一前缀匹配）：

| 指令 | 语义 |
|---|---|
| `帮助` / `help` | 显示语法 |
| `状态` / `ls` / `status [P-id]` | GetState 摘要 / 单个 pipeline 详情 |
| `新功能 <项目> <目标>[；绑定话术]` / `feature …` | ParseUtterance(Feature) + CreatePipeline → 回复绑定确认卡 |
| `查错 <项目> <现象>[；绑定话术]` / `debug …` | ParseUtterance(Debug) + CreatePipeline → 回复绑定确认卡 |
| `回复 <目标> <文本>` / `reply …` | Dispatch。`<目标>` 可以是 `%12`、`s:<session_id>` 或 `P-…/<stage>`；**在卡片话题中直接回复时可省略 `<目标>`**，默认指向该话题对应 pipeline 当前阶段的会话 |
| `确认 [P-id]` / `confirm` | ConfirmPipeline，或对处于 AwaitingHuman 的计划执行 ApprovePlan（省略 P-id 时取话题上下文） |
| `拒绝 [P-id] [理由]` / `reject` | 绑定待确认阶段 → Cancel；计划待批阶段 → RequestChanges{note: 理由} |
| `停止 <P-id>` / `stop` | PipelineAction::Cancel |
| `静音 <30m\|2h>` / `mute`；`取消静音` / `unmute` | 暂停飞书推送（Web 不受影响） |

**卡片回调**的 `action_value` 统一为 `{"starfane":1,"op":"<op>","pipeline_id":"P-…","review_id"?:"…","finding_id"?:"…","nonce":"<8 位>"}`。`op` 取值：`confirm_pipeline`、`cancel_pipeline`、`approve_plan`、`request_changes`、`continue_deep`、`stop_deep`、`adopt_mitigation`、`feedback_valid`、`feedback_fp`、`feedback_wontfix`。绑定确认卡的 `form_value` 以阶段名为键，值为简写语法（来自 `select_static`）。

**安全**：`operator_id` 必须在 `allow_open_ids` 中，`chat_id` 必须在 `allow_chat_ids` 中（私聊豁免 chat 检查），否则只记审计日志不执行；同一个 nonce 只处理一次；远程文本先清洗（去除控制字符、截断到 2KB、换行折叠为空格）再进入 Dispatch。

---

## 8. `bin/starfane` CLI

全局参数：`--json`（机器可读输出）、`--socket <path>`。退出码：0 成功；1 错误；2 用法错误；3 daemon 不可用；4 门禁拒绝。

| 子命令 | 参数 | 实现 |
|---|---|---|
| `emit` | `--agent <claude\|codex\|agy> [--event <E>]` | `starfane_emit::run_emit`（**在 main 中最先匹配**，此时尚未初始化 clap 以外的任何东西） |
| `daemon` | `[--detach] [--foreground] [--web-dir <dir>] [--port <n>]` | `starfane_daemon::run`；`--detach` 时 fork 到后台并重定向日志到 `<state>/starfaned.log` |
| `doctor` | `[--json]` | `adapters::doctor_checks` + tmux / Rust / 磁盘 / lark-cli 检查 |
| `install` | `[--agent <a>]... \| --all  [--scope user\|project] [--dry-run] [--agy-permission-heuristic] [--no-mcp]` | install_plan → 打印 diff → `apply_plan`（`--dry-run` 时不执行） |
| `uninstall` | `[--agent <a>]... \| --all  [--scope] [--dry-run]` | uninstall_plan → apply_plan |
| `ls` | `[--all] [--project <dir>]` | ListSessions + ListPipelines，输出表格 |
| `mcp` | `--agent <a>` | `starfane_daemon::mcp::run_stdio` |
| `handoff` | `--from <sel> --to <sel> [--intent review] [--mode read-only] [-m <summary>] [--body-file <f>]` | CreateHandoff（由 CLI 自动收集 git diff --stat 作为 body） |
| `pipeline` | `new "<话术>" [--project <dir>] [--playbook feature\|debug] [--yes]` \| `list` \| `show <id>` \| `confirm <id>` \| `approve <id>` \| `reject <id> [-m]` \| `cancel <id>` | ParseUtterance → 展示绑定表 → 交互确认（或 `--yes`）→ Create + Confirm |
| `debug` | `"<现象；绑定话术>" [--project] [--yes]` \| `continue <id>` \| `stop <id>` | 等价于 `pipeline new --playbook debug` |
| `review` | `show <R-id>` \| `feedback <R-id> <F-id> valid\|false-positive\|wontfix [-m]` \| `verdict <R-id> <F-id> <verdict>` \| `precision` | 对应的 DaemonCommand |
| `web` | `[--print-url] [--no-open]` | 读取 `web.token`，用 `open` 打开 `http://127.0.0.1:<port>/auth?token=…` |
| `tmux` | `backup [--force]` \| `restore [--file <path>] [--no-agents] [--dry-run]` \| `backups` | 备份与恢复 tmux 会话/窗口/分屏布局及可选恢复 `claude --resume` / `codex resume`（daemon 在线或离线均可） |

选择器 `<sel>`：`%12`（pane）、`s:<session_id>`、`P-…/<stage>`，或 agent 名（要求在当前项目内唯一）。

---

## 9. `tools/fake-agent`

```
fake-agent --agent <claude|codex|agy> --script <file.toml> [--emit-bin <path>]
```

行为：在当前 pane 中打印该 agent 的空闲提示符（取 `GateConfig.idle_prompts` 的默认值），从 stdin 按行读取输入，并按 script 依次通过 `<emit-bin> emit --agent X --event E` 投递 fixture payload（payload 中的 `session_id` / `conversationId` 等字段做模板替换）。也可以把 `run_emit_with` 作为库直接调用。

脚本格式：`[[step]] on = "start" | "input" | "after_ms:<n>"`，`event = "Stop"`，`payload = "<fixtures 相对路径>"`，`set = { fullyIdle = true }`。用于 M1–M4 在沙箱 `tmux -L starfane-test` 中做端到端测试。

---

## 10. 测试契约与并行分工建议

| 资产 | 位置 | 要求 |
|---|---|---|
| hook stdin 样本 | `tests/fixtures/<agent>/<Event>[__<variant>].json` | 每个 agent 覆盖 §3.2 中的每一行；内容来自真实 schema 的最小化样本 |
| 归一化契约测试 | `starfane-adapters/tests/normalize.rs` | 每个 fixture 都断言 EventKind、Confidence、subagent_id 和 model |
| install 往返测试 | `starfane-adapters/tests/roundtrip.rs` | 在 tempdir 中构造 HOME（预置用户已有的 PostToolUse、notify 等内容），install 再 uninstall 后与原文件逐字节一致 |
| G2 金标测试 | `starfane-evidence/tests/golden/` | 一个小型 git 仓库 fixture，包含 verified / relocated / ambiguous / fabricated / log 各类样例 |
| 话术测试 | `starfane-binding/tests/utterances.toml` | ≥ 30 条，格式为 `{text, expect: {stage: shorthand}, warnings: [...]}` |
| 门禁测试 | `starfane-tmux/tests/gate.rs` | 纯函数矩阵测试；外加在 `tmux -L starfane-test` 中跑的集成测试（用 `#[ignore]` 标记，CI 显式开启） |
| MCP schema 快照 | `starfane-proto/tests/mcp_schemas.rs` | 对 `mcp_tool_schemas()` 做快照测试 |

**Wave 2 并行切分**（单向依赖，接口以本文为准）：
- **W2-0**：`starfane-proto`（先行，约 1 小时，完成后冻结）；
- **W2-0 合入后并行**：
  - W2-a：`starfane-emit` + `starfane-adapters` + fixtures；
  - W2-b：`starfane-tmux`；
  - W2-c：`starfane-evidence`；
  - W2-d：`starfane-binding`；
  - W2-e：`starfane-daemon`（core、store、web、lark、mcp）；
  - W2-f：`web/` 前端（只依赖 §7.2 REST 与 §1.9 SSE）；
  - W2-g：`tools/fake-agent`；
- **最后**：`bin/starfane` 负责把各 crate 串起来（由 W2-e 的作者负责）。
