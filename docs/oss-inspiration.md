# 开源 Coding Agent 控制台与多 Agent 编排生态调研（Baton 借鉴指南）

> 调研对象：GitHub 与 X（Twitter）上具有代表性的 8 款开源 Coding Agent 控制台、tmux 多 Agent 编排器与移动端远程开发工具（含 1 款配套可观测性工具 `ccusage`）。
> 调研目标：在不破坏 `baton` 六条硬约束（**零 Headless、BYO-tmux、只用官方扩展面、非侵入可逆、Rust 单二进制 ≤6MB + 零 `node_modules` 前端 ≤200KB、G1/G2/G3 证据闭环**）的前提下，提炼下一迭代可直接落地的工程与交互实践。

---

## 1. 代表性开源项目横向速览

| 项目 | 仓库 / 形态 | 核心定位 | 最亮眼的 1–2 个设计 |
|---|---|---|---|
| **Vibe Kanban** | `BloopAI/vibe-kanban`<br>Web 看板 + 本地后台 | 把多个 CLI Agent 当作异步并行开发者调度（Backlog → In Progress → In Review → Done） | ① **行内 Diff 评论批量回传**：在 Diff 任意行点 `+` 批注，跨文件攒齐后一键打包为单条结构化反馈打回 Agent；<br>② **任务级 Worktree 自动生命周期**：自动执行 setup 脚本、隔离跑测、合并后清理。 |
| **Superset（现 Paradev）** | `superset-sh/superset`<br>桌面工作区 + 终端 Hub | 并行 CLI Agent 工作区与集成代码审查台 | ① **高密度 Diff 查看器**：支持 Unified/Split、单文件「已阅（Viewed）」勾选、折叠未改区块；<br>② **PR Review 评论直通 Agent**：选中 Diff 行直接把审阅意见派发回运行中的 Agent 会话。 |
| **Crystal（现 Nimbalyst）** | `stravu/crystal`<br>跨平台桌面工作区（IVE） | 多会话 Claude Code / Codex 图形化编排 | ① **Artifact 一等公民**：把 Markdown 计划、图表与会话看板放在同一工作区；<br>② **多会话并行 Diff 对比**：同一任务可开多个工作树横向对照不同模型方案。 |
| **Happy（Happy Coder）** | `slopus/happy`<br>本地 CLI 包装 + iOS/Android/Web | 离开工位后的手机端远程监控与即时接管 | ① **零加价复用原生订阅 + 阻塞秒推**：Agent 停在审批/提问时手机秒推，直接回复继续跑；<br>② **移动端碎片化交互**：支持快捷操作、语音输入与团队围观实时会话。 |
| **Claude Squad** | `smtg-ai/claude-squad`<br>Go + tmux TUI 复用器 | 轻量终端内多 Agent 并行管理 | ① **极简 tmux + git worktree 组合**：一条 `cs` 命令把每个任务绑定独立分支与 worktree，彻底消灭多 Agent 互踩文件；<br>② **合并前 Diff 检查闸门**：先看改动再 `gh` 提 PR。 |
| **OpenCode** | `anomalyco/opencode`<br>本地 HTTP+SSE Server + TUI/Web | 终端优先的开源 Agent 与双端客户端架构 | ① **统一交互语法**：输入框原生支持 `/command`、`@file` 文件上下文引用、`!cmd` 内联执行；<br>② **Git 快照级 `/undo`**：每次改动留存轻量 git 快照，误改一键回滚。 |
| **cmux** | `manaflow-ai/cmux`<br>macOS 原生终端（libghostty） | 专为并行 Coding Agent 设计的终端工作台 | ① **未读阻塞跳转（`Cmd+Shift+U`）+ 通知环**：侧边栏汇总分支、PR 状态、**活跃监听端口**，一键跳到下一条等审批/等输入的会话；<br>② **Unix Socket JSON-line 控制面**：同 `baton` 路线一致，可脚本化控制全部窗口。 |
| **Agent Deck**（+ **ccusage**） | `asheshgoplani/agent-deck`（Go + tmux TUI）<br>`ryoppippi/ccusage` | 跨 Agent tmux 总控台 + 本地 Token/费用审计 | ① **会话 Fork（`f`）与状态快筛（`!` 运行 / `@` 等待 / `#` 空闲 / `&` 报错）**；<br>② **MCP Unix Socket 共享池**（多会话共享 MCP 省 85–90% 内存）+ 解析本地 transcript 计算 **Input/Output/Cache 命中率与估算费用**。 |

