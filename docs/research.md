# 本地平台能力与开源生态调研报告

> 调研范围：本机已安装 CLI/工具集成面实测 + 同类开源项目横向对比 + 面向架构设计的关键约束与机会。
> 所有本地 CLI 标志、Hook 事件、Schema 与配置现状均经本机只读命令实测验证（未修改任何用户配置）。

## 本机环境基线

| 组件 | 实测版本 | 路径 |
|---|---|---|
| Claude Code | `2.1.299` | `~/.local/bin/claude` → `~/.local/share/claude/versions/2.1.299` |
| Codex CLI | `0.167.2` | `~/.nvm/versions/node/v22.23.1/bin/codex`（原生 ARM64 二进制） |
| Antigravity CLI | `1.5.13` | `~/.local/bin/agy` |
| Lark CLI | `1.0.97` | `~/.nvm/versions/node/v22.23.1/bin/lark-cli` |
| tmux | `3.7c` | `/opt/homebrew/bin/tmux` |

---

## 1. 本地 CLI 与平台集成面实测

### 1.1 三大 Coding Agent 核心能力总览（直接回应架构师 #5 问题）

| 维度 | `claude` (2.1.299) | `codex` (0.167.2) | `agy` (1.5.13) |
|---|---|---|---|
| **非交互/Headless 模式** | `-p/--print`，支持 `--output-format text\|json\|stream-json`、`--input-format text\|stream-json`、`--json-schema`、`--include-hook-events` | `codex exec`（别名 `e`），支持 `--json`（JSONL）、`-o/--output-last-message`、`--output-schema`、`codex review` | `-p/--print/--prompt`，支持 `--output-format text\|json\|stream-json`、`--input-format text\|stream-json`（多轮 NDJSON）、`--json-schema`、`--print-timeout` |
| **会话恢复与后台管理** | `--session-id <uuid>`、`-r/--resume`、`-c/--continue`、`--fork-session`、`--bg` + `agents --json`/`attach`/`logs`/`stop` | `resume [id\|--last]`、`exec resume`、`fork`、**`queue --thread <id> --message <text> [--resume]`**、`agents` | `-c/--continue`、`--conversation <id>`、`--project`、`--new-project`、`agy acp`（ACP stdio JSON-RPC） |
| **独立「等待审批」Hook** | **有（双重覆盖）**：`PermissionRequest` + `Notification`（`notification_type: permission_prompt \| idle_prompt \| elicitation_dialog \| auth_success`） | **有**：`hooks.json` 支持 `PermissionRequest`（注意：**无** `Notification` hook；传统 `notify` 另见下节） | **无**：`hooks.json` 仅 6 个事件，**没有** `PermissionRequest` 或 `Notification`（需用 `PreToolUse` / 抓屏 / 空闲超时兜底） |
| **独立「回合结束/出错」Hook** | `Stop`、`StopFailure`、`SubagentStop`、`SessionEnd` | `Stop`、`SubagentStop`、`Interrupt`、`SessionEnd` | `Stop`（含 `terminationReason`：`NO_TOOL_CALL`/`ERROR`/`USER_CANCELED` 等，含 `fullyIdle`） |
| **Hook Stdin 关键字段** | **snake_case**：`session_id`、`transcript_path`、`cwd`、`permission_mode`、`hook_event_name`；`Stop` 另带 `last_assistant_message`、`stop_hook_active`、`background_tasks` | **snake_case**（内嵌 JSON Schema 实测）：`session_id`、`transcript_path`（可为 null）、`cwd`、`permission_mode`、`model`、`hook_event_name`、`turn_id`；`Stop` 另带 `last_assistant_message`、`stop_hook_active`；`PermissionRequest` 另带 `tool_name`、`tool_input` | **camelCase**（protojson）：`conversationId`、`transcriptPath`、`cwd`、`workspacePaths`、`artifactDirectoryPath`、`modelName`、`lastUserInput`、`hookEventName`；`Stop` 另带 `executionNum`、`terminationReason`、`error`、`fullyIdle`、`continuationActive` |
| **Hook 执行模型与超时** | 支持 `async: true` / `asyncRewake: true`（不阻塞回合）；支持 `command`/HTTP/prompt/MCP | 支持 `async` 字段与 `timeout`；受 **SHA-256 信任哈希**机制约束（见 1.3） | **仅同步 `command`**（`sh -c`，默认超时 30s，阻塞主循环）；hook 进程 CWD 为 `hooks.json` 所在目录而非项目 `cwd`；**必须向 stdout 输出合法 JSON（如 `{}`）** |
| **Hook 继承 `$TMUX_PANE`** | 交互式 pane 内启动时默认继承环境；`--bg` / 远程会话无当前 pane | 直接进程默认继承，但 `daemon_auto_start=true`（默认开启）下由共享 `app-server` 守护进程触发的 hook **可能缺失或指向 daemon 启动时的旧 pane** | 本机实测子进程继承 `$TMUX`；但在 `--remote-agent` / 共享 LS 场景下不可单独依赖 |
| **MCP 能力** | 客户端（`stdio`/`http`/`sse`/`ws`，`claude mcp add` / `--mcp-config`）+ 内置服务端 `claude mcp serve` | 客户端（`stdio` + Streamable HTTP `--url`，`codex mcp add` 写入 `~/.codex/config.toml`）；0.167.2 **无** `mcp-server` 子命令（程序化服务为 `codex app-server`） | 客户端（`stdio` + **Streamable HTTP**，不支持旧版 HTTP+SSE；`agy mcp add` 写入 `~/.gemini/config/mcp_config.json`） |

