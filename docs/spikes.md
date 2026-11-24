# M0 前置技术 Spike 实测报告

> 测试日期：2026-11-20 · 隔离环境：`tmux -L starfane-spike` 与独立临时 `CODEX_HOME`（未触碰用户任何真实 tmux 会话、未修改 `~/.claude` / `~/.codex` / `~/.gemini`）。
> 测试版本：`agy 1.5.13`、`codex-cli 0.167.2`、`tmux 3.7c`。

---

## Spike 1：`agy -i` 初始 Prompt 行为与斜杠命令（`/swarm`、`/boost`、`/teamwork`）触发策略

### 1.1 实测方法
1. 只读分析 `agy 1.5.13` 二进制符号与 `--help` 文本。
2. 在隔离 `tmux -L starfane-spike` 中进行两组实验：
   - **实验 A（未信任目录）**：在新建临时目录执行 `agy -i "/help"`。
   - **实验 B（已信任目录 `/Users/demo/work/starfane`）**：先执行 `agy -i "/help"` 观察 5 秒，再在同一 pane 内执行 L2 注入序列：`printf '%s' '/help' | tmux load-buffer -b starfane_spike -` → `tmux paste-buffer -d -p -b starfane_spike -t <pane>` → 等待 `300ms` → `tmux send-keys -t <pane> C-m`。

### 1.2 实测结果
1. **L4 新目录信任弹窗陷阱（实验 A）**：
   - 在未信任目录（例如首次创建的隔离 `git worktree`）启动 `agy -i` 时，TUI 会停在交互式信任确认界面：
     `Do you trust the contents of this project? > Yes, I trust this folder / No, exit`。
   - **架构含义**：L4 spawn 后如果 5–10 秒内未收到 `SessionStart` / `PreInvocation` hook，应提示用户当前新窗格可能停在 Workspace Trust 确认弹窗（或在已信任的父仓库目录/子目录中创建 worktree，避免卡死）。
2. **`agy -i` 对斜杠命令的处理（二进制 + 实验 B）**：
   - 二进制内 `google3/third_party/jetski/cli/slashcmd` 包含 `LeadingName`、`ParseLeading`、`ParseSuffix`、`StripLeading`、`PlanPrompt` 等解析器，且 changelog 明确说明：Skill 类斜杠命令（如 `/swarm` 等注册技能）会走技能展开，提供 `--disable-slash-commands` 仅对 `-p/--print` 生效。
   - 但在实验 B 中观察到关键差异：`agy -i "/help"` 在初始化完成后（约 4–5 秒）将 `/help` 作为初始会话消息发往模型侧（终端出现 `> /help` + `⣯ Generating...`），而**没有**触发本地 TUI 的 `/help` 面板；随后用 **L2 注入**（`paste-buffer -d -p` + `300ms` + `C-m`）发出的 `/help` 则**立刻成功触发**了 `agy` 原生 TUI 斜杠命令面板（`Antigravity CLI general commands shortcuts`），且没有被自动补全弹窗吞掉。
   - 此外，`agy --help` 明确：`--print` 与 `--prompt-interactive` 互斥，且启动耗时约 3–5 秒才出现提示符 `>`。

### 1.3 工程落地结论（面向 M2/M3）
- **常规任务（无斜杠命令）**：优先走 **L4**（`agy --model <m> --effort <e> -i "starfane: ... 请调用 starfane.read_inbox"`），避开向运行中 TUI 注入的任何风险。
- **需要触发 `agy` TUI 斜杠命令（`/swarm <args>`、`/boost <args>`、`/teamwork <args>`）**：
  - **推荐两步策略**：(1) 若复用已有空闲 `agy` pane，走 **L2 门禁注入**（`load-buffer -` → `paste-buffer -d -p` → 等待 `250–300ms` → `send-keys C-m`），且**始终带任务参数**（`/swarm <指针>`，绝不发裸 `/swarm`）；(2) 若通过 L4 新开窗格，先以不带斜杠的指针启动或空启动等待 `SessionStart`/提示符就绪后再走 L2 注入斜杠命令；(3) 3–5 秒内未收到 `PreInvocation` ACK 时，在 Web/飞书卡片附一键复制命令兜底。

---

## Spike 2：`codex queue` 对 Live TUI 的行为特征与超时回退策略

### 2.1 实测方法
1. 只读分析 `codex 0.167.2` 原生 ARM64 二进制中 `QueueCommand`、`thread/queue/add`、`queue_1.sqlite` 相关逻辑与 `codex app-server daemon --help`。
2. 使用完全隔离的 `CODEX_HOME=$(mktemp -d)`（零接触用户真实 `~/.codex`）实测以下调用边界：
   - `codex queue --thread <uuid> --message "starfane: ping"`（不带 / 带 `--resume`）
   - `codex queue --thread test-id --message "--leading-dash"`
   - `codex queue -- --thread test-id --message "hi"`

### 2.2 实测结果
1. **底层协议与存储机制**：
   - `codex queue` 并非简单写文件，而是通过本地 `app-server` daemon 发起 JSON-RPC 调用 **`thread/queue/add`**，持久化到 `$CODEX_HOME/queue_1.sqlite`。
   - 当目标 thread 不存在时，立刻返回 **exit 1** 与清晰错误：`thread/queue/add failed: failed to read thread: invalid thread-store request: no rollout found for thread id ... (code -32603)`。
   - 当 daemon 版本/配置不支持或 `--resume` 失败时，二进制明确输出 `Warning: message was queued, but could not resume thread ...` **但命令仍可能返回 exit 0**（见 `--help` 文档：“A failed resume produces a warning and the command still succeeds”）。
2. **CLI 参数解析硬陷阱（已实测复现）**：
   - `--message "--leading-dash"` 直接报错 **exit 2**（`error: unexpected argument '--leading-dash' found`）！
   - 传递 `--` 分隔符同样报错 **exit 2**（二进制中显式禁用 `--`）。
   - **修复方案**：调用 `codex queue` 时，**必须**使用等号绑定形式 `--message=<TEXT>` **且**统一加安全前缀 `starfane: `（绝不以 `-` 开头）。
3. **对 Live TUI 的即时性与离线限制**：
   - 当 Codex TUI 使用默认共享 daemon 模式（`daemon_auto_start = true`）运行时，`codex queue --thread <session_id> --message="starfane: ..." --resume` 可通过 `app-server` 投递；
   - 但若用户以 **`--no-daemon`** 启动独立 TUI，或 TUI 未绑定同一 app-server 控制 socket，或 `--resume` 静默告警，消息只会滞留在 `queue_1.sqlite` 而不立即触发当前 TUI 回合。

### 2.3 工程落地结论（面向 M2 投递阶梯）
- **L1（`codex queue`）调用规范**：
  ```bash
  codex queue --thread "$SESSION_ID" --message="starfane: 新任务 $TASK_ID，请调用 starfane.read_inbox" --resume
  ```
- **强制超时回退状态机（绝不信任 exit 0 即已送达）**：
  1. 若 `codex queue` 返回非 0（如 `no rollout found` / daemon 未运行），立即降级到 **L2（门禁粘贴）**；
  2. 若 `codex queue` 返回 0，检查 stderr 是否含 `Warning: message was queued, but could not resume thread`——如有则直接降级 **L2**；
  3. 否则启动 **5 秒 ACK 计时器**等待目标会话上报 `UserPromptSubmit` hook；若 5 秒内未收到 ACK，视为 live TUI 未即时消费，自动降级 **L2**（若 L2 门禁不满足则保留 **L0 inbox** 并在 Web/飞书提示用户）。