---

## 2. 最值得 `baton` 立即借鉴的 Top 6 实践

> `baton` 已在 v0.1.5–v0.1.7 领先完成了「BYO-tmux 零 Headless 观测」「G1/G2/G3 反幻觉防误报」「长文本防跳顶 + 长粘贴落盘胶囊」「原生 `/` 斜杠菜单」「飞书同会话话题 + 自定义静音」。以下 6 项是与现有架构最契合、投入产出比最高的增量能力。

### 实践 1：输入框 `@file` 仓库文件补全 + 快捷提示词模板（借鉴 OpenCode）
- **现状痛点**：`baton` v0.1.7 已支持 `/` 斜杠命令和长粘贴胶囊，但用户在 Web 或移动端（Tailscale）远程指挥 Agent 时，仍需手敲冗长的相对文件路径（如 `crates/baton-daemon/src/core/review.rs`），极易拼错或漏给上下文。
- **具体做法**：
  - 后端新增轻量只读接口 `GET /api/files?session_id=<id>&q=<prefix>&limit=20`，通过 `git -C <git_root> ls-files`（复用已缓存目录或 10s TTL 内存缓存）做子序列模糊匹配，零外部依赖；
  - 前端复用 v0.1.7 的斜杠菜单浮层组件：输入 `@` 时弹出文件候选列表（支持 `↑/↓/Tab/Enter/Esc`），选中后插入 `@crates/...` 路径引用；同时支持为选中代码片段一键追加 `path/to/file.rs:L10-L35` 行号引用。
- **落点模块**：`crates/baton-daemon/src/web.rs`（新增 `/api/files`）、`web/app.js`（复用 Slash Popover 架构扩展 `@` 触发器）。
- **预期收益**：手机/平板端远程下达精准修改指令的打字量降低 60% 以上，大幅减少 Agent 全仓盲搜的 Token 浪费。

### 实践 2：内置 Git Diff 查看器 +「行内批注打包回传」（借鉴 Vibe Kanban + Superset）
- **现状痛点**：当前 `baton` 的 `review.v1` 已有极强的机器证据校验（G1/G2/G3）和结构化 Finding，但人要看某会话/某 worktree 的**实时未提交代码改动**仍需切回终端敲 `git diff`，且看完想提 3 处修改意见时只能手动拼文字发消息。
- **具体做法**：
  - 后端新增 `GET /api/sessions/{id}/diff`（返回 `git diff --stat` + 按文件拆分的 unified diff，单文件限流防大文件撑爆内存）；
  - 前端在主工作区顶栏 `[阅读 | 终端]` 旁增加第 3 个轻量标签 **`改动 (N)`**（或右侧 Inspector 抽屉「变更文件」折叠区），每个文件支持「已阅」勾选和点击行号旁的 `+` 添加行内批注；
  - 底部浮动条显示「已收集 K 条批注 · 一键发送给当前 Agent」，点击后按 `file:line + 代码摘录 + 意见` 打包为一条结构化消息（走现有长文本落盘胶囊或 L0/L2 注入通道）回传给该会话！
- **落点模块**：`crates/baton-daemon/src/web.rs`（`/api/sessions/{id}/diff`）、`crates/baton-daemon/src/core/delivery.rs`（批量批注投递）、`web/app.js` + `web/app.css`（纯原生 DOM 渲染 Unified Diff，估算仅增 ~8–12KB，仍远低于 200KB 红线）。
- **预期收益**：把「看代码 → 提意见 → Agent 返工」收敛进同一个页面，天然补齐 `baton` 的人工 Review 闭环，并与 `review.v1` 的 G2 行号引用无缝衔接。