---

### 1.2 Claude Code (`claude 2.1.299`) 实测细节

- **非交互与结构化集成**：
  - `claude -p "<prompt>" --output-format stream-json --input-format stream-json` 支持双向流式 NDJSON；`--include-hook-events` 可将 hook 生命周期直接打进事件流。
  - `--settings <file-or-json>` 与 `--mcp-config <configs...>` 支持**会话级临时注入**，不污染用户全局配置；`--bare` 可跳过插件/hooks/CLAUDE.md 做纯净自动化调用。
  - 权限控制支持 `--permission-mode acceptEdits|auto|bypassPermissions|manual|dontAsk|plan` 与 `--permission-prompts host|none`、`--max-budget-usd`。
- **后台会话原生能力**：
  - `--bg/--background` 启动后台会话并返回短 ID，`claude agents --json [--all] [--cwd <path>]` 可无 TTY 列出所有活跃/后台会话，`claude attach/logs/stop/rm` 管理生命周期。
- **Hook 完整事件集（从本地二进制验证）**：
  - `PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`PostToolBatch`、`Notification`、`UserPromptSubmit`、`UserPromptExpansion`、`SessionStart`、`SessionEnd`、`Stop`、`StopFailure`、`SubagentStart`、`SubagentStop`、`PreCompact`、`PostCompact`、`PreModelSwitch`、`PostModelSwitch`、`PermissionRequest`、`PermissionDenied`、`Setup`、`TeammateIdle`、`TaskCreated`、`TaskCompleted`、`Elicitation`、`ElicitationResult`、`ConfigChange`、`WorktreeCreate`、`WorktreeRemove`、`CwdChanged`。
  - `Notification` 事件的 matcher 匹配 `notification_type`（实测取值：`permission_prompt`、`idle_prompt`、`auth_success`、`elicitation_dialog`）。
  - `Stop` / `SubagentStop` 均提供 `last_assistant_message` 字段，**无需解析整份 transcript 文件**即可在通知中展示最后回复摘要。
- **本机现有配置现状（只读检查）**：
  - `~/.claude/settings.json` 已存在 `hooks.PostToolUse`（`Edit|MultiEdit|Write` → `~/.claude/hooks/fix-after-edit.sh`）、`statusLine` 与 `agentPushNotifEnabled: true`。安装器合并 hook 时**必须做幂等追加与标记**（而非覆写整个 `hooks` 对象）。

---

### 1.3 Codex CLI (`codex-cli 0.167.2`) 实测细节

- **非交互与程序化接口**：
  - `codex exec`（别名 `e`）支持 `--json`（JSONL 事件流）、`-o <file>` 输出最终回复、`--output-schema <file>`、`-s read-only|workspace-write|danger-full-access`、`--approve-for-me`、`--ephemeral`、`--worktree`。
  - **跨会话投递原语**：`codex queue --thread <UUID|name> --message <TEXT> [--resume]` 可直接向已有会话排队投递消息，无需依赖脆弱的 `tmux send-keys`。
  - **App Server**：`codex app-server --listen stdio://|unix://PATH|ws://IP:PORT`（实验性，支持 `generate-ts` 与 `generate-json-schema`），且 `codex features list` 显示 `daemon_auto_start = stable true`（可通过 `--no-daemon` 禁用）。
