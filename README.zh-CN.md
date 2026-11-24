<p align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="starfane logo" />
</p>

<h1 align="center">starfane</h1>

<p align="center">
  <strong>轻量级 Rust 单二进制多 Agent 控制面：跨 <code>tmux</code>、<code>zellij</code>（0.44+）与多机联邦节点统一编排 Claude Code、Codex CLI、Antigravity (<code>agy</code>) 与可选 Shell 窗格。</strong>
</p>

<p align="center">
  <a href="README.md">English</a> · 简体中文
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-1.82%2B-orange?logo=rust" alt="Rust 1.82+" />
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT License" />
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="Platform macOS | Linux" />
  <img src="https://img.shields.io/badge/architecture-BYO--tmux%20%C2%B7%20Zellij%20%C2%B7%20Zero%20Headless-1E1D1B" alt="BYO-tmux · Zellij · Zero Headless" />
</p>

---

## Web 控制台预览

<p align="center">
  <img src="assets/preview.png" width="100%" alt="starfane Web console preview" />
</p>

---

## 核心特性

- **零 Headless · BYO-`tmux` 与 `zellij`（0.44+）**：通过统一的 `TerminalMux` 抽象同时支持 `tmux` 与 `zellij >= 0.44`（检测到自动启用），保留你长期运行的交互式 `claude`、`codex` 与 `agy` 窗格；启用 `[discovery] include_shell_panes = true` 后可一并纳管普通终端/训练/监控窗格（`shell`）；新建交互会话默认工作目录 `~/.agent_session`（指定目录时尊重用户目录）。
- **亚 10ms Hook 热路径**：`starfane emit` 仅依赖 Rust 标准库（无异步运行时冷启动开销），通过本地 UDS（`starfaned.sock`）上报事件，守护进程重启期间自动落盘 spool 补传。
- **全量历史浏览与零乱码内嵌 Web 控制台**：前端拆分为零构建原生 ES Modules（`web/app.js` + `web/js/*.js`）并支持零拷贝 ETag `304` 缓存；支持游标分页全量对话记录（`加载全部` / 加载更早历史）与全量终端滚动历史（`tmux capture-pane -S -` / `zellij dump-screen --full`，按 200 行分块配合 `content-visibility: auto` 流畅渲染 50,000+ 行）；内置离屏 `<canvas>` Nerd Font 探测与 Powerline/P10k/Starship PUA 私有图标兼容映射（右上角支持 `图标: 兼容 / 原始` 切换，亦支持 `[web] terminal_font` 或 `~/.config/starfane/fonts/terminal.woff2` / `.ttf` 自定义字体），彻底消除终端字体方块乱码。
- **多机联邦纳管（`[[nodes]]`）**：支持在同一台 Hub 上混搭 **SSH ControlMaster 免安装节点**（`kind = "ssh"`，适合云主机/GPU 容器）与 **对等 `starfaned` 联邦节点**（`kind = "peer"`，支持多候选出站 `endpoints` 自动故障切换与对话记录代理）；侧边栏按主机分组展示，并支持在页面中实时重命名主机展示名（`POST /api/hosts/{id}/rename` 持久化）。
- **按需启用多渠道通知路由与 Split Ownership**：全新安装默认零通知通道且保持静默，100% 向后兼容原有 `[feishu]` 飞书双向卡片（自动映射为 `feishu-default`）；支持按需配置 `[[notifiers]]`（多套 `feishu` profile、`telegram`、`webhook`、`macos` 原生通知）与 `[[notify_routes]]` 差异化路由，仓库内 `.starfane/team.toml` `[notify]` 仅允许引用逻辑 ID（解析阶段拒绝任何密钥/URL）。基于 **Split Ownership（`node_local` / `hub`）**，Peer 节点默认本地自发通知、凭证完全隔离不重复推送，SSH 免安装节点默认由 Hub 代发。
- **原生 `tmux` 定期快照与重启自动恢复**：内置 `[tmux.backup]` 支持定期保存去重后的 `tmux` 会话、窗口与分屏布局，支持守护进程启动时在空 `tmux` 环境下自动重建现场（`auto_restore_on_start = true`）并按需恢复 `claude --resume` / `codex resume` 会话（`starfane tmux backup | restore | backups`）。
- **Swarm 子会话可观测与防误报证据门禁**：自动折叠父子会话与 Swarm Worker，实时展示上下文占比与生成速度（`tok/s`）；内置 **新功能协作（Feature Pipeline）** 与 **两速查错（Two-Speed Debug）**，支持自然语言阶段模型绑定与确定性 G1/G2/G3 证据核验。
- **组网无关通用访问门禁（`AccessGate`）**：启动期预编译 IPv4/IPv6 `allowed_cidrs`、精确与 `*.suffix` 通配 `allowed_hosts`（适配反向代理 80/443/自定义端口）、具名 `presets`（`easytier` / `tailscale`）以及 `[[web.scoped_tokens]]`（支持 `read_only` 只读观察令牌或联邦令牌），在网关层严格拦截未授权局域网与公网访问（`403`）。