### 实践 3：全局「下一条阻塞（Next Attention）」一键跳 + 活跃监听端口显示（借鉴 cmux + Agent Deck）
- **现状痛点**：同时跑 10–20 个 tmux 会话 + Swarm 子会话时，虽然左侧已有分组折叠和状态圆点，但人回到电脑前仍需在列表里逐个扫哪几个窗格在等权限审批、等用户输入或刚报错；同时前端/服务类任务跑起来的 `localhost:xxxx` 端口藏在终端输出深处，手机通过 Tailscale 预览时不好找。
- **具体做法**：
  - **快捷键与顶栏队列**：在 Web 顶栏放一颗紧凑胶囊 `待处理 (N)` 并绑定快捷键 **`Cmd/Ctrl+Shift+U`**（以及侧栏筛选支持快速切「仅看运行中 / 等待中 / 报错」），每次按下循环跳到下一个 `NeedsPermission` / `NeedsInput` / `Error` / `AwaitingHuman` 的会话或流水线；
  - **端口感知**：在 `discover.rs` 的周期巡检中（低频 10s 一次，复用 `lsof -nP -iTCP -sTCP:LISTEN` 按 pane PID 子树过滤，失败静默降级）把该会话监听的本地端口挂到会话元数据，在右侧 Inspector 抽屉展示可点击链接（Tailscale 访问时自动替换为当前 `window.location.hostname`！）。
- **落点模块**：`crates/baton-daemon/src/core/discover.rs`、`crates/baton-daemon/src/core/router.rs`、`web/app.js`。
- **预期收益**：20+ 并行会话下的上下文切换延迟从 10 秒级降至 1 次按键；开发服务器预览链路（本机 + Tailscale）开箱即点。

### 实践 4：多 Agent 协作与交接可视化强化——Session Fork + Worktree 状态条 + Handoff 时间线（借鉴 Agent Deck + Claude Squad + Crystal）
- **现状痛点**：`baton` 已有 Feature Pipeline / Two-Speed Debug 状态机和 `handoff.v1` 协议，但日常非 Pipeline 的自由多会话协作中，用户经常想「基于当前会话上下文另开一条实验分支对比 Claude vs Codex」或一眼看清「每个会话在哪个 branch / worktree、有没有脏文件、跟谁做过交接」。
- **具体做法**：
  - **Worktree 徽标与冲突预警**：在会话卡片与 Inspector 顶栏醒目展示 `branch` + `dirty 文件数`；当**两个活跃写会话指向同一个 `cwd`（非独立 worktree）**时，在界面与新建弹窗弹出柔和黄条提醒「检测到同目录并发写入，建议启用独立 worktree」，避免 Agent 互踩；
  - **一键「交接给另一个 Agent」**：在对话顶栏和 Inspector 增加「转交（Handoff）」按钮，自动抓取当前会话最后一条总结 + `git diff --stat` 生成 `HandoffDraft`，选目标会话或 L4 新开不同模型（如 Claude 写完一键交接给 Codex/AGY 做 D1 Review），把 `baton` 独有的跨模型交接能力从 Pipeline 解放到任意日常会话！
  - **一键分叉（Fork）**：针对支持 resume/fork 的会话（如 Claude Code `--resume --fork-session`）提供 L4 交互式拉起新窗口按钮，保留对话上下文探索备选方案。
- **落点模块**：`crates/baton-daemon/src/core/discover.rs`（同目录并发检测）、`crates/baton-daemon/src/core/engine.rs`、`crates/baton-adapters`（fork 启动参数）、`web/app.js`。
- **预期收益**：在不增加任何 Headless 调用的前提下，把多 Agent 并行防冲突与跨模型协作的门槛降到一次点击。