- **`hooks.json` 事件集与内嵌 Schema（从本地原生二进制提取）**：
  - `codex features list` 确认 `hooks` 特性已是 **`stable (true)`**。
  - 二进制内嵌的 12 个 command hook 事件：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、`PreCompact`、`PostCompact`、`SubagentStart`、`SubagentStop`、`Stop`、`Interrupt`（另支持 TOML 内联 `[hooks]` 配置）。
  - 实测 `stop.command.input` 必填字段：`cwd`、`hook_event_name`、`last_assistant_message`（可空）、`model`、`permission_mode`、`session_id`、`stop_hook_active`、`transcript_path`（可空）、`turn_id`。
  - 实测 `permission-request.command.input` 必填字段：`cwd`、`hook_event_name`、`model`、`permission_mode`、`session_id`、`tool_name`、`tool_input`、`transcript_path`（可空）、`turn_id`（可选 `agent_id`、`agent_type`）。
- **`hooks.json` 与 `config.toml notify` 共存实测**：
  - **结论：完全可以共存，且本机当前就处于共存状态。**
  - 本机 `~/.codex/config.toml` 已配置 `notify = ["~/.codex/computer-use/.../SkyComputerUseClient", "turn-ended"]`（二进制内部命名为 `legacy_notify`，仅支持单条命令数组）；同时本机 `~/.codex/hooks.json` 已配置 `PostToolUse` hook，二者走独立调度路径，互不覆盖。
  - **重要约束 1**：严禁修改 `config.toml` 的 `notify` 字段（会破坏 Computer Use）。项目应只接入 `hooks.json`（`Stop`、`PermissionRequest`、`SessionStart` 等）。
  - **重要约束 2（Hook Trust Hash）**：本机 `config.toml` 存在 `[hooks.state."~/.codex/hooks.json:post_tool_use:0:0"] trusted_hash = "sha256:..."`。Codex 对新增/变更的 `hooks.json` 条目要求用户信任确认（或在自动化场景使用 `--dangerously-bypass-hook-trust`）。因此项目的 `init` / `doctor` 流程必须提示并检测 Codex hook 信任状态。

---

### 1.4 Antigravity CLI (`agy 1.5.13`) 实测细节

- **非交互与协议支持**：
  - `agy -p "<prompt>" --output-format stream-json --input-format stream-json` 支持通过 stdin 逐行发送 NDJSON 进行多轮非交互对话；`--settings <json-or-file>` 支持会话级临时覆盖配置而不落盘。
  - `agy acp` 提供标准 **Agent Client Protocol (ACP)** stdio JSON-RPC 服务（支持 `--process-per-session` 按会话进程隔离），适合作为后期结构化集成通道。
- **Hooks 实测（依据内置文档 `agy-customizations/docs/hooks.md` + 二进制 `HookRequest` proto）**：
  - 配置文件位置：全局 `~/.gemini/config/hooks.json` 或项目级 `.agents/hooks.json`（向上遍历至 git 根目录发现）。本机 `~/.gemini/config/` 下当前**尚无** `hooks.json`。
  - **顶层格式与其他两家不同**：以**命名 hook 组**为顶层 key（可独立 `"enabled": false`），而非 `{"hooks": {...}}`：
    - `SessionStart`、`PreInvocation`、`PostInvocation`、`Stop` 为**扁平 handler 数组**（无 `matcher` 包装）。
    - `PreToolUse`、`PostToolUse` 为**分组数组**（`{matcher, hooks}`）。
  - **关键缺口**：二进制 `HookRequest` proto 仅包含 `preInvocation`、`postInvocation`、`stop`、`preTool`、`postTool`（加 `SessionStart`），**不存在 `PermissionRequest` 或 `Notification` hook**。
  - **同步阻塞与契约要求**：仅支持 `type: "command"`，通过 `sh -c` **同步阻塞**执行（默认超时 30s），进程工作目录为 `hooks.json` 所在目录（需读取 stdin 中的 `cwd` 字段获取项目路径），且必须向 stdout 返回合法 JSON（如 `{}`）。