---

## 系统架构

<p align="center">
  <img src="assets/architecture.png" width="100%" alt="starfane Architecture Diagram (BYO tmux, starfaned daemon, Control Surfaces)" />
</p>

## 两大工作流与证据门禁

<p align="center">
  <img src="assets/workflow.png" width="100%" alt="starfane Feature Pipeline and Two-Speed Debug Workflows with G1/G2 Evidence Gate" />
</p>

---

## 快速开始

### 环境准备

- **Rust `1.82+`** 与 Cargo
- **`tmux`** 与/或 **`zellij` `>= 0.44`** *（可选，已安装时自动识别）*
- 已安装并登录至少一个编码 Agent CLI：[`claude`](https://docs.anthropic.com/en/docs/claude-code)、[`codex`](https://developers.openai.com/codex/cli) 或 `agy`
- *（可选）* `lark-cli`（若需要飞书双向卡片通知）

### 1. 编译与安装

```bash
git clone https://github.com/starlitxiling/starfane.git
cd starfane
cargo build --release --bin starfane

mkdir -p ~/.local/bin
ln -sf "$(pwd)/target/release/starfane" ~/.local/bin/starfane
```

确认 `~/.local/bin` 已在 `PATH` 中，并执行自检：

```bash
starfane doctor
```

### 2. 安装官方 Hook 与 MCP

先用 `--dry-run` 预览配置变更（实际写入时会自动备份原文件，严格保留非 starfane 的现有 hooks 与 Codex `notify`）：

```bash
starfane install --all --dry-run
starfane install --all
```

> 也可通过 `--agent claude|codex|agy` 单独安装，或 `--scope project` 仅安装到当前仓库；随时可执行 `starfane uninstall --all` 完整无损回滚。

### 3. 启动守护进程并打开控制台

```bash
starfane daemon --detach
starfane web
```

仅打印解析后的控制台地址而不拉起浏览器：

```bash
starfane web --print-url --no-open
```

---

## 配置参考（`~/.config/starfane/config.toml`）

```toml
[web]
bind = "127.0.0.1"              # 若通过私有组网或反向代理对外开放可设为 "0.0.0.0"
port = 7878
auth_enabled = true             # 默认开启 Token 鉴权（含 scoped_tokens）；仅在可信组网 CIDR 下可设为 false
allowed_cidrs = []              # 组网无关 IPv4/IPv6 白名单，如 ["10.88.0.0/16", "fd00::/8"]
allowed_hosts = []              # 精确或 *.suffix 域名白名单，如 ["console.example.com", "*.ts.net"]
presets = []                    # 可选具名预设：["easytier", "tailscale"]
allow_easytier = true           # 旧版预设开关（完全兼容）
easytier_cidrs = ["10.144.144.0/24"]
allow_tailscale = false         # 旧版 Tailscale 开关（放行 100.64.0.0/10、fd7a:115c:a1e0::/48 与 *.ts.net）
terminal_font = ""              # 可选终端字体名（如 "JetBrainsMono Nerd Font"），前置到 Web 终端字体栈
                                # 也可将字体放入 ~/.config/starfane/fonts/terminal.woff2（或 .ttf）自动加载

# 可选作用域令牌（如只读观察端或联邦 Hub 客户端）
# [[web.scoped_tokens]]
# label = "viewer"
# token_env = "STARFANE_VIEWER_TOKEN"
# read_only = true

[notify]
watch_window_secs = 90
escalate_after_secs = 180
away_digest_after_secs = 600    # 离开电脑期间聚合多会话完成通知，防止卡片轰炸
cooldown_secs = 120

[gate]
user_inactive_secs = 15
paste_settle_ms = 250
ack_timeout_ms = 4000
max_pointer_len = 300

# 传统飞书双向卡片通道（启用后自动合成逻辑 ID "feishu-default"）
[feishu]
enabled = false
lark_cli = "lark-cli"
chat_id = ""
allow_open_ids = []
allow_chat_ids = []

# 按需启用的多渠道插件通知（feishu / telegram / webhook / macos）
# [[notifiers]]
# id = "feishu-work"
# kind = "feishu"
# profile = "work"              # 可选 `lark-cli --profile <name>` 实现多租户隔离
# chat_id = "oc_xxx"
# ownership = "node_local"      # 默认仅推送本机会话通知（Split Ownership，隔离公司凭证）
#
# [[notifiers]]
# id = "tg-ops"
# kind = "telegram"
# bot_token_env = "STARFANE_TG_TOKEN"
# chat_id = "-1001234567890"
# ownership = "hub"             # 允许在 Hub 侧为 SSH 免安装节点代发通知
#
# [[notify_routes]]
# node = "tencent"
# project = "workflow"          # 同时支持 `cwd`、`agent`、`event_kinds` 匹配
# notifiers = ["tg-ops"]

[paths]
claude = "claude"
codex = "codex"
agy = "agy"
tmux = "tmux"
zellij = "zellij"

[discovery]
include_shell_panes = false     # 设为 true 可在控制台一并显示普通 tmux/zellij shell 与脚本窗格

[tmux]
socket_name = ""                # 可选 `tmux -L <name>`

[tmux.backup]
enabled = false                 # 开启定期 tmux 布局与 Agent 恢复快照
interval_secs = 300             # 启用后最小 10 秒
max_snapshots = 20              # 历史 snapshot-*.json 滚动保留数量
auto_restore_on_start = false   # 守护进程启动且当前无 tmux 会话时自动从 latest.json 恢复
restore_agents = true           # 重建窗格后自动恢复已保存的 `claude --resume` / `codex resume`
restore_bypass_permissions = false
exclude_sessions = []

[zellij]
enabled = true                  # 自动发现并接管本机 Zellij（>= 0.44）会话（未安装时静默跳过）

[node]
id = "local"
display_name = "Local"          # 运行期可通过 Web 弹窗或 POST /api/hosts/{id}/rename 重命名

# 多机联邦节点：支持 SSH 免安装与 Peer starfaned 混搭
# [[nodes]]
# id = "gpu-box"
# display_name = "GPU 服务器"
# kind = "ssh"
# ssh_target = "ubuntu@gpu-box" # user@host 或 ~/.ssh/config 别名
# include_shell_panes = true
# notify_ownership = "hub"      # SSH 节点默认由 Hub 代发通知
#
# [[nodes]]
# id = "mac-work"
# display_name = "公司 Mac"
# kind = "peer"
# endpoints = ["http://10.144.144.2:7878", "http://100.64.0.2:7878"]
# token_env = "STARFANE_MAC_WORK_TOKEN"
# notify_ownership = "node_local" # Peer 节点默认 Split Ownership：由对端本机通知，Hub 不重复推送
```

> **项目级通知路由（`.starfane/team.toml`）**：仓库内可通过 `[notify] notifiers = ["feishu-work"]` 或 `[[notify.routes]]` 仅引用逻辑通知 ID；配置解析器会严格拒绝任何内嵌 `token`、`secret`、`url`、`chat_id` 等敏感字段，杜绝密钥入库。

> **安全说明**：`starfaned` 在启动期预编译 `AccessGate`，对每个请求统一校验来源 IP（`peer_ip_gate`：回环、配置 `allowed_cidrs` 与开启的预设）、HTTP `Host` / 同源 `Origin` 与常时间 Token / Scoped Token 鉴权，普通局域网（`192.168.x.x`、`10.17.x.x`、`172.16.x.x`）与公网请求直接返回 `403`。

---

## CLI 常用命令

| 命令 | 说明 |
| :--- | :--- |
| `starfane doctor` | 只读检查 Agent CLI、Hook、MCP、Codex 信任哈希、`tmux`（含 `history-limit`）、`zellij`（`>= 0.44`）、守护进程与配置 |
| `starfane install [--all \| --agent X]` | 安装带标记的 Hook 与 MCP（支持 `--dry-run`，自动备份原配置） |
| `starfane uninstall [--all \| --agent X]` | 干净还原 starfane 写入的配置条目 |
| `starfane daemon [--detach \| --foreground]` | 启动 `starfaned` 守护进程（支持 `--port`、`--web-dir` 前端热重载） |
| `starfane web [--print-url] [--no-open]` | 打开或打印当前 Web 控制台地址 |
| `starfane ls [--all] [--project P]` | 列出活跃 tmux/zellij/远程会话、状态、模型、上下文占比、TPS 与流水线 |
| `starfane tmux backup [--force]` | 手动备份一次 tmux 布局与 Agent 会话 ID（守护进程离线亦可用） |
| `starfane tmux restore [--file P] [--no-agents] [--dry-run]` | 从最近或指定快照恢复缺失的 tmux 会话/窗口/分屏，并可恢复 claude/codex |
| `starfane tmux backups` | 查看已保存的 tmux 快照清单 |
| `starfane handoff --from A --to B` | 在两个会话之间投递结构化上下文交接包 |
| `starfane pipeline new "<说明>"` | 解析自然语言阶段/模型绑定并拉起 Feature 工作流 |
| `starfane debug "<现象; 绑定>"` | 启动两速查错流程（支持 `continue <id>` / `stop <id>`） |
| `starfane review <show\|feedback\|verdict\|precision>` | 查看带证据校验的评审结论与历史误报率 |

新增核心 Web / 联邦接口：`GET /api/hosts`、`POST /api/hosts/{id}/rename`、`GET /api/notifiers`、`POST /api/notifiers/{id}/test`、游标分页 `GET /api/sessions/{id}/transcript?limit=&before=&all=1`，以及全量滚动抓取 `GET /api/panes/{pane_id}/preview?lines=&before_line=&all=1`。

---

## 仓库结构

```text
crates/
  starfane-proto      # 共享协议类型、HostRecord/HostId、状态机转换与 Schema（纯 serde）
  starfane-emit       # 纯 std Hook 热路径（<10ms）与离线 spool 落盘
  starfane-adapters   # claude / codex / agy Hook 安装、事件归一化、L4 启动参数组装与 doctor 自检
  starfane-tmux       # TerminalMux 抽象、tmux + Zellij（>= 0.44）后端、本机/SSH 执行层、7 重安全注入门禁与快照备份
  starfane-evidence   # Review / Triage / RCA 的 G1/G2 确定性证据校验器
  starfane-binding    # 自然语言工作流阶段-模型绑定解析器
  starfane-daemon     # tokio + axum 控制面守护进程（REST/SSE、AccessGate、NotifyRouter、多机 NodeProvider、对话遥测、MCP、飞书）
bin/starfane          # 单二进制 CLI 主入口
tools/fake-agent      # 交互式 TUI 与 Hook 沙箱测试工具
web/                  # 零构建内嵌 Web 控制台（原生 HTML/CSS + ES Modules）
  index.html
  app.css
  app.js              # ES 模块入口与路由调度
  js/                 # 模块化源码：constants / store / dom / markdown / terminal / api / topbar / composer / workspace / views / overlays
tests/fixtures/       # 脱敏的真实 Hook 输入样例库（41 个用例）
docs/                 # 架构文档、Crate 契约、环境与 Spike 调研笔记
```

### 本地校验

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
node --check web/app.js web/js/*.js
cargo test --workspace
cargo test -p starfane-tmux -- --include-ignored
```

---

## 路线图

已完成落地：
- ~~**Zellij（0.44+）原生适配**~~ — 统一 `TerminalMux` 与 `ZellijMux` 发现、全量抓屏、安全注入、焦点探测与创建窗格。
- ~~**多机联邦协同（`[[nodes]]`）**~~ — 支持 SSH `ControlMaster` 免安装节点与多候选地址 Peer `starfaned` 联邦混搭及主机重命名。
- ~~**按需多渠道通知路由与 Split Ownership**~~ — 支持 `[[notifiers]]`（飞书多 Profile、Telegram、Webhook、macOS 原生）、`[[notify_routes]]` 与 `.starfane/team.toml` 无密钥路由。
- ~~**组网无关通用访问门禁（`AccessGate`）**~~ — IPv4/IPv6 `allowed_cidrs`、通配 `allowed_hosts`、具名预设与 `read_only` / 管理 `scoped_tokens`。

后续计划：
- **Zellij 布局备份与远程 SSH Zellij 纳管**：将布局快照恢复与零安装 SSH 发现能力扩展至远程 Zellij 会话。
- **移动端短码配对与更多内置交互渠道**：一次性扫码/短码配对接入 Web 控制台，并在通用 Webhook 之外提供 Slack / Discord 原生交互卡片适配器。
- **跨主机全局检索与历史统计**：支持跨联邦节点的历史对话搜索与 Token/吞吐趋势汇总。

---

## 开源协议

[MIT](LICENSE)
