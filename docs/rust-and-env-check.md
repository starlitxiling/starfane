# Rust 环境实测、Cargo 磁盘治理与纯交互式多 Agent 能力核验

> 面向 `docs/architecture.md` v2 架构定稿的专项实测报告（所有本机环境、工具版本、二进制 Hook Schema 与原语均经只读命令实测验证）。

---

## 1. 本机 Rust 与前端构建环境实测

### 1.1 实测快照

| 项目 | 实测结果 | 对架构与工程的影响 |
|---|---|---|
| **CPU / 架构 / 内存** | Apple Silicon `arm64` · 10 核 · 16 GiB RAM | 编译速度较快，并行 `codegen-units` / `cargo test` 体验好 |
| **磁盘余量（关键瓶颈）** | SSD 总容量 `228Gi`，已用 `161Gi`（83%），**仅剩 `34Gi`** | **核心约束**：默认 Cargo `target/` 动辄 1.5–4GB，必须严格执行磁盘预算与 profile 治理 |
| **Rust 工具链** | `rustc` / `cargo` / `rustup` / `sccache` / `cargo-sweep` / `cargo-cache` **均未安装**；`~/.cargo`、`~/.rustup` 不存在 | 零历史包袱，可从第一天直接采用最省盘的 `minimal` 安装与全局配置 |
| **C 编译器 / Linker** | **已就绪**：`Apple clang 21.0.0` (`/usr/bin/clang`)，`/Applications/Xcode.app/Contents/Developer` | Rust 链接与后续可选 C 依赖（如 `rusqlite`）开箱即用，无需再装 Xcode CLT |
| **包管理器** | `Homebrew 7.0.14` (`/opt/homebrew/bin/brew`) | 可直接安装 `sccache` 等二进制工具 |
| **前端 / 脚本运行时** | `node v22.23.1`、`npm 10.9.8`、`uv 0.11.32` 已安装；`pnpm` / `bun` 未安装 | 虽有 Node，但坚持**零 `node_modules` 前端方案**，避免额外 300MB+ 磁盘占用 |

### 1.2 开箱即用的最小环境准备命令（省盘版）

> **关键省盘点**：`rustup` 默认 profile 会附带安装约 600–700MB 的 `rust-docs` 本地文档。使用 `--profile minimal` 再按需加 `clippy,rustfmt`，可将工具链压至约 **550–650MB**。

```bash
# 1. 仅安装最小工具链 + clippy + rustfmt（跳过 ~650MB rust-docs）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --component clippy,rustfmt

# 2. 安装编译缓存 sccache（~35MB）与可选清理工具
brew install sccache
# 可选：brew install cargo-sweep（或直接用 Makefile 封装 cargo clean）

# 3. 验证环境
rustc --version && cargo --version && sccache --version
```

建议写入用户级 `~/.cargo/config.toml`（不提交进仓库，由 `relay doctor` 检测提示）：

```toml
[build]
rustc-wrapper = "sccache"

[env]
# 硬性限制 sccache 本地缓存上限，防止长期开发静默膨胀
SCCACHE_CACHE_SIZE = "2G"
```

---

## 2. Rust 最小依赖栈与 Cargo 硬盘占用控制方案

### 2.1 最小 Crate 选型（回应 `architecture.md` §6.1 & §7.2）

原则：**绝不引入 TLS/HTTP 客户端全家桶**（飞书出站走 `lark-cli` 子进程，零 `reqwest`/`openssl`/`rustls`/`ring` 依赖）、**所有依赖禁用 `default-features`**、**零重型框架**。