### 实践 5：移动端 / 飞书碎片化远程操作的「三步变一步」（借鉴 Happy Coder）
- **现状痛点**：用户离开工位用手机看飞书或 Tailscale Web 时，最常见的是 3 类高频微操作：① 审批确认/拒绝；② 给卡住的 Agent 回复一句短指令（如「继续跑测试」「采用方案 A」）；③ 看完进度后决定静音一会儿。飞书卡片已支持话题续聊和自定义静音，但 Web 移动端和飞书卡片的「超快回复」还能再省一步。
- **具体做法**：
  - **飞书卡片内置上下文快捷按钮**：当检测到 `NeedsInput` 或阶段完成时，卡片除自由回复外提供 2–3 个低风险高频按钮（如「查看最近输出」「静音 30m」「继续下一步」），且对 `NeedsPermission` 明确标注**来自哪个主/子会话 + 待执行命令摘要**；
  - **Web 响应式移动端精简栏 + 快捷短语芯片**：窄屏（`≤ 768px`）下把左侧会话栏收敛为底部/抽屉切换，输入框上方提供可定制的单点快捷短语芯片（如 `继续`、`跑单测`、`总结改动`、`git status`），并支持浏览器原生 `SpeechRecognition` 语音转文字按钮（零依赖，仅在浏览器支持时显现，完全本地/系统能力）。
- **落点模块**：`crates/baton-daemon/src/lark.rs`、`web/app.css`（窄屏媒体查询优化）、`web/app.js`。
- **预期收益**：通勤/离座期间在手机上处理阻塞事件的平均耗时缩短至 5 秒内，真正做到「人离开桌面，Agent 队列不停转」。

### 实践 6：上下文水位、Prompt Cache 命中率与零成本费用可观测性（借鉴 ccusage + Agent Deck）
- **现状痛点**：`crates/baton-daemon/src/core/metrics.rs` 已经收集了不少会话与 Review 指标，但用户在使用长对话（尤其 Opus / 高强度思考 / Swarm 多子会话）时，最关心两个实战问题：**「当前上下文快满了吗（该不该 `/compact`）？」** 以及 **「今天/这个 Pipeline 烧了多少 Token，Prompt Cache 命中率如何？」**
- **具体做法**：
  - 借鉴 `ccusage` 的纯本地解析思路：在 `transcript.rs` 与 `metrics.rs` 增量解析三家官方本地 transcript（Claude JSONL 的 `usage.{input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens}`、Codex/AGY 对应统计字段），**零额外 API 请求**；
  - 在会话顶栏和 Inspector 抽屉展示一颗极克制的 **上下文水位进度条**（如 `Context 68% · Cache 89%`），超过 80% 时在输入框提示「上下文偏高，可输入 `/compact` 压缩」；
  - 在统计面板按 **今日 / 本周 / 单条 Pipeline / 模型维度** 聚合 Token 分布与参考估算费用，与现有的 `review.v1` Precision 误报率并排展示，形成「质量 × 成本」双维度看板。
- **落点模块**：`crates/baton-daemon/src/core/transcript.rs`、`crates/baton-daemon/src/core/metrics.rs`、`web/app.js`。
- **预期收益**：提前预防长对话静默降智与限流，同时让用户在决定「Plan 用 Opus、Implement 用 Codex、Review 用 AGY」时拥有真实的成本与精度数据支撑。

---

## 3. 刻意不学的反模式（守护 `baton` 核心边界）

开源生态中有很多流行做法，在桌面端或单一 Agent 场景下很酷，但会**直接破坏 `baton` 的底层架构原则**，必须明确拒绝：