- **MCP 与原生 Swarm / Subagents**：
  - `~/.gemini/config/mcp_config.json`（当前为空文件）支持 `stdio` 与 **Streamable HTTP**（明确不支持旧版 2024-11-05 HTTP+SSE）。
  - `agy` 内置 `swarm`（共享消息板 `post_message`/`read_board`/`swarm_wait` + 独立 workspace）与 `run_workflow`（Python 编排脚本），但仅限 `agy` 自身运行时内部，无法直接纳管外部 tmux 中的 `claude`/`codex` 会话。

---

### 1.5 飞书 / `lark-cli 1.0.97` 实测细节

- **本机认证与就绪状态**：
  - `lark-cli auth status` 显示 `brand: feishu`、`identities.bot.status = "ready"`（bot 身份已就绪，可直接 `--as bot` 发消息与消费事件）；`user` 身份亦已配置（自动刷新）。
- **单向通知能力（Outbound）**：
  - `lark-cli im +messages-send --as bot (--chat-id oc_xxx | --user-id ou_xxx) (--text | --markdown | --content <json> --msg-type interactive) [--idempotency-key <key>]`。
  - 支持幂等键（最长 50 字符，天然防止重试重复推送）、Markdown 自动转富文本、**交互式卡片（`--msg-type interactive`）**，以及 `+messages-reply --message-id om_xxx [--reply-in-thread]` 话题回复与 `+messages-edit`（仅 bot）原地更新状态。
- **双向事件消费能力（Inbound — 回答架构师问题 #4）**：
  - **结论：支持长连接实时接收交互卡片回调与 IM 回复，无需公网 Webhook 服务器。**
  - `lark-cli event consume <EventKey> --as bot` 通过本地事件总线守护进程（UDS IPC，所有 consumer 共享单一 bus daemon，当前 `event status` 显示 `Bus: not running`，首次 consume 自动拉起）将事件以 **NDJSON** 流式输出到 stdout。
  - 关键可用 EventKey（`lark-cli event list` & `event schema` 实测）：
    1. **`card.action.trigger`**（`auth: bot`，需 scope `im:message:readonly` + 开发者后台开启事件）：接收卡片按钮/下拉/表单回调，输出包含 `action_tag`、`action_name`、`action_value`、`form_value`、`input_value`、`operator_id`、`chat_id`、`message_id`、`card_content` 及 **`token`（30 分钟内有效、最多 2 次延迟更新卡片**，配合 `lark-cli api POST /open-apis/interactive/v1/card/update` 回写「已批准/已拒绝」状态）。
    2. **`im.message.receive_v1`**（`auth: bot`，需 scope `im:message.p2p_msg:readonly`）：接收用户给 bot 发的消息/话题回复（含 `chat_id`、`chat_type`、`content`、`message_id`、`mentions`），可用于远程向指定 agent 追发指令。
  - **子进程集成契约（来自 `lark-cli skills read lark-event`）**：
    - 一个 `consume` 进程只订阅一个 `EventKey`（多事件开多个子进程，共享同一个 UDS bus daemon，开销极小）。
    - stderr 输出 `[event] ready event_key=<key>` 作为就绪信号（切勿传 `--quiet`，否则丢失就绪与丢包诊断）。
    - **stdin EOF 陷阱**：无界运行（未设 `--max-events`/`--timeout`）时，关闭 stdin（如 `< /dev/null` 或 daemon 默认 null stdin）会触发优雅退出（exit 0, `reason: signal`），父进程必须保持 stdin pipe 打开。
    - 禁止 `kill -9`（应使用 SIGTERM 或关闭 stdin 优雅退出，避免部分事件泄漏服务端订阅）。

---