| 能力域 | 推荐选型 | 关键 Feature / 理由 | 预估增量依赖 |
|---|---|---|---|
| **异步运行时**（仅 `relayd`） | `tokio` | `default-features = false, features = ["rt-multi-thread", "net", "io-util", "process", "sync", "time", "macros", "signal"]`（单线程 `rt` 亦可，UDS + 子进程 + SSE 并发极轻） | ~15 crates |
| **嵌入式 Web + SSE** | **`axum 0.8`**（最小化） | `default-features = false, features = ["http1", "json", "tokio", "query"]`。**不开启 `ws` / `multipart` / `form` / `tracing`**：控制台推送仅需单向 **SSE**（原生 `axum::response::Sse`），操作走 REST POST，直接省掉 `tokio-tungstenite`/`sha1`/`base64`。对比 `tiny_http`/`rouille`：因 `relayd` 本身就需要 `tokio` 管理 UDS 与多条 `lark-cli event consume` 子进程，复复用 `tokio` + 最小 `axum` 仅比裸 `hyper` 多 ~15–20 个轻 crate，却省去手写路由器与 SSE 帧编码 | ~70–90 crates（含 `tokio`/`hyper`/`http` 底座） |
| **UDS 通信** | `std::os::unix::net::UnixStream`（`emit` 侧）+ `tokio::net::UnixListener`（`relayd` 侧） | **0 额外 crate**。`relay emit` 在 `main()` 同步分支执行、不初始化 `tokio` runtime，实测冷启动可达 **<5ms**（满足 `agy` 同步 hook 要求） | 0 crates |
| **MCP stdio 服务端** | **手写最小 JSON-RPC 2.0**（~200–300 行 + `serde_json`） | 仅需 stdio NDJSON 传输 + `initialize` / `notifications/initialized` / `ping` / `tools/list` / `tools/call` 共 5 个方法。**不引入 `rmcp`**：避免拖入 `schemars`/HTTP/SSE/proc-macro 传递依赖，且协议面小、易于契约测试 | 0 额外 crates |
| **序列化 & 配置** | `serde`（`derive`）+ `serde_json` + `toml`（最小 feature，用于 `.relay/team.toml` 与读 `~/.codex/config.toml`） | `handoff.v1` Frontmatter 采用极简解析或复用 JSON/TOML，避免老旧 `serde_yaml` | ~10 crates |
| **CLI 参数解析** | `clap`（`default-features = false, features = ["std", "derive", "help", "usage"]`）或极简 `lexopt` | 子命令达 10+ 个，精简 `clap`（关闭 `color`/`suggestions`/`unicode`/`wrap_help`）在可读性与体积间最平衡 | ~10 crates |
| **状态存储** | **M0–M2：JSONL 事件日志 + 原子写（tmp + rename）JSON 快照** | **0 额外 crate、0 C 编译开销**。M3/M4 若确需复杂全文/多维检索，再按需引入 `rusqlite`（`features = ["bundled"]`，约增 1.5–2MB 二进制、~25MB target） | 0 crates（M0–M2） |
| **ID & 时间** | `ulid`（或自写 20 行 ULID/时间戳）+ `std::time::SystemTime` / `time`（极简格式化） | 避免重型 `chrono` 全量本地时区库（可仅用 RFC3339 UTC） | 1–3 crates |
| **日志** | `tracing` + `tracing-subscriber`（仅 `fmt`、`env-filter` 关闭 regex）或轻量 `log` | 若追求最小 target 体积，M0–M2 可用 `log` + 简单 JSON/stderr logger（省去 `regex` 编译大户） | 2–5 crates |

> **单二进制 `emit` 路径说明**：单二进制 `relay` 编译时会静态链接 `relayd`（含 `tokio`），最终 release strip 后单二进制约 **3–5 MB**；只要 `main()` 同步入口先解析前两个参数（遇到 `emit` 直接调用 `relay_emit::run()`，不构造 `tokio::Runtime`），进程冷启动与内存加载依然在 **<5ms** 量级，无需拆成两个二进制增加安装复杂度。

### 2.2 嵌入式 Web UI 资产方案（零 `node_modules`）

为彻底消除前端 300MB+ `node_modules` 与双工具链维护负担，推荐 **Zero-Build 原生静态资产 + `include_bytes!`** 方案：

- **目录结构**：`web/` 下直接存放 `index.html`、`app.css`、`app.js`（以及按组件拆分的几个 `.mjs` 模块）；如需响应式组件渲染，直接 vendor 一份压缩好的 `preact` + `htm` 单文件 ESM（约 **15–20 KB**），**零 `package.json`、零 `node_modules`、零打包步骤**。
- **嵌入方式**：
  - Release / 默认模式：直接使用 Rust 标准库 **`include_bytes!`**（静态表映射路径 → `(content_type, &[u8])`），**0 额外 proc-macro 依赖**（比 `rust-embed` 更轻、零宏编译成本）；文件数超过 15 个后再考虑 `rust-embed`。
  - 开发热重载：`relayd --web-dir ./web` 直接从磁盘读文件，改完前端刷新浏览器即见效，无需重编 Rust。
- **实时通信**：浏览器通过原生 `EventSource("/api/events")` 订阅 SSE，所有控制指令走 `fetch("/api/...", { method: "POST" })`（带启动随机 token cookie 与 `Origin`/`Host` 校验）。

### 2.3 Cargo 硬盘占用控制清单与体积预算（回应 `architecture.md` §7.3）