| 反模式 | 代表做法 | 为什么 `baton` 刻意不学 |
|---|---|---|
| **1. 自己重造终端模拟器或接管用户 tmux 布局** | `cmux`（自建 Ghostty GUI App）、部分桌面工具强制重排/接管全部窗格 | 违背 **H2（BYO-tmux）** 与 **H5（Rust 单二进制）**。用户已有成熟的 tmux / Ghostty / iTerm2 工作流，`baton` 坚持只认领已有 pane 或 `-d` 新建 window，绝不绑架终端选型。 |
| **2. 用 Headless / `-p` / `exec` 后台静默跑 Agent** | Vibe Kanban / 部分自动化编排器用非交互子进程静默执行任务 | 违背 **H1（零 Headless）**。Headless 无法随时接管、丢失原生交互体验（如 `/swarm`、`/boost`、审批上下文），且容易在出错时变成黑盒。`baton` 坚持一切执行发生在可见可接管的 tmux 交互会话中。 |
| **3. 引入 Electron / React / Vite / Monaco 重型前端栈** | Superset、Crystal/Nimbalyst、ClaudeCodeUI 的重型桌面/前端构建链 | 违背 **H5（磁盘可控、零 `node_modules`、`web/` ≤ 200KB、`include_bytes!` 单二进制内嵌）**。`baton` 的原生 ESM + 温润纸感 CSS 已证明无需 300MB Electron 也能做到丝滑交互。 |
| **4. 默认开启全自动 YOLO（`--dangerously-skip-permissions`）** | Claude Squad `--autoyes`、部分一键自动化脚本默认绕过全部审批 | 违背安全边界与 **H4（非侵入、可控）**。高危命令审批必须由用户显式在 `team.toml [spawn]` 中自行选择，且远程审批默认关闭；`baton` 的价值在于「把真正需要人的审批秒级推到眼前」，而不是关掉安全网。 |
| **5. 依赖云端中继服务器或托管同步账号** | Happy 的云中继、早期 OpenCode 的 `opncd.ai` 云端分享（官方近期亦已下线） | 违背本地优先与隐私极简原则。`baton` 配合用户已有的 **Tailscale 私有内网 + 飞书自建机器人** 即可实现零第三方中继的端到端远程控制，不引入任何云账号依赖。 |
| **6. 无门禁的「多模型并发群殴 Review」** | 同时拉 3 个模型散文式挑刺再简单拼接 | 违背 **H6（结论必须有证据）**。正如 `architecture.md §7.1` 所述，不经 G1/G2/G3 校验的多 Reviewer 只会把误报噪声翻倍；`baton` 坚持确定性代码行校验 + 跨模型证伪（Falsifier）+ 误报率闭环。 |
| **7. 强侵入修改 Agent 私有配置或劫持 PTY 进程树** | 部分工具直接改写 `codex config.toml notify` 或在 Agent 外层强套自定义 PTY 代理 | 违背 **H3（只用官方 Hook / MCP）** 与 **H4（安装可逆、卸载逐字节还原）**。 |

---

## 4. 建议落地优先级路线图（供 v0.1.8+ 规划参考）

| 优先级 | 实践项 | 复杂度 | 主要改动面 | 核心价值 |
|---|---|---|---|---|
| **P0（最先做）** | **内置 Diff 视图 + 行内批注打包回传**（实践 2） | 中 | `web.rs` + `web/app.js` + `web/app.css` | 补齐 Web 端「看改动 → 批注 → 打回 Agent」闭环 |
| **P0（最先做）** | **输入框 `@file` 补全**（实践 1） | 低 | `web.rs`（`git ls-files`）+ `web/app.js` | 复用已有 `/` 菜单，极低成本大幅提升远程指令精准度 |
| **P1（紧接着）** | **上下文水位条 + 本地 Token/Cache 统计**（实践 6） | 低–中 | `core/transcript.rs` + `core/metrics.rs` + `web/app.js` | 提前预警 `/compact`，形成「质量 × 成本」闭环 |
| **P1（紧接着）** | **`Cmd+Shift+U` 下一条阻塞 + 监听端口发现**（实践 3） | 低 | `core/discover.rs` + `web/app.js` | 20+ 会话高效巡检 + Tailscale 一键预览本地服务 |
| **P2（按需跟进）** | **一键跨 Agent Handoff + 同目录并发防踩提示**（实践 4） | 中 | `core/discover.rs` + `core/engine.rs` + `web/app.js` | 将跨模型交接从 Pipeline 扩展至任意自由会话 |
| **P2（按需跟进）** | **移动端窄屏快捷短语 + 语音输入 + 飞书一键短操作**（实践 5） | 低–中 | `lark.rs` + `web/app.css` + `web/app.js` | 进一步压缩离座场景操作步数 |
