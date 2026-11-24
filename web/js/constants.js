// starfane web console — shared constants and built-in fallback catalogs
export const VIEWS = ['workspace', 'inbox', 'pipelines', 'reviews'];
export const VIEW_LABEL = { workspace: '会话对话', inbox: '待处理', pipelines: '流水线', reviews: '代码评审' };
export const AGENTS = ['claude', 'codex', 'agy'];
export const AGENT_DISPLAY = { claude: 'Claude', codex: 'Codex', agy: 'AGY', shell: 'Shell' };
export const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];
export const DEPTHS = ['D0', 'D1', 'D2', 'D3'];
export const SEVERITIES = ['blocker', 'major', 'minor', 'nit'];
export const SSE_TYPES = ['event', 'session', 'host', 'pipeline', 'artifact', 'review', 'delivery', 'attention', 'resync'];
export const STAGE_LABEL = {
  plan: 'Plan',
  implement: 'Implement',
  test_review: 'Test & Review',
  report: 'Report',
  triage: '快筛 Triage',
  deep: '深挖 Deep',
};
export const PLAYBOOK_STAGES = {
  feature: ['plan', 'implement', 'test_review', 'report'],
  debug: ['triage', 'deep'],
};
export const STATE_LABEL = {
  working: '运行中',
  starting: '启动中',
  idle: '空闲',
  needs_permission: '等待审批',
  needs_input: '等待输入',
  error: '异常',
  interrupted: '已中断',
  ended: '已结束',
  unknown: '未知',
  running: '进行中',
  pending: '待开始',
  awaiting_human: '待人工确认',
  awaiting_binding_confirm: '待确认绑定',
  blocked: '已阻塞',
  done: '已完成',
  cancelled: '已取消',
  failed: '已失败',
  skipped: '已跳过',
};
export const STATE_PRIO = {
  needs_permission: 0,
  needs_input: 0,
  error: 1,
  working: 2,
  starting: 3,
  idle: 4,
  interrupted: 4,
  unknown: 5,
  ended: 6,
};
export const LEVEL_LABEL = {
  l0_inbox: 'L0 收件箱',
  l1_codex_queue: 'L1 Codex 队列',
  l2_inject: 'L2 直接注入',
  l4_spawn: 'L4 新建窗口',
};
export const TOKEN_KEY = 'starfane_token';
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
export const PASTE_CHAR_LIMIT = 400;
export const PASTE_LINE_LIMIT = 6;
export const IMG_MAX_BYTES = 10 * 1024 * 1024;
export const EFFORT_LABEL = {
  low: 'low · 低',
  medium: 'medium · 中',
  high: 'high · 高',
  xhigh: 'xhigh · 超高',
  max: 'max · 最大',
};
export const STARFANE_SLASH = [
  { cmd: '/feature', args_hint: '', desc: '切换为新功能协作模式', group: 'starfane', action: 'mode_feature' },
  { cmd: '/debug', args_hint: '', desc: '切换为两速查错模式', group: 'starfane', action: 'mode_debug' },
  { cmd: '/mute', args_hint: '', desc: '打开飞书通知静音设置', group: 'starfane', action: 'open_mute' },
  { cmd: '/terminal', args_hint: '', desc: '切换阅读视图 / 原始终端', group: 'starfane', action: 'toggle_terminal' },
  { cmd: '/drawer', args_hint: '', desc: '展开当前会话详情抽屉', group: 'starfane', action: 'open_drawer' },
];
export const FALLBACK_SLASH = {
  agy: [
    { cmd: '/swarm', args_hint: '<任务>', desc: '群体多 Agent 并行协作', group: 'native', needs_args: true },
    { cmd: '/boost', args_hint: '<任务>', desc: '高算力深度思考执行', group: 'native', needs_args: true },
    { cmd: '/teamwork', args_hint: '<任务>', desc: '多角色分工协同推进', group: 'native', needs_args: true },
    { cmd: '/plan', args_hint: '<目标>', desc: '先出实施计划再动手', group: 'native', needs_args: true },
    { cmd: '/compact', args_hint: '', desc: '压缩当前上下文窗口', group: 'native' },
    { cmd: '/model', args_hint: '<模型>', desc: '切换当前会话模型', group: 'native', needs_args: true },
    { cmd: '/effort', args_hint: '<档位>', desc: '调整思考强度档位', group: 'native', needs_args: true },
    { cmd: '/clear', args_hint: '', desc: '清空当前对话上下文', group: 'native' },
    { cmd: '/help', args_hint: '', desc: '查看 AGY 命令帮助', group: 'native' },
  ],
  claude: [
    { cmd: '/compact', args_hint: '', desc: '压缩当前会话上下文', group: 'native' },
    { cmd: '/clear', args_hint: '', desc: '清空当前对话历史', group: 'native' },
    { cmd: '/plan', args_hint: '<目标>', desc: '进入规划模式输出方案', group: 'native', needs_args: true },
    { cmd: '/review', args_hint: '<范围>', desc: '执行代码变更评审', group: 'native', needs_args: true },
    { cmd: '/model', args_hint: '<模型>', desc: '切换 Claude 模型', group: 'native', needs_args: true },
    { cmd: '/effort', args_hint: '<档位>', desc: '调整推理思考强度', group: 'native', needs_args: true },
    { cmd: '/context', args_hint: '', desc: '查看上下文占用明细', group: 'native' },
    { cmd: '/cost', args_hint: '', desc: '查看本次会话 Token 消耗', group: 'native' },
    { cmd: '/agents', args_hint: '', desc: '管理子代理配置', group: 'native' },
    { cmd: '/mcp', args_hint: '', desc: '查看 MCP 服务状态', group: 'native' },
    { cmd: '/help', args_hint: '', desc: '查看 Claude Code 帮助', group: 'native' },
  ],
  codex: [
    { cmd: '/compact', args_hint: '', desc: '压缩上下文释放窗口', group: 'native' },
    { cmd: '/new', args_hint: '', desc: '开启全新对话线程', group: 'native' },
    { cmd: '/review', args_hint: '<范围>', desc: '评审当前分支改动', group: 'native', needs_args: true },
    { cmd: '/diff', args_hint: '', desc: '查看工作区 Git Diff', group: 'native' },
    { cmd: '/model', args_hint: '<模型>', desc: '切换 Codex 模型', group: 'native', needs_args: true },
    { cmd: '/status', args_hint: '', desc: '查看会话与上下文状态', group: 'native' },
    { cmd: '/permissions', args_hint: '', desc: '查看或调整审批权限', group: 'native' },
    { cmd: '/mcp', args_hint: '', desc: '查看 MCP 工具连接', group: 'native' },
    { cmd: '/help', args_hint: '', desc: '查看 Codex 命令帮助', group: 'native' },
  ],
};
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