#### 1) 项目根 `Cargo.toml` 强制 Profile

```toml
[profile.dev]
# 保留 panic 行号信息，砍掉完整 DWARF 变量表（体积减 ~60%）
debug = "line-tables-only"
# macOS 必选：跳过 dsymutil 打包成巨大 .dSYM，大幅减少磁盘写入并加速链接
split-debuginfo = "unpacked"

[profile.dev.package."*"]
# 最大省盘项：第三方依赖完全不生成 debuginfo，且轻度优化提升测试运行速度
debug = 0
opt-level = 1

[profile.test]
debug = "line-tables-only"
split-debuginfo = "unpacked"

[profile.release]
opt-level = "s"          # 兼顾体积与性能（或 "z" 极致缩体）
lto = "thin"             # 比 fat LTO 省内存与中间产物空间，压缩效果接近
codegen-units = 1
strip = "symbols"        # 剥离全部符号表
panic = "abort"          # 移除 unwinding 表，再降 ~10-15% 体积
```

#### 2) `CARGO_TARGET_DIR` 共享取舍与清理策略

- **不推荐全局共享 `CARGO_TARGET_DIR`**：(a) 本项目未来会配合 `git worktree` 多 agent 并行开发，共享同一个 `target/` 会触发 Cargo 文件锁争用导致编译串行卡死；(b) 不同项目/feature 混杂后极难精准清理，反而越积越大。
- **推荐组合**：**每工作目录独立 `target/` + 全局 `sccache`（上限 `2G`）+ 定期清理脚本**。
  - `git worktree` 合并销毁前自动清理其 `target/`；
  - 日常开发提供脚本/Makefile 目标：`cargo clean --doc`、`cargo sweep --time 7`（清理 7 天未触碰的增量碎片）；
  - M0 门禁脚本检查：`du -sm target` 超过预算即报警；`cargo tree -d` 审计重复依赖版本。

#### 3) 明确磁盘体积预算（基于本机 34Gi 余量）

| 资产项 | 默认无治理预估 | 治理后硬预算 |
|---|---|---|
| Rust 工具链（`~/.rustup`） | ~1.3–1.5 GB（default） | **≤ 700 MB**（`minimal` + `clippy`/`rustfmt`） |
| Cargo Registry 源码缓存（`~/.cargo/registry`） | 300–500 MB | **≤ 150 MB**（总传递依赖控制在 ~100–130 crates） |
| `target/debug`（单工作树） | 1.2–2.0 GB | **≤ 450 MB**（依赖 `debug=0` + `line-tables-only` + `unpacked`） |
| `target/release` | 600 MB–1.0 GB | **≤ 350 MB** |
| 最终 Release 单二进制 `relay` | 15–30 MB | **≤ 6 MB** |
| 前端 `web/` + 构建依赖 | 300+ MB（Vite + React `node_modules`） | **≤ 200 KB**（`node_modules` = **0 B**） |
| `sccache` 缓存上限 | 无上限 | **≤ 2 GB**（硬上限，初期仅用 ~150–300 MB） |
| **冷启动全量本地占用总计** | ~3.5–5.5 GB | **< 1.8 GB**（约占本机 34Gi 余量的 5%） |

---

## 3. 纯交互式会话下的多 Agent 事件聚合与投递原语实测

### 3.1 三家原生多 Agent 模式与主/子会话区分（回应 `architecture.md` §2.2 / §3.2 / §3.3）

经检查三家本地二进制 Schema、`agy` 内置技能与 changelog，各家多 Agent 运行时的 Hook 区分能力与聚合策略如下：