### 1.6 `tmux 3.7c` 实测细节与真实环境发现

本机实时 `tmux list-sessions` / `list-panes` / `list-clients` 暴露了几个极具价值的真实场景细节：

1. **真实多 Agent 负载画像**：本机同时运行 5 个 session（`learning`、`prome`、`teleop` 已 attach；`sparc`、`spot` detached），共 25+ 个 pane，混跑 `codex`、`claude`、`agy` 与 `zsh`。
2. **关键踩坑 —— `#{pane_current_command}` 无法直接识别 Claude Code**：
   - 实测 `tmux list-panes` 输出中，所有 Claude Code pane 的 `cmd=` 均为 **`2.1.299`**（因为 `~/.local/bin/claude` 是指向 `~/.local/share/claude/versions/2.1.299` 的软链接，macOS `comm` 取的是版本号文件名！），而 `codex` 显示 `codex`，`agy` 显示 `agy`。
   - **启示**：绝不能只匹配 `pane_current_command == "claude"`，必须结合 hook 自注册、`ps -o args= -p <pane_pid>` 进程命令行或子进程树来准确识别 agent 类型。
3. **焦点与防打扰检测的真实陷阱**：
   - `list-panes` 可查 `#{session_attached}`、`#{window_active}`、`#{pane_active}`、`#{pane_in_mode}`（实测 `learning:5.0` 的 `mode=1`，正处于 copy-mode！）。
   - **关键发现**：本机 `tmux list-clients` 中三个已 attach 的客户端（`prome`、`teleop`、`learning`）**同时显示 `flags=attached,focused,UTF-8`**（因为多终端窗口/未开启 `focus-events` 时 tmux 无法区分 OS 前台窗口），但 `#{client_activity}`（最后一次按键的 Unix 时间戳）明显不同（彼此相差数十秒到数十分钟）。
   - **启示**：防打扰判定不能仅靠 `focused` 标志，应组合：`session_attached == 1 && window_active == 1 && pane_active == 1` + `client_activity` 距今活跃窗口（如 60–120s 内），必要时在 macOS 可选结合系统空闲时间 / 前台终端窗口检查。
4. **交互与状态原语**：
   - 状态检测：`window_bell_flag`、`window_activity_flag`、`window_silence_flag`、`set-hook`、pane 自定义用户变量（`set-option -p @agent_state`）供 status-bar 展示；若透传 OSC 通知需开启 `allow-passthrough on`。
   - 内容读取与注入：`capture-pane -p -S -N -t %id`（抓屏兜底，需容忍全屏 TUI 与 ANSI 转义）；`send-keys -l -t %id` / `load-buffer` + `paste-buffer`（注入前**必须检查 `#{pane_in_mode}`**，若处于 copy-mode 需先退出或改走异步收件箱，避免把命令按键打进滚动模式）；`switch-client` / `select-window` / `select-pane` 可实现点击通知跳回目标 pane。

---

## 2. 开源生态全景调研（7 个代表性项目）

调研覆盖四个细分流派：**tmux 会话 TUI 管理器**、**事件/通知监控守护进程**、**MCP 跨 Agent 协作桥**、**重型多 Agent 编排器**。

