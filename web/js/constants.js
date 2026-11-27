// starfane web console — shared constants and built-in fallback catalogs
import { L } from './i18n.js';

export const VIEWS = ['workspace', 'inbox', 'pipelines', 'reviews'];
export function viewLabel(v) {
  return {
    workspace: L('会话对话', 'Workspace'),
    inbox: L('待处理', 'Inbox'),
    pipelines: L('流水线', 'Pipelines'),
    reviews: L('代码评审', 'Reviews'),
  }[v] || String(v ?? '');
}
export const AGENTS = ['claude', 'codex', 'agy'];
export const AGENT_DISPLAY = { claude: 'Claude', codex: 'Codex', agy: 'AGY', shell: 'Shell' };
export const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];
export const DEPTHS = ['D0', 'D1', 'D2', 'D3'];
export const SEVERITIES = ['blocker', 'major', 'minor', 'nit'];
export const SSE_TYPES = ['event', 'session', 'host', 'output', 'pipeline', 'artifact', 'review', 'delivery', 'attention', 'notifiers', 'resync', 'ping'];
export const NOTIFIER_KINDS = ['feishu', 'telegram', 'discord', 'slack', 'wecom', 'qq', 'webhook', 'macos'];
export function notifierKindLabel(kind) {
  return {
    feishu: L('飞书', 'Feishu'),
    telegram: 'Telegram',
    discord: 'Discord',
    slack: 'Slack',
    wecom: L('企微', 'WeCom'),
    qq: 'QQ',
    webhook: 'Webhook',
    macos: 'macOS',
  }[String(kind || '').toLowerCase()] || String(kind || 'channel');
}
export function stageLabel(k) {
  return {
    plan: 'Plan',
    implement: 'Implement',
    test_review: 'Test & Review',
    report: 'Report',
    triage: L('快筛 Triage', 'Fast Triage'),
    deep: L('深挖 Deep', 'Deep RCA'),
  }[k] || String(k ?? '').replace(/_/g, ' ');
}
export const PLAYBOOK_STAGES = {
  feature: ['plan', 'implement', 'test_review', 'report'],
  debug: ['triage', 'deep'],
};
export function stateLabel(s) {
  return {
    working: L('运行中', 'Working'),
    starting: L('启动中', 'Starting'),
    idle: L('空闲', 'Idle'),
    needs_permission: L('等待审批', 'Needs approval'),
    needs_input: L('等待输入', 'Needs input'),
    error: L('异常', 'Error'),
    interrupted: L('已中断', 'Interrupted'),
    suspended: L('已休眠', 'Suspended'),
    ended: L('已结束', 'Ended'),
    unknown: L('未知', 'Unknown'),
    running: L('进行中', 'Running'),
    pending: L('待开始', 'Pending'),
    awaiting_human: L('待人工确认', 'Awaiting human'),
    awaiting_binding_confirm: L('待确认绑定', 'Confirm bindings'),
    blocked: L('已阻塞', 'Blocked'),
    done: L('已完成', 'Done'),
    cancelled: L('已取消', 'Cancelled'),
    failed: L('已失败', 'Failed'),
    skipped: L('已跳过', 'Skipped'),
    sent: L('已发送', 'Sent'),
    queued: L('已入队', 'Queued'),
    delivered: L('已送达', 'Delivered'),
    unconfirmed: L('未确认', 'Unconfirmed'),
    confirmed: L('已确认', 'Confirmed'),
    disproved: L('已证伪', 'Disproved'),
    downgraded: L('已降级', 'Downgraded'),
    pending_falsifier: L('待证伪', 'Pending falsifier'),
  }[s] || String(s ?? '').replace(/_/g, ' ');
}
export const STATE_PRIO = {
  needs_permission: 0,
  needs_input: 0,
  error: 1,
  working: 2,
  starting: 3,
  idle: 4,
  interrupted: 4,
  suspended: 5,
  unknown: 6,
  ended: 7,
};
export function levelLabel(lvl) {
  return {
    l0_inbox: L('L0 收件箱', 'L0 Inbox'),
    l1_codex_queue: L('L1 Codex 队列', 'L1 Codex Queue'),
    l2_inject: L('L2 直接注入', 'L2 Inject'),
    l4_spawn: L('L4 新建窗口', 'L4 Spawn'),
  }[lvl] || String(lvl ?? '');
}
export const TOKEN_KEY = 'starfane_token';
export const THEME_KEY = 'starfane_theme';
export const REQUEST_TIMEOUT_MS = 15000;
export const HEALTH_POLL_MS = 8000;
export const PREVIEW_POLL_MS = 3000;
export const PREVIEW_LINES = 400;
export const PREVIEW_STEPS = [400, 1000, 'all'];
export const PREVIEW_PAGE_STEP = 1000;
export const TRANSCRIPT_LIMIT = 60;
export const TRANSCRIPT_PAGE_STEP = 100;
export const LIVE_TAIL_LINES = 30;
export const CODE_COLLAPSE_LINES = 24;
export const TERM_CHUNK_LINES = 200;
export const TICK_MS = 15000;
export const PARSE_DEBOUNCE_MS = 380;
export const MAX_ACTIVITY = 80;
export const MAX_BACKOFF_MS = 15000;
export const TOAST_MS = 4200;
export const COLLAPSED_KEY = 'starfane_collapsed_groups';
export const GROUPS_INIT_KEY = 'starfane_groups_init';
export const SPAWN_PREFS_KEY = 'starfane_spawn_prefs';
export const OUTLINE_COLLAPSED_KEY = 'starfane_outline_collapsed';
export const GLYPH_MODE_KEY = 'starfane_term_glyph_mode';
export const NON_TMUX_GROUP = '非 tmux / 后台';
export const NON_TMUX_OPEN_KEY = 'starfane_nontmux_open';
export const PASTE_CHAR_LIMIT = 1500;
export const PASTE_LINE_LIMIT = 24;
export const IMG_MAX_BYTES = 10 * 1024 * 1024;
export function effortLabel(e) {
  return {
    low: L('low · 低', 'low'),
    medium: L('medium · 中', 'medium'),
    high: L('high · 高', 'high'),
    xhigh: L('xhigh · 超高', 'xhigh'),
    max: L('max · 最大', 'max'),
  }[e] || String(e ?? '');
}
export function starfaneSlash() {
  return [
    { cmd: '/feature', args_hint: '', desc: L('切换为新功能协作模式', 'Switch to feature pipeline mode'), group: 'starfane', action: 'mode_feature' },
    { cmd: '/debug', args_hint: '', desc: L('切换为两速查错模式', 'Switch to two-speed debug mode'), group: 'starfane', action: 'mode_debug' },
    { cmd: '/mute', args_hint: '', desc: L('打开通知静音与渠道设置', 'Open notification mute & channel settings'), group: 'starfane', action: 'open_mute' },
    { cmd: '/terminal', args_hint: '', desc: L('切换阅读视图 / 原始终端', 'Toggle reading / raw terminal view'), group: 'starfane', action: 'toggle_terminal' },
    { cmd: '/drawer', args_hint: '', desc: L('展开当前会话详情抽屉', 'Open current session inspector drawer'), group: 'starfane', action: 'open_drawer' },
  ];
}
export function fallbackSlash(agent) {
  const catalogs = {
    agy: [
      { cmd: '/swarm', args_hint: L('<任务>', '<task>'), desc: L('群体多 Agent 并行协作', 'Parallel multi-agent swarm collaboration'), group: 'native', needs_args: true },
      { cmd: '/boost', args_hint: L('<任务>', '<task>'), desc: L('高算力深度思考执行', 'Deep reasoning execution'), group: 'native', needs_args: true },
      { cmd: '/teamwork', args_hint: L('<任务>', '<task>'), desc: L('多角色分工协同推进', 'Multi-role collaborative execution'), group: 'native', needs_args: true },
      { cmd: '/plan', args_hint: L('<目标>', '<goal>'), desc: L('先出实施计划再动手', 'Draft implementation plan before editing'), group: 'native', needs_args: true },
      { cmd: '/compact', args_hint: '', desc: L('压缩当前上下文窗口', 'Compact current context window'), group: 'native' },
      { cmd: '/model', args_hint: L('<模型>', '<model>'), desc: L('切换当前会话模型', 'Switch current session model'), group: 'native', needs_args: true },
      { cmd: '/effort', args_hint: L('<档位>', '<effort>'), desc: L('调整思考强度档位', 'Adjust reasoning effort level'), group: 'native', needs_args: true },
      { cmd: '/clear', args_hint: '', desc: L('清空当前对话上下文', 'Clear current conversation context'), group: 'native' },
      { cmd: '/help', args_hint: '', desc: L('查看 AGY 命令帮助', 'Show AGY command help'), group: 'native' },
    ],
    claude: [
      { cmd: '/compact', args_hint: '', desc: L('压缩当前会话上下文', 'Compact conversation context'), group: 'native' },
      { cmd: '/clear', args_hint: '', desc: L('清空当前对话历史', 'Clear conversation history'), group: 'native' },
      { cmd: '/plan', args_hint: L('<目标>', '<goal>'), desc: L('进入规划模式输出方案', 'Enter plan mode to design solution'), group: 'native', needs_args: true },
      { cmd: '/review', args_hint: L('<范围>', '<scope>'), desc: L('执行代码变更评审', 'Run code change review'), group: 'native', needs_args: true },
      { cmd: '/model', args_hint: L('<模型>', '<model>'), desc: L('切换 Claude 模型', 'Switch Claude model'), group: 'native', needs_args: true },
      { cmd: '/effort', args_hint: L('<档位>', '<effort>'), desc: L('调整推理思考强度', 'Adjust reasoning effort'), group: 'native', needs_args: true },
      { cmd: '/context', args_hint: '', desc: L('查看上下文占用明细', 'Inspect context usage breakdown'), group: 'native' },
      { cmd: '/cost', args_hint: '', desc: L('查看本次会话 Token 消耗', 'Show token usage for this session'), group: 'native' },
      { cmd: '/agents', args_hint: '', desc: L('管理子代理配置', 'Manage subagent configurations'), group: 'native' },
      { cmd: '/mcp', args_hint: '', desc: L('查看 MCP 服务状态', 'Inspect MCP server status'), group: 'native' },
      { cmd: '/help', args_hint: '', desc: L('查看 Claude Code 帮助', 'Show Claude Code help'), group: 'native' },
    ],
    codex: [
      { cmd: '/compact', args_hint: '', desc: L('压缩上下文释放窗口', 'Compact context to free window space'), group: 'native' },
      { cmd: '/new', args_hint: '', desc: L('开启全新对话线程', 'Start a fresh conversation thread'), group: 'native' },
      { cmd: '/review', args_hint: L('<范围>', '<scope>'), desc: L('评审当前分支改动', 'Review current branch changes'), group: 'native', needs_args: true },
      { cmd: '/diff', args_hint: '', desc: L('查看工作区 Git Diff', 'View working tree Git diff'), group: 'native' },
      { cmd: '/model', args_hint: L('<模型>', '<model>'), desc: L('切换 Codex 模型', 'Switch Codex model'), group: 'native', needs_args: true },
      { cmd: '/status', args_hint: '', desc: L('查看会话与上下文状态', 'View session and context status'), group: 'native' },
      { cmd: '/permissions', args_hint: '', desc: L('查看或调整审批权限', 'Inspect or change approval permissions'), group: 'native' },
      { cmd: '/mcp', args_hint: '', desc: L('查看 MCP 工具连接', 'Inspect MCP tool connections'), group: 'native' },
      { cmd: '/help', args_hint: '', desc: L('查看 Codex 命令帮助', 'Show Codex command help'), group: 'native' },
    ],
  };
  return catalogs[agent] || catalogs.claude;
}
export const FALLBACK_MODELS = {
  claude: {
    default: 'fable',
    default_effort: 'high',
    efforts: ['low', 'medium', 'high', 'max'],
    models: [
      { id: 'fable', label: 'fable (default)', efforts: ['low', 'medium', 'high', 'max'] },
      { id: 'opus', label: 'opus', efforts: ['low', 'medium', 'high', 'max'] },
      { id: 'sonnet', label: 'sonnet', efforts: ['low', 'medium', 'high', 'max'] },
      { id: 'claude-opus-5-5', label: 'claude-opus-5-5', efforts: ['low', 'medium', 'high', 'max'] },
      { id: 'claude-sonnet-5-5', label: 'claude-sonnet-5-5', efforts: ['low', 'medium', 'high', 'max'] },
    ],
  },
  codex: {
    default: 'gpt-6.2-sol',
    default_effort: 'high',
    efforts: ['low', 'medium', 'high', 'xhigh'],
    models: [
      { id: 'gpt-6.2-sol', label: 'gpt-6.2-sol', efforts: ['low', 'medium', 'high', 'xhigh'] },
      { id: 'gpt-5-codex', label: 'gpt-5-codex', efforts: ['low', 'medium', 'high', 'xhigh'] },
      { id: 'gpt-5', label: 'gpt-5', efforts: ['low', 'medium', 'high', 'xhigh'] },
    ],
  },
  agy: {
    default: 'gemini-4-neon',
    default_effort: 'high',
    efforts: ['low', 'medium', 'high', 'xhigh'],
    models: [
      { id: 'gemini-4-neon', label: 'Gemini 4 Neon', efforts: ['low', 'medium', 'high', 'xhigh'] },
      { id: 'gemini-4-carbon', label: 'Gemini 4 Carbon', efforts: ['low', 'medium', 'high'] },
      { id: 'gemini-4-argon', label: 'Gemini 4 Argon', efforts: ['low', 'medium', 'high'] },
      { id: 'gemini-4-hydrogen', label: 'Gemini 4 Hydrogen', efforts: ['low', 'medium', 'high', 'xhigh'] },
      { id: 'claude-opus-5-5', label: 'Claude Opus 5.5', efforts: ['low', 'medium', 'high'] },
      { id: 'Orchestrator (Neon)', label: 'Orchestrator (Neon)', efforts: [] },
      { id: 'Orchestrator (Carbon)', label: 'Orchestrator (Carbon)', efforts: [] },
    ],
  },
};