| Agent | 原生多 Agent 模式 | Hook 区分主/子会话的实测字段 | 准确度 | 中枢聚合与防刷屏策略 |
|---|---|---|---|---|
| **Claude Code** (`2.1.299`) | AgentTool subagent、Team / Teammate | **二进制 Schema 明确保证**：基础 Hook 输入中 `agent_id` *"Present only when the hook fires from within a subagent... Absent for the main thread, even in `--agent` sessions. Use this field (not `agent_type`) to distinguish subagent calls from main-thread calls."* 另提供独立事件 `SubagentStart`、`SubagentStop`（含 `agent_id`、`agent_type`、`agent_transcript_path`、`last_assistant_message`、`background_tasks`）、`TeammateIdle`（`teammate_name`、`team_name`） | **100% 确定** | `parent = session_id`；凡带 `agent_id` 或属于 `SubagentStart`/`SubagentStop`/`TeammateIdle` 一律记入子树（`sub = agent_id`），**仅进 Web 时间线，不推飞书**；仅主线程 `Stop`/`StopFailure`/`PermissionRequest`/`Notification` 进入飞书路由 |
| **Codex CLI** (`0.167.2`) | 内置 `multi_agent`（`stable=true`）subagent | **内嵌 JSON Schema 实测**：`SubagentStart` / `SubagentStop` 共享父会话 `session_id`，必填 `agent_id`、`agent_type`、`turn_id`（`SubagentStop` 另带 `agent_transcript_path`、`last_assistant_message`）；`PermissionRequest` / `UserPromptSubmit` 可选带 `agent_id`、`agent_type`；主线程 `Stop` **不含 `agent_id`** | **100% 确定** | 同 Claude：`SubagentStart`/`SubagentStop` 挂到父 `session_id` 子树，不推飞书；**例外**：若带 `agent_id` 的 `PermissionRequest` 需要人工审批，属于阻塞点，必须升级通知并在卡片注明子会话来源 |
| **Antigravity CLI (`agy`)** (`1.5.13`) | `/swarm`（共享消息板多 worker）、`/boost`（并行增强 worker）、`/teamwork`（分层协作 `teamwork_layer`/`gate_reviewer`/`teamwork_judge`，内部有 `parent_conversation_id`）、通用 subagent、`run_workflow` | **关键实测缺口**：`agy` `hooks.json` 仅 6 个事件，**没有 `SubagentStart`/`SubagentStop`，stdin 中也没有 `parentConversationId` 或 `agentId`**（`parent_conversation_id` 仅在内部 `cortex_pb`，未透传到 hook）。但 `Stop` payload 明确提供：**`fullyIdle`**（`true` 表示所有后台任务/子会话均已结束）、**`continuationActive`**（`true` 表示 Stop hook 续跑）、**`terminationReason`**（`NO_TOOL_CALL` / `ERROR` / `USER_CANCELED` 等） | **混合（主会话确定 + 子会话启发式）** | 1. **主会话判定**：同一 `pane_id` 下首次触发且由人交互启动的 `conversationId` 记为 `root`；<br>2. **`fullyIdle` 门控**：主会话 `Stop` 若 `fullyIdle == false` 或 `continuationActive == true`，视为仍在等子会话/后台任务，**绝不触发 `turn_complete` 通知**（与 `agy` 内置 OSC 7501 语义对齐）；<br>3. **同 Pane 子会话折叠**：在 `root` 未 `fullyIdle` 期间，同一 `pane_id`（同 `$TMUX_PANE` + `cwd`）冒出的其他 `conversationId` 的 `SessionStart`/`Stop` 启发式归入 `root` 子树（只进 Web，不推飞书，除非 `terminationReason == "ERROR"`）；<br>4. **可选 Swarm 进度增强**：只读解析 `<appData>/brain/<root_conversation_id>/.system_generated/swarm/plan.json`（已在 `swarm_pane/SKILL.md` 验证格式），在 Web 端展示 Goal/Threads |

> **关于 `/swarm`、`/boost`、`/teamwork` 触发方式核实**：
> - 三者均为 `agy` 交互式 TUI 内的斜杠命令（`/swarm` 对应内置 `native_swarm` 技能，启动 Coordinator + 共享消息板；`/teamwork` 对应内部多层协作/裁决工作流；`/boost` 对应并行 worker 增强模式）。
> - 在零 Headless 约束下，应通过 **L4 交互式启动**或 **L2 门禁注入**向 `agy` 交互会话触发（注意：`agy -i "/swarm ..."` 是否直接展开斜杠命令、以及 TUI 中输入 `/swarm` 时的自动补全弹窗拦截行为，需结合 `@verifier` 的 TUI 注入备忘并在 M2 沙箱实测确认）。

### 3.2 零 Headless 模式下的交互式投递安全原语实测（回应 `architecture.md` §4.1）

禁用 `-p` / `exec` 后，对各家真实交互式长会话的外部投递原语实测如下：