| 项目 | 形态与技术栈 | 核心解决的问题 | 突出优势 | 关键短板 / 缺口 |
|---|---|---|---|---|
| **[asheshgoplani/agent-deck](https://github.com/asheshgoplani/agent-deck)** | Go + Bubble Tea + tmux TUI 控制台 | 在单一 TUI 中管理多个 Claude/Codex/Gemini/OpenCode 会话与 git worktree | 单二进制分发、状态一目了然（running/waiting/idle）、支持会话 fork 与 MCP 集中管理 | 偏「人管多 agent」，缺乏结构化跨 agent 上下文交接协议与外部 IM（飞书）双向通知 |
| **[njbrake/agent-of-empires](https://github.com/njbrake/agent-of-empires)** (AoE) | Rust + tmux + Web Dashboard | 多 agent 并行开发、git worktree 隔离、可选 Docker 沙箱与移动端 Web 查看 | 隔离完整、兼顾终端 TUI 与手机 Web 查看 | 强接管会话生命周期，迁移成本高；不解决异构 agent 间的消息/交接协作 |
| **[epilande/ccmux](https://github.com/epilande/ccmux)** | 后台监控 Daemon + SSE + TUI + tmux | 解决后台多 pane 状态盲区（`idle`/`working`/`waiting`） | 精准识别审批/计划等待状态；桌面通知直接带 Approve/Deny 操作按钮；支持 Claude/Codex/Cursor/OpenCode | 聚焦本地桌面通知与单机视角，缺少跨 agent 交接协议与可插拔 IM 通道 |
| **[Dicklesworthstone/mcp_agent_mail_rust](https://github.com/Dicklesworthstone/mcp_agent_mail_rust)** (`mcp-agent-mail`) | Rust MCP Server + SQLite + Git 审计 | 异构 agent 间点对点通信、线索追踪与并发改文件防冲突 | 完全基于 MCP、与具体 CLI 解耦；提供收件箱、检索线程与**协作式文件租约锁（advisory file reservation）** | 纯 MCP 拉取模式**无法感知卡在审批的 agent（痛点 P2）**，也无法主动唤醒空闲 pane；45+ 工具对上下文开销偏重 |
| **[gastownhall/gastown](https://github.com/gastownhall/gastown)** (Gas Town) | Go + tmux + Beads (Git issue 数据面) | 长周期大规模多 agent 委派与上下文耗尽恢复（Mayor + Polecats 架构） | 工业级持久化、任务可跨崩溃/换会话延续 | 架构和概念体系极重、强绑定特定工作流，不适合只想保留自己日常 tmux 布局的开发者 |
| **[raysonmeng/agent-bridge](https://github.com/raysonmeng/agent-bridge)** / **[zazencodes/tmux-agent-plane](https://github.com/zazencodes/tmux-agent-plane)** | 本地桥接 Daemon / tmux 状态栏 + CLI/Skill 协调面 | 直接打通 Claude Code ↔ Codex 双向消息中继与 tmux 内 agent 互相列表/发信/等待 | 直击异构 CLI 互相协作与去复制粘贴痛点，贴近 tmux 原生习惯 | 生态尚早期，缺少统一通知路由/防打扰机制、审批感知与标准化插件边界 |
| **[peon-ping](https://github.com/PeonPing/peon-ping)** / **[ddzero2c/claude-tmux-notifier](https://github.com/ddzero2c/claude-tmux-notifier)** / **[kbwo/ccmanager](https://github.com/kbwo/ccmanager)** | Hook 脚本/插件 + 音频/系统通知；TypeScript Ink TUI | 解决「什么时候该看回终端」与点击通知定位到具体 tmux pane | **采用门槛极低**（即插即用、零工作流劫持）、PID 树反查 tmux pane 体验好 | 仅覆盖单一痛点（纯通知或纯 worktree 管理），不涉及跨 agent 协作 |

### 什么让这类开源项目真正被广泛采用？（关键规律）

1. **「自带 tmux 会话（Bring Your Own tmux）」胜过「接管整个工作流」**：像 `peon-ping`、`claude-tmux-notifier`、`ccmux` 容易获得自然增长，而要求重写全套工作方式的编排器（如 Gas Town）只吸引极少数重度玩家。
2. **非侵入与可逆安装**：开发者对 `~/.claude/settings.json`、`~/.codex/config.toml`、`~/.gemini/config/` 高度敏感；提供幂等 `init` / `doctor` / `uninstall` 且绝不破坏现有配置（如 `notify`、`PostToolUse`）是基本信任门槛。
3. **极简 MCP 工具面**：相比 45+ 个工具导致 prompt 膨胀，把跨 agent 协作收敛到 5–8 个高内聚工具（如 `list_agents`、`send_message`、`read_inbox`、`create_handoff`、`read_handoff`、`reserve_files`）更易被 Claude/Codex/agy 稳定调用。
4. **低门槛分发 + 清晰的插件扩展点**：单二进制或一条命令安装，零强制云端依赖；同时在核心层抽象出 `Agent Adapter` 与 `Notifier Adapter`，社区才能轻松贡献 OpenCode / Gemini CLI / Slack / Telegram / 飞书 / macOS 通知插件。

---

## 3. 提炼给架构师的关键技术约束与机会

1. **三家 CLI 的「等待审批」能力不对称 → 状态采集必须「Hook 优先 + 轻量兜底」**：
   - `claude`（`PermissionRequest` + `Notification:permission_prompt`）与 `codex`（`PermissionRequest`）均能通过 Hook **精确、实时**上报审批阻塞。
   - **`agy` 没有审批/通知类 Hook**（仅 6 个生命周期事件）。因此若要让 `agy` 同样具备「卡在审批」提醒，必须为 `agy` Adapter 设计兜底机制：例如 `PreToolUse` 触发后启动计时器（若 N 秒内未收到对应 `PostToolUse` 且 `capture-pane` 出现审批提示特征，则转入 `needs_permission`），或在远期利用 `agy acp` 协议原生接收权限请求。
2. **三家 Hook 运行时差异显著 → Hook 采集端 CLI（`emit`）必须极快且协议适配**：
   - `agy` 仅支持**同步阻塞**命令 hook、CWD 不是项目目录、stdin 为 **camelCase**、且要求 stdout 输出 `{}`；而 `claude`/`codex` 支持异步与 **snake_case**。
   - **设计要求**：所有 hook 统一调用一个轻量短命命令（如 `<bin> hook emit --agent <claude|codex|agy>`），在 **<30ms** 内完成 stdin 解析、归一化、发往本地 daemon（UDS/HTTP）并立即返回 `{}`（退出码 0），绝不阻塞 agent 主循环，也不因 daemon 未启动而导致 agent 报错。
3. **Codex `notify` 禁区与 Trust Hash 引导**：
   - 本机 `~/.codex/config.toml` 的 `notify` 已被 Computer Use 占用，但实测与 `~/.codex/hooks.json` 完全共存 → **只写 `hooks.json`，绝不碰 `config.toml` 的 `notify`**。
   - 因 Codex 校验 `hooks.json` 的 SHA-256 信任哈希，`<bin> doctor` 应能检测 `[hooks.state]` 缺失并引导用户在 Codex 中完成一次授权信任。
4. **会话 ↔ tmux Pane 映射的健壮性**：
   - 不能单靠 `$TMUX_PANE`（Codex 共享 `app-server` 守护进程或后台/远程会话可能缺失或错位），也不能单靠 `#{pane_current_command}`（Claude Code 显示为版本号 `2.1.299`）。
   - **推荐策略**：三层关联——优先使用 hook 进程继承的 `$TMUX_PANE`；缺失时按 `pid`/命令行 + `cwd` + `session_id` 反查 `tmux list-panes`；并记录 `pane_id ↔ session_id ↔ agent_type ↔ cwd` 注册表。
5. **跨 Agent 交互（痛点 P1）：MCP 拉取为主 + 安全投递为辅**：
   - 三家 CLI **全部原生支持 stdio 与 Streamable HTTP MCP**，这是跨异构 agent 共享上下文交接包（Handoff Artifact：目标、结论、变更文件、git diff 引用、待办）最稳健、无终端解析脆弱性的公共最大公约数。
   - 如需「推送唤醒」另一个正在 tmux 中空闲的 agent，可按适配器分级：`codex` 优先探索原生 `codex queue --thread`；通用路径使用 `tmux` 注入前强制检查 `#{pane_in_mode}` 与空闲状态，避免打断正在执行的回合或把文本打进 copy-mode。
6. **飞书双向审批的低成本机会**：
   - `lark-cli 1.0.97` 已自带本地长连接事件总线与 `card.action.trigger` / `im.message.receive_v1` 消费能力，且本机 bot 已就绪。
   - 这意味着 **Phase 1** 即可通过 `lark-cli im +messages-send --as bot` 实现高颜值飞书卡片/Markdown 通知（带幂等键防重），**Phase 2** 无需公网域名/服务器，仅需守护进程托管 `lark-cli event consume card.action.trigger --as bot`（注意保持 stdin 打开 + 监听 stderr ready 标记 + SIGTERM 优雅退出）即可实现手机端飞书卡片一键回传状态或向指定 tmux pane 投递指令。