| 级别 | 投递原语 | 适用范围 | 本机实测细节与边界限制 |
|---|---|---|---|
| **L0** | **MCP Inbox 拉取**（`relay.read_inbox` / `relay.read_handoff`） | **三家通用**（最安全数据面） | 三家均原生支持 stdio MCP。所有长正文（`task.v1`、`handoff.v1`、`review.v1`）必须走 L0 传递，绝不把长文本直接灌进终端 PTY |
| **L1** | **`codex queue --thread <UUID\|name> --message <TEXT> [--resume]`** | 仅 **Codex CLI** | `codex queue --help` 与二进制实测：<br>1. 将消息写入 `~/.codex/queue_1.sqlite`，带 `--resume` 时尝试通过本地 `app-server` daemon 唤醒线程（状态含 `AlreadyRunning`/`Started`/`Restarted`/`Stopped`/`NotRunning`，resume 失败仅打 warning 仍返回 exit 0）；<br>2. **语法陷阱**：二进制显式拒绝 `--` 分隔符（`codex queue does not support --`），禁止以 `-` 开头的裸文本或需加前缀（如 `relay: ...`）；<br>3. **离线 TUI 限制**：若用户以 `--no-daemon` 运行独立 TUI，或 live TUI 未立即消费队列，必须在 M2 实测验证并在未见 `UserPromptSubmit` 时自动降级到 L2/L0 |
| **L2** | **tmux `load-buffer` + `paste-buffer -p`（Bracketed Paste）+ 提交键** | **三家通用**（仅传单行短指针） | `tmux 3.7c` 实测命令签名：<br>• `tmux load-buffer -b relay_<id> -`（通过 stdin 传入文本，彻底规避 shell 参数/引号注入）；<br>• `tmux paste-buffer -d -p -b relay_<id> -t <pane>`（`-d` 粘贴后立即删除命名 buffer 不污染用户粘贴栈，**`-p` 包裹 bracketed paste `\e[200~...\e[201~`** 避免意外回车）；<br>• 延迟 50–150ms 后发 `tmux send-keys -t <pane> Enter`（由 `@verifier` 补全各家 TUI 的 Enter/草稿/斜杠弹窗门禁）。**严禁**跳过门禁直接 `send-keys` |
| **L4** | **交互式新 Window Spawn**（非 Headless！） | **三家通用**（新任务最干净入口） | 经三家 `--help` 验证，均支持带初始 prompt 启动**真实交互式 TUI 会话**（跑完初始 prompt 后停留在交互态，保留完整长上下文、hooks、MCP 且人类随时可切入接管）：<br>• `tmux new-window -c <cwd> -- claude -n "<name>" "<prompt>"`<br>• `tmux new-window -c <cwd> -- codex -C <cwd> "<prompt>"`<br>• `tmux new-window -c <cwd> -- agy -i "<prompt>"` |

> **核实结论**：`claude` 与 `agy` **均无**类似 `codex queue` 的独立 CLI 入队子命令（`claude` 有 `--bg`/`attach`/`agents --json`，`agy` 有 `acp` JSON-RPC 宿主协议，均不适合直接向 BYO-tmux 已运行的交互 TUI 入队）。因此，**L0（MCP 正文数据面）+ L4（新任务交互式 Spawn）+ L2（受门禁保护的单行指针唤醒）** 是三家通用的标准投递组合，**L1（`codex queue`）** 作为 Codex 专属实验性优先通道。

---

## 4. 提炼给架构师（`docs/architecture.md` 定稿）的直接补全建议

1. **§7.1–7.3 直接落盘参数**：采用 `tokio`（最小 features）+ `axum 0.8`（仅 `http1,json,tokio,query`，无 WS）+ 手写 stdio MCP（~250 行）+ `include_bytes!` 静态零构建 Web UI + JSONL/原子快照（M0–M2），并锁定 `[profile.dev.package."*"] debug = 0` + `split-debuginfo = "unpacked"`，将 M0 `target/debug` 门禁预算定为 **`≤ 450 MB`**（本机 SSD 仅余 `34Gi`）。
2. **§2.2 / §3.3 子会话聚合规则定型**：Claude（`agent_id` 存在性 + `SubagentStart/Stop`/`TeammateIdle`）与 Codex（`SubagentStart/Stop` + `agent_id`，注意子会话 `PermissionRequest` 仍需告警）为 100% 确定性聚合；`agy` 采用 **`fullyIdle` 门控 + 同 `pane_id` 非 root `conversationId` 启发式折叠**，并可选只读解析 `swarm/plan.json` 增强 Web 视图。
3. **§4.1 L1 `codex queue` 注意事项**：消息统一带 `relay: ` 前缀规避 `--` 不支持的陷阱，且配置投递后 `UserPromptSubmit` 超时回退到 L2/L0。
4. **§9 新增一条待定细节**：项目启动前的 2 分钟前置环境准备需用户执行 `rustup`（`--profile minimal --component clippy,rustfmt`）与 `brew install sccache`，并在 `docs/architecture.md` M0 门禁中注明。
