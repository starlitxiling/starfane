// baton web console — offline ES module
'use strict';
const VIEWS = ['workspace', 'inbox', 'pipelines', 'reviews'];
const VIEW_LABEL = { workspace: '会话对话', inbox: '待处理', pipelines: '流水线', reviews: '代码评审' };
const AGENTS = ['claude', 'codex', 'agy'];
const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];
const DEPTHS = ['D0', 'D1', 'D2', 'D3'];
const SEVERITIES = ['blocker', 'major', 'minor', 'nit'];
const SSE_TYPES = ['event', 'session', 'pipeline', 'artifact', 'review', 'delivery', 'attention', 'resync'];
const STAGE_LABEL = { plan: 'Plan', implement: 'Implement', test_review: 'Test & Review', report: 'Report', triage: '快筛 Triage', deep: '深挖 Deep' };
const PLAYBOOK_STAGES = { feature: ['plan', 'implement', 'test_review', 'report'], debug: ['triage', 'deep'] };
const STATE_LABEL = {
working: '运行中', starting: '启动中', idle: '空闲', needs_permission: '等待审批',
needs_input: '等待输入', error: '异常', interrupted: '已中断', ended: '已结束', unknown: '未知',
running: '进行中', pending: '待开始', awaiting_human: '待人工确认', awaiting_binding_confirm: '待确认绑定',
blocked: '已阻塞', done: '已完成', cancelled: '已取消', failed: '已失败', skipped: '已跳过',
};
const STATE_PRIO = { needs_permission: 0, needs_input: 0, error: 1, working: 2, starting: 3, idle: 4, interrupted: 4, unknown: 5, ended: 6 };
const LEVEL_LABEL = { l0_inbox: 'L0 收件箱', l1_codex_queue: 'L1 Codex 队列', l2_inject: 'L2 直接注入', l4_spawn: 'L4 新建窗口' };
const TOKEN_KEY = 'baton_token';
const REQUEST_TIMEOUT_MS = 15000;
const HEALTH_POLL_MS = 8000;
const PREVIEW_POLL_MS = 3000;
const PREVIEW_LINES = 100;
const TRANSCRIPT_LIMIT = 24;
const CODE_COLLAPSE_LINES = 24;
const TICK_MS = 15000;
const PARSE_DEBOUNCE_MS = 380;
const MAX_ACTIVITY = 80;
const MAX_BACKOFF_MS = 15000;
const TOAST_MS = 4200;
const COLLAPSED_KEY = 'baton_collapsed_groups';
const GROUPS_INIT_KEY = 'baton_groups_init';
const SPAWN_PREFS_KEY = 'baton_spawn_prefs';
const OUTLINE_COLLAPSED_KEY = 'baton_outline_collapsed';
const PASTE_CHAR_LIMIT = 400;
const PASTE_LINE_LIMIT = 6;
const EFFORT_LABEL = { low: 'low · 低', medium: 'medium · 中', high: 'high · 高', xhigh: 'xhigh · 超高', max: 'max · 最大' };
const BATON_SLASH = [
{ cmd: '/feature', args_hint: '', desc: '切换为新功能协作模式', group: 'baton', action: 'mode_feature' },
{ cmd: '/debug', args_hint: '', desc: '切换为两速查错模式', group: 'baton', action: 'mode_debug' },
{ cmd: '/mute', args_hint: '', desc: '打开飞书通知静音设置', group: 'baton', action: 'open_mute' },
{ cmd: '/terminal', args_hint: '', desc: '切换阅读视图 / 原始终端', group: 'baton', action: 'toggle_terminal' },
{ cmd: '/drawer', args_hint: '', desc: '展开当前会话详情抽屉', group: 'baton', action: 'open_drawer' },
];
const FALLBACK_SLASH = {
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
const FALLBACK_MODELS = {
claude: {
default: 'fable', default_effort: 'high', efforts: ['low', 'medium', 'high', 'max'],
models: [
{ id: 'fable', label: 'fable (default)', efforts: ['low', 'medium', 'high', 'max'] },
{ id: 'opus', label: 'opus', efforts: ['low', 'medium', 'high', 'max'] },
{ id: 'sonnet', label: 'sonnet', efforts: ['low', 'medium', 'high', 'max'] },
{ id: 'claude-opus-5-5', label: 'claude-opus-5-5', efforts: ['low', 'medium', 'high', 'max'] },
{ id: 'claude-sonnet-5-5', label: 'claude-sonnet-5-5', efforts: ['low', 'medium', 'high', 'max'] },
],
},
codex: {
default: 'gpt-6.2-sol', default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'],
models: [
{ id: 'gpt-6.2-sol', label: 'gpt-6.2-sol', efforts: ['low', 'medium', 'high', 'xhigh'] },
{ id: 'gpt-5-codex', label: 'gpt-5-codex', efforts: ['low', 'medium', 'high', 'xhigh'] },
{ id: 'gpt-5', label: 'gpt-5', efforts: ['low', 'medium', 'high', 'xhigh'] },
],
},
agy: {
default: 'gemini-4-neon', default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'],
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
function loadSpawnPrefs() {
try {
const o = JSON.parse(localStorage.getItem(SPAWN_PREFS_KEY) || '{}');
return { last_agent: AGENTS.includes(o.last_agent) ? o.last_agent : 'claude', by_agent: o.by_agent && typeof o.by_agent === 'object' ? o.by_agent : {} };
} catch { return { last_agent: 'claude', by_agent: {} }; }
}
function saveSpawnPrefs(agent, patch) {
try {
const p = loadSpawnPrefs();
p.last_agent = agent;
p.by_agent[agent] = { ...(p.by_agent[agent] || {}), ...patch };
localStorage.setItem(SPAWN_PREFS_KEY, JSON.stringify(p));
} catch { /* ignore */ }
}
const NON_TMUX_GROUP = '非 tmux / 后台';
const NON_TMUX_OPEN_KEY = 'baton_nontmux_open';
const IMG_MAX_BYTES = 10 * 1024 * 1024;
function loadCollapsedGroups() {
try {
const raw = localStorage.getItem(COLLAPSED_KEY);
const set = raw ? new Set(JSON.parse(raw)) : new Set();
if (localStorage.getItem(NON_TMUX_OPEN_KEY) !== '1') set.add(NON_TMUX_GROUP);
return set;
} catch { return new Set([NON_TMUX_GROUP]); }
}
function saveCollapsedGroups() {
try {
localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...UI.collapsedGroups]));
localStorage.setItem(GROUPS_INIT_KEY, '1');
} catch { /* ignore */ }
}
const S = {
conn: 'connecting', healthFails: 0, version: '', uptimeMs: null, lastSeq: 0, lastLoad: 0,
loaded: false, loadError: null, authEnabled: true, authNeeded: false, retry: 0, es: null, reconnectTimer: 0,
sessions: new Map(), pipelines: new Map(), attention: [], models: null,
slashCmds: new Map(), slashFetching: new Set(),
reviews: new Map(), reviewsState: 'idle', reviewsErr: null,
precision: null, precisionState: 'idle', precisionErr: null,
artifacts: new Map(), handoffs: new Map(), previews: new Map(), transcripts: new Map(),
activity: [], muteUntil: Number(localStorage.getItem('baton_mute_until') || 0),
};
const UI = {
view: 'workspace', arg: null, modal: null, drawer: null,
selSession: null, composerMode: 'direct', outputMode: 'reading', autoRefreshPreview: true, autoConfirm: true,
showEnded: false, agentFilter: '', pipeFilter: 'all', revTab: 'reports', precAgent: '',
selStage: {}, parsePreviews: {}, drafts: new Map(), attachments: new Map(), busy: new Set(), open: new Set(),
slash: { open: false, start: 0, end: 0, query: '', items: [], idx: 0 },
collapsedGroups: loadCollapsedGroups(), groupsInited: !!localStorage.getItem(GROUPS_INIT_KEY),
outlineCollapsed: localStorage.getItem(OUTLINE_COLLAPSED_KEY) === '1',
jumpLockUntil: 0,
muteMenu: false, muteCustomVal: '45', muteCustomUnit: 'm', muteTimeVal: '',
};
let attachSeq = 0;
const trReqSeq = new Map();
const pvReqSeq = new Map();
function hashStr(str) {
let h = 2166136261;
const s = String(str || '');
for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
return (h >>> 0).toString(36);
}
const $ = (s, r = document) => r.querySelector(s);
function append(el, kids) {
for (const k of kids) {
if (k == null || k === false) continue;
if (Array.isArray(k)) append(el, k);
else if (k instanceof Node) el.appendChild(k);
else el.appendChild(document.createTextNode(String(k)));
}
}
function h(tag, props, ...kids) {
const el = document.createElement(tag);
let value;
if (props) {
for (const [k, v] of Object.entries(props)) {
if (v == null || v === false) continue;
if (k === 'class') el.className = v;
else if (k === 'value') value = v;
else if (k.length > 2 && k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
else if (v === true) el.setAttribute(k, '');
else el.setAttribute(k, v);
}
}
append(el, kids);
if (value !== undefined) el.value = value;
return el;
}
const span = (cls, ...kids) => h('span', { class: cls }, ...kids);
const enc = encodeURIComponent;
function btn(label, { cls = '', onclick, disabled, title, busyKey } = {}) {
const isBusy = busyKey && UI.busy.has(busyKey);
return h('button', { type: 'button', class: `btn ${cls}${isBusy ? ' is-busy' : ''}`, onclick, disabled: disabled || isBusy, title }, label);
}
function field(key, attrs = {}, def = '') {
const { tag = 'input', ...rest } = attrs;
const el = h(tag, { ...rest, 'data-draft': key });
el.value = UI.drafts.has(key) ? UI.drafts.get(key) : def;
el.addEventListener('input', () => UI.drafts.set(key, el.value));
return el;
}
function selectField(key, options, def = '', attrs = {}) {
const cur = UI.drafts.has(key) ? UI.drafts.get(key) : def;
const el = h('select', { class: 'inp', ...attrs, 'data-draft': key },
options.map(([v, l]) => h('option', { value: v, selected: v === cur }, l)));
el.value = cur;
el.addEventListener('change', () => UI.drafts.set(key, el.value));
return el;
}
const draft = (key, def = '') => (UI.drafts.has(key) ? UI.drafts.get(key) : def);
const base = (p) => (p ? String(p).replace(/\/+$/, '').split('/').pop() || p : '—');
const pct = (x) => (typeof x === 'number' ? `${Math.round(x * 100)}%` : '—');
const stateText = (s) => STATE_LABEL[s] || String(s ?? '').replace(/_/g, ' ');
const stageName = (k) => STAGE_LABEL[k] || String(k ?? '').replace(/_/g, ' ');
const short = (s, n = 8) => (s ? String(s).slice(0, n) : '—');
function dur(ms) {
const s = Math.max(0, Math.floor((ms || 0) / 1000));
if (s < 60) return `${s}s`;
const m = Math.floor(s / 60);
if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`;
const hr = Math.floor(m / 60);
if (hr < 48) return `${hr}h${String(m % 60).padStart(2, '0')}m`;
return `${Math.floor(hr / 24)}d`;
}
function ago(ms) {
if (!ms) return '—';
const d = Math.max(0, Date.now() - ms);
return d < 5000 ? '刚刚' : `${dur(d)}前`;
}
function clock(ms) { return ms ? new Date(ms).toLocaleTimeString('zh-CN', { hour12: false }) : ''; }
function clockHM(ms) { return ms ? new Date(ms).toLocaleTimeString('zh-CN', { hour12: false, hour: '2-digit', minute: '2-digit' }) : ''; }
function fmtBytes(n) {
if (n < 1024) return `${n} B`;
if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
function muteRemain(until) {
const rem = Math.max(0, (until || 0) - Date.now());
const mins = Math.ceil(rem / 60000);
if (mins < 1) return '<1m';
if (mins < 60) return `${mins}m`;
const h = Math.floor(mins / 60); const m = mins % 60;
if (h < 24) return m ? `${h}h${m}m` : `${h}h`;
const d = Math.floor(h / 24); const rh = h % 24;
return rh ? `${d}d${rh}h` : `${d}d`;
}
function muteUntilLabel(ms) {
if (!ms) return '';
const now = new Date(); const dt = new Date(ms);
const sameDay = now.getFullYear() === dt.getFullYear() && now.getMonth() === dt.getMonth() && now.getDate() === dt.getDate();
const tom = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1);
const isTom = tom.getFullYear() === dt.getFullYear() && tom.getMonth() === dt.getMonth() && tom.getDate() === dt.getDate();
const hm = clockHM(ms);
if (sameDay) return `今天 ${hm}`;
if (isTom) return `明天 ${hm}`;
return `${dt.getMonth() + 1}/${dt.getDate()} ${hm}`;
}
function tomorrowAt9Ms() {
const d = new Date();
d.setDate(d.getDate() + 1);
d.setHours(9, 0, 0, 0);
return d.getTime();
}
function timeStrToUntilMs(hhmm) {
const m = String(hhmm || '').match(/^(\d{2}):(\d{2})$/);
if (!m) return null;
const hh = Number(m[1]); const mm = Number(m[2]);
if (hh > 23 || mm > 59) return null;
const d = new Date();
d.setHours(hh, mm, 0, 0);
if (d.getTime() <= Date.now() + 30000) d.setDate(d.getDate() + 1);
return d.getTime();
}
function defaultMuteTimeStr() {
const d = new Date(Date.now() + 3600000);
d.setMinutes(0, 0, 0);
if (d.getTime() <= Date.now() + 900000) d.setHours(d.getHours() + 1);
return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}
function shorthand(b) {
if (!b) return '默认';
if (b.same_as) return `同 ${stageName(b.same_as)}`;
let s = b.agent || 'auto';
if (b.model) s += `:${b.model}`;
if (b.effort) s += `@${b.effort}`;
if (b.native) s += ` /${b.native}`;
return s;
}
function isTmuxSession(s) {
const hasPane = Boolean(s?.pane && String(s.pane).trim());
if (s?.state !== 'ended') return hasPane;
return hasPane || Boolean(s?.tmux_target && String(s.tmux_target).trim());
}
function tmuxGroupOf(s) {
if (s?.state !== 'ended' && !(s?.pane && String(s.pane).trim())) return NON_TMUX_GROUP;
if (s.tmux_target && String(s.tmux_target).includes(':')) return String(s.tmux_target).split(':')[0];
if (s.tmux_target && String(s.tmux_target).trim()) return String(s.tmux_target).trim();
if (s.pane && String(s.pane).trim()) {
if (s.tmux_label && String(s.tmux_label).includes('-')) return String(s.tmux_label).split('-')[0];
return 'tmux';
}
return NON_TMUX_GROUP;
}
function modelBaseOf(model, effort) {
if (!model) return '';
if (effort && model.toLowerCase().endsWith(`-${effort.toLowerCase()}`)) return model.slice(0, -(effort.length + 1));
return model;
}
function modelEffortLabel(model, effort) {
const m = modelBaseOf(model, effort);
if (m && effort) return `${m} · ${effort}`;
return m || effort || '';
}
function sessionLabel(s) {
if (s.tmux_label) return s.tmux_label;
if (s.tmux_target) return s.tmux_target;
return `${s.agent} ${s.pane || base(s.git_root || s.cwd)}`.trim();
}
const isFin = (v) => typeof v === 'number' && Number.isFinite(v);
function fmtTokens(n) {
if (!isFin(n) || n < 0) return '—';
if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 1 : 2).replace(/\.0+$/, '')}M`;
if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 100_000 ? 0 : 1).replace(/\.0$/, '')}K`;
return String(Math.round(n));
}
function ctxPctOf(m) {
if (!m) return null;
if (isFin(m.context_used_pct)) return Math.max(0, Math.min(100, m.context_used_pct));
if (isFin(m.context_used_tokens) && isFin(m.context_window_tokens) && m.context_window_tokens > 0) {
return Math.max(0, Math.min(100, (m.context_used_tokens / m.context_window_tokens) * 100));
}
return null;
}
function tpsOf(m) {
if (!m || !isFin(m.tps) || m.tps <= 0) return null;
return m.tps;
}
function ctxTone(p) {
if (p == null) return 'ok';
if (p > 85) return 'high';
if (p >= 70) return 'warn';
return 'ok';
}
function metricsSrcLabel(src) {
return {
cc_switch: 'cc-switch 代理日志',
transcript: '对话日志 (transcript)',
status_line: '终端状态栏 (status_line)',
none: '暂无数据源',
}[src] || src || '未知';
}
function fmtMs(ms) {
if (!isFin(ms) || ms < 0) return '—';
return ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(2)}s`;
}
function ctxBar(pctVal, small = false) {
const tone = ctxTone(pctVal);
const w = pctVal == null ? 0 : Math.max(0, Math.min(100, pctVal));
return h('span', { class: `m-bar${small ? ' sm' : ''}${tone !== 'ok' ? ` m-${tone}` : ''}` }, h('i', { style: `width:${w.toFixed(1)}%` }));
}
function buildTelemetryStrip(s) {
const m = s?.metrics;
const pctVal = ctxPctOf(m);
const tpsVal = tpsOf(m);
const tone = ctxTone(pctVal);
const hasUsed = isFin(m?.context_used_tokens);
const hasWin = isFin(m?.context_window_tokens) && m.context_window_tokens > 0;
const hasCtx = pctVal != null || hasUsed;
if (!hasCtx && tpsVal == null) {
return h('div', { class: 'ws-telemetry', 'data-tel-sid': s?.session_id || '' },
span('ws-tel-empty', '遥测待采集'));
}
let ctxText = '';
if (hasUsed && hasWin && pctVal != null) ctxText = `上下文 ${fmtTokens(m.context_used_tokens)} / ${fmtTokens(m.context_window_tokens)} · ${Math.round(pctVal)}%`;
else if (pctVal != null) ctxText = `上下文 ${Math.round(pctVal)}%`;
else if (hasUsed) ctxText = `上下文 ${fmtTokens(m.context_used_tokens)}`;
const ctxTipParts = [];
if (m?.source && m.source !== 'none') ctxTipParts.push(`来源：${metricsSrcLabel(m.source)}`);
if (isFin(m?.total_input_tokens) || isFin(m?.total_output_tokens)) {
ctxTipParts.push(`累计输入 ${fmtTokens(m?.total_input_tokens)} · 输出 ${fmtTokens(m?.total_output_tokens)}`);
}
const tpsTipParts = [];
if (isFin(m?.ttft_ms)) tpsTipParts.push(`首字延迟 TTFT：${fmtMs(m.ttft_ms)}`);
if (isFin(m?.last_output_tokens) && isFin(m?.last_duration_ms)) {
tpsTipParts.push(`最近回合：${fmtTokens(m.last_output_tokens)} tok / ${fmtMs(m.last_duration_ms)}`);
}
if (m?.source && m.source !== 'none') tpsTipParts.push(`来源：${metricsSrcLabel(m.source)}`);
const ctxCls = tone === 'high' ? ' ws-tel-high' : tone === 'warn' ? ' ws-tel-warn' : '';
return h('div', { class: 'ws-telemetry', 'data-tel-sid': s?.session_id || '' },
hasCtx ? h('span', { class: `ws-tel-pill${ctxCls}`, title: ctxTipParts.join(' ｜ ') || '上下文容量' },
ctxText, pctVal != null ? ctxBar(pctVal, false) : null) : null,
tpsVal != null ? h('span', { class: 'ws-tel-pill', title: tpsTipParts.join(' ｜ ') || '输出速度' },
`${tpsVal.toFixed(1)} tok/s`) : null);
}
function errText(e) {
if (e instanceof ApiError) return e.code && e.code !== 'unavailable' ? `${e.message}（${e.code}）` : e.message;
return e?.message || String(e);
}
function deliveryText(r) {
if (!r) return '已发送';
const lvl = LEVEL_LABEL[r.level] || r.level || '';
const ack = r.ack === 'unverified_legacy' ? ' · 存量会话已写入' : '';
return `${lvl} · ${stateText(r.status)}${r.pane ? ` · ${r.pane}` : ''}${ack}`;
}
function warnText(w) {
switch (w.warning) {
case 'same_model_reviewer': return `Reviewer 与实现者使用同一模型（${w.model}），存在自审偏差风险`;
case 'native_agent_mismatch': return `${stageName(w.stage)}：原生模式 ${w.native} 与 agent ${w.agent} 不匹配`;
case 'unknown_model': return `${stageName(w.stage)}：${w.agent} 未知模型 “${w.model}”`;
case 'stage_not_in_playbook': return `阶段 ${stageName(w.stage)} 不属于 ${w.playbook}，已忽略`;
case 'unrecognized': return `未识别片段：“${w.text}”`;
default: return JSON.stringify(w);
}
}
class ApiError extends Error {
constructor(code, message, status) { super(message); this.code = code; this.status = status; }
}
function authHeaders() {
const t = localStorage.getItem(TOKEN_KEY);
return t ? { 'X-Baton-Token': t } : {};
}
async function api(method, path, body) {
const ctl = new AbortController();
const timer = setTimeout(() => ctl.abort(), REQUEST_TIMEOUT_MS);
let res;
try {
res = await fetch(path, {
method, credentials: 'same-origin', signal: ctl.signal,
headers: { Accept: 'application/json', ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}), ...authHeaders() },
body: body !== undefined ? JSON.stringify(body) : undefined,
});
} catch (e) {
throw new ApiError('unavailable', e.name === 'AbortError' ? '请求超时' : `无法连接本地 daemon（${e.message}）`, 0);
} finally { clearTimeout(timer); }
let env = null;
try { env = await res.json(); } catch { env = null; }
if (res.status === 401 || (res.status === 403 && S.authEnabled)) {
if (S.authEnabled) { S.authNeeded = true; schedule(); }
throw new ApiError('unauthorized', env?.error?.message || '需要认证 token', res.status);
}
if (!res.ok || !env || env.ok === false) {
throw new ApiError(env?.error?.code || 'internal', env?.error?.message || `HTTP ${res.status}`, res.status);
}
return env.data !== undefined ? env.data : env;
}
const qs = (o) => {
const p = new URLSearchParams();
for (const [k, v] of Object.entries(o)) if (v !== '' && v != null) p.set(k, v);
const s = p.toString();
return s ? `?${s}` : '';
};
async function act(key, fn, ok) {
if (UI.busy.has(key)) return undefined;
UI.busy.add(key); schedule();
try {
const r = await fn();
if (ok) {
const m = typeof ok === 'function' ? ok(r) : ok;
if (m) toast(m.msg || m, m.kind || 'ok');
}
return r;
} catch (e) {
toast(errText(e), 'err');
return undefined;
} finally { UI.busy.delete(key); schedule(); }
}
function toast(msg, kind = 'ok') {
const box = $('#toasts');
const dotCls = kind === 'err' ? 'dot-error' : kind === 'warn' ? 'dot-needs_permission' : 'dot-done';
const el = h('div', { class: 'toast' }, h('i', { class: `dot ${dotCls}` }), h('span', null, msg));
box.appendChild(el);
while (box.children.length > 4) box.firstChild.remove();
setTimeout(() => el.remove(), kind === 'err' ? TOAST_MS * 2 : TOAST_MS);
}
function migrateSessionSelection(oldId, newId) {
if (!oldId || !newId || oldId === newId) return;
if (UI.selSession === oldId) UI.selSession = newId;
if (UI.arg === oldId) {
UI.arg = newId;
if (UI.view === 'workspace') history.replaceState(null, '', `#/workspace/${enc(newId)}`);
}
const dkOld = `ws:msg:${oldId}`; const dkNew = `ws:msg:${newId}`;
if (UI.drafts.has(dkOld) && !UI.drafts.has(dkNew)) UI.drafts.set(dkNew, UI.drafts.get(dkOld));
if (UI.attachments.has(oldId) && !UI.attachments.has(newId)) UI.attachments.set(newId, UI.attachments.get(oldId));
if (WS?.comp?.sid === oldId) {
WS.comp.sid = newId;
WS.comp.ta.dataset.draft = dkNew;
}
}
function upsertSession(s) {
if (!s || !s.session_id) return;
const prev = S.sessions.get(s.session_id);
if (!s.is_subagent && !s.discovered_only && s.pane && s.state !== 'ended') {
for (const [oldId, old] of S.sessions.entries()) {
if (oldId !== s.session_id && old.discovered_only && old.pane === s.pane) {
S.sessions.delete(oldId);
migrateSessionSelection(oldId, s.session_id);
}
}
}
if (s.state === 'ended' && (s.discovered_only || !UI.showEnded)) {
S.sessions.delete(s.session_id);
if (UI.selSession === s.session_id) {
const pane = s.pane || prev?.pane;
const repl = pane ? [...S.sessions.values()].find((x) => !x.is_subagent && x.state !== 'ended' && x.pane === pane) : null;
if (repl) {
migrateSessionSelection(s.session_id, repl.session_id);
loadSessionOutput(repl, { silent: false });
} else {
UI.selSession = null;
}
}
return;
}
S.sessions.set(s.session_id, s);
if (UI.selSession === s.session_id && s.state === 'ended' && !UI.showEnded) {
const pane = s.pane || prev?.pane;
const repl = pane ? [...S.sessions.values()].find((x) => !x.is_subagent && x.state !== 'ended' && x.session_id !== s.session_id && x.pane === pane) : null;
if (repl) {
migrateSessionSelection(s.session_id, repl.session_id);
loadSessionOutput(repl, { silent: false });
} else {
UI.selSession = null;
}
}
}
const upsertPipeline = (p) => { if (p && p.id) S.pipelines.set(p.id, p); return p; };
function sessionsDiffSig(map) {
return [...map.values()].map((s) => `${s.session_id}|${s.state}|${s.tmux_label || ''}|${s.tmux_target || ''}|${s.pane || ''}|${s.model || ''}|${s.effort || ''}|${s.last_message || ''}|${s.last_error || ''}|${s.subagent_count || 0}|${s.metrics?.context_used_pct ?? ''}|${s.metrics?.tps ?? ''}`).sort().join('\n');
}
let stateSyncBusy = false;
async function syncStateSilent() {
if (document.hidden || !S.loaded || S.authNeeded || stateSyncBusy) return;
stateSyncBusy = true;
try {
const st = await api('GET', '/api/state');
const prevSig = sessionsDiffSig(S.sessions);
const prevMute = S.muteUntil;
const prevAttn = S.attention.length;
const prevPipe = [...S.pipelines.values()].map((p) => `${p.id}:${p.status}:${p.updated_ms || 0}`).sort().join(',');
const nextSessions = new Map();
for (const s of st.sessions || []) {
if (!s || !s.session_id) continue;
if (s.state === 'ended' && !UI.showEnded) continue;
nextSessions.set(s.session_id, s);
}
for (const [id, old] of S.sessions.entries()) {
if (old.is_subagent && old.parent_session_id && nextSessions.has(old.parent_session_id) && !nextSessions.has(id)) {
nextSessions.set(id, old);
}
}
if (UI.selSession && (!nextSessions.has(UI.selSession) || (nextSessions.get(UI.selSession)?.state === 'ended' && !UI.showEnded))) {
const prevSel = S.sessions.get(UI.selSession);
const pane = prevSel?.pane;
const repl = pane ? [...nextSessions.values()].find((x) => !x.is_subagent && x.state !== 'ended' && x.pane === pane) : null;
if (repl) migrateSessionSelection(UI.selSession, repl.session_id);
else UI.selSession = null;
}
const nextSig = sessionsDiffSig(nextSessions);
const nextPipelines = new Map();
for (const p of st.pipelines || []) if (p && p.id) nextPipelines.set(p.id, p);
const nextPipe = [...nextPipelines.values()].map((p) => `${p.id}:${p.status}:${p.updated_ms || 0}`).sort().join(',');
const nextAttn = st.attention || [];
const nextMute = typeof st.feishu_muted_until_ms === 'number' ? st.feishu_muted_until_ms : (st.feishu_muted_until_ms === null ? 0 : S.muteUntil);
const changed = prevSig !== nextSig || prevPipe !== nextPipe || prevAttn !== nextAttn.length || prevMute !== nextMute;
S.lastSeq = Math.max(S.lastSeq, st.event_seq || 0);
if (changed) {
S.sessions = nextSessions;
S.pipelines = nextPipelines;
S.attention = nextAttn;
if (nextMute !== prevMute) setMute(nextMute);
pickDefaultSession();
schedule();
const subQs = UI.showEnded ? { subagents: 1, ended: 1 } : { subagents: 1 };
if (UI.showEnded || [...nextSessions.values()].some((s) => (s.subagent_count || 0) > 0)) {
api('GET', `/api/sessions${qs(subQs)}`)
.then((list) => {
const before = sessionsDiffSig(S.sessions);
(list || []).forEach(upsertSession);
if (sessionsDiffSig(S.sessions) !== before) schedule();
})
.catch(() => {});
}
}
} catch { /* silent fallback */ }
finally { stateSyncBusy = false; }
}
function pickDefaultSession() {
if (UI.view === 'workspace' && UI.arg && S.sessions.has(UI.arg)) {
const target = S.sessions.get(UI.arg);
if (target && (UI.showEnded || target.state !== 'ended')) {
if (UI.selSession !== target.session_id) {
UI.selSession = target.session_id;
loadSessionOutput(target, { silent: false });
}
return;
}
}
const cur = UI.selSession ? S.sessions.get(UI.selSession) : null;
if (cur && (UI.showEnded || cur.state !== 'ended')) return;
const live = rootSessions();
const first = live.find(isTmuxSession) || live[0];
if (first) {
UI.selSession = first.session_id;
loadSessionOutput(first, { silent: false });
} else {
UI.selSession = null;
}
}
function sessionNeedsLiveTail(s, tr) {
if (!s) return false;
if (['error', 'needs_permission', 'needs_input'].includes(s.state)) return true;
if (!tr || tr.state !== 'ok' || tr.source !== 'transcript' || !tr.messages.length) return true;
return tr.messages[tr.messages.length - 1]?.role === 'user';
}
function loadSessionOutput(s, { silent = false, forcePreview = false } = {}) {
if (!s) return;
loadTranscript(s.session_id, { silent });
if (!s.pane) return;
if (silent && !forcePreview && UI.outputMode !== 'raw') {
const tr = S.transcripts.get(s.session_id);
const tailOpen = UI.open.has(`tail:${s.session_id}`) || Boolean($('details.live-tail')?.open);
if (!sessionNeedsLiveTail(s, tr) && !tailOpen) {
return;
}
}
loadPreview(s.pane, { lines: PREVIEW_LINES, silent });
}
async function loadAll() {
S.lastLoad = Date.now();
try {
const st = await api('GET', '/api/state');
S.version = st.version || S.version;
S.lastSeq = st.event_seq || 0;
S.sessions = new Map(); S.pipelines = new Map();
(st.sessions || []).forEach(upsertSession);
(st.pipelines || []).forEach(upsertPipeline);
S.attention = st.attention || [];
if (typeof st.feishu_muted_until_ms === 'number') setMute(st.feishu_muted_until_ms);
else if (st.feishu_muted_until_ms === null) setMute(0);
S.loaded = true; S.loadError = null; S.authNeeded = false;
pickDefaultSession();
const subQs = UI.showEnded ? { subagents: 1, ended: 1 } : { subagents: 1 };
api('GET', `/api/sessions${qs(subQs)}`)
.then((list) => { (list || []).forEach(upsertSession); pickDefaultSession(); schedule(); })
.catch((e) => console.warn('sessions?subagents=1 failed', e));
} catch (e) {
S.loadError = errText(e);
}
schedule();
}
async function loadTranscript(sessionId, { silent = false } = {}) {
if (!sessionId) return;
const prev = S.transcripts.get(sessionId);
if (prev?.unsupported) return;
const seq = (trReqSeq.get(sessionId) || 0) + 1;
trReqSeq.set(sessionId, seq);
if (!prev) {
S.transcripts.set(sessionId, { state: 'loading', source: 'none', messages: [], hash: '', updatedMs: 0 });
if (!silent) schedule();
}
try {
const data = await api('GET', `/api/sessions/${enc(sessionId)}/transcript${qs({ limit: TRANSCRIPT_LIMIT })}`);
if (trReqSeq.get(sessionId) !== seq) return;
const msgs = Array.isArray(data?.messages) ? data.messages : [];
const source = data?.source === 'transcript' && msgs.length > 0 ? 'transcript' : 'none';
const nextHash = hashStr(`${source}|${JSON.stringify(msgs)}`);
const changed = !prev || prev.state !== 'ok' || prev.hash !== nextHash;
S.transcripts.set(sessionId, {
state: 'ok', source, transcriptPath: data?.transcript_path || null,
messages: msgs, hash: nextHash, updatedMs: Date.now(), err: null,
});
if (data?.metrics && S.sessions.has(sessionId)) {
const cur = S.sessions.get(sessionId);
const prevM = JSON.stringify(cur.metrics || null);
const nextM = JSON.stringify(data.metrics);
if (prevM !== nextM) {
S.sessions.set(sessionId, { ...cur, metrics: data.metrics });
patchTelemetryStrip(sessionId);
}
}
if (changed) notifyOutputUpdated(sessionId);
else updateOutputClock(sessionId);
} catch (e) {
if (trReqSeq.get(sessionId) !== seq) return;
if (e instanceof ApiError && e.status === 405) {
S.transcripts.set(sessionId, { state: 'ok', source: 'none', messages: [], hash: 'none', updatedMs: Date.now(), unsupported: true });
if (!prev || prev.state === 'loading') notifyOutputUpdated(sessionId);
return;
}
const errMsg = errText(e);
const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
S.transcripts.set(sessionId, { ...(prev || { source: 'none', messages: [], hash: '' }), state: 'err', err: errMsg, updatedMs: Date.now() });
if (changed) notifyOutputUpdated(sessionId);
}
}
async function loadPreview(paneId, { lines = PREVIEW_LINES, silent = false } = {}) {
if (!paneId) return;
const key = `${paneId}:${lines}`;
const prev = S.previews.get(key);
const seq = (pvReqSeq.get(key) || 0) + 1;
pvReqSeq.set(key, seq);
if (!prev) {
S.previews.set(key, { state: 'loading', text: '', hash: '', updatedMs: 0 });
if (!silent) schedule();
}
try {
const data = await api('GET', `/api/panes/${enc(paneId)}/preview${qs({ lines })}`);
if (pvReqSeq.get(key) !== seq) return;
const text = data?.text ?? '';
const nextHash = hashStr(`${text}|${!!data?.in_mode}|${!!data?.dead}`);
const changed = !prev || prev.state !== 'ok' || prev.hash !== nextHash;
S.previews.set(key, { state: 'ok', updatedMs: Date.now(), data, text, hash: nextHash, err: null });
if (lines === PREVIEW_LINES) {
const cur = currentSession();
if (cur?.pane === paneId) {
if (changed) notifyOutputUpdated(cur.session_id);
else updateOutputClock(cur.session_id);
}
} else if (changed) {
schedule();
}
} catch (e) {
if (pvReqSeq.get(key) !== seq) return;
const errMsg = errText(e);
const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
S.previews.set(key, { ...(prev || { text: '', hash: '' }), state: 'err', err: errMsg, status: e?.status || 0, updatedMs: Date.now() });
if (changed) {
const cur = currentSession();
if (lines === PREVIEW_LINES && cur?.pane === paneId) notifyOutputUpdated(cur.session_id);
else schedule();
}
}
}
function updateOutputClock(sessionId) {
if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
const el = $('.term-updated');
if (el) el.textContent = `更新于 ${clock(Date.now())}`;
}
function patchTelemetryStrip(sessionId) {
if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
const s = S.sessions.get(sessionId);
const el = $('.ws-telemetry');
if (el && s && el.dataset.telSid === sessionId) {
el.replaceWith(buildTelemetryStrip(s));
}
}
function syncOutlineSpy(vp) {
if (!vp || Date.now() < UI.jumpLockUntil) return;
const items = [...document.querySelectorAll('.outline-item[data-turn-idx]')];
if (!items.length) return;
const vpRect = vp.getBoundingClientRect();
const anchorY = vpRect.top + Math.min(160, vpRect.height * 0.35);
let activeIdx = items[0].dataset.turnIdx;
for (const it of items) {
const el = document.getElementById(`turn-u-${it.dataset.turnIdx}`);
if (!el) continue;
if (el.getBoundingClientRect().top <= anchorY) activeIdx = it.dataset.turnIdx;
}
for (const it of items) it.classList.toggle('active', it.dataset.turnIdx === activeIdx);
}
function syncScrollLatestBtn(vp) {
if (!vp) return;
const b = $('.scroll-latest-btn');
if (b) {
const dist = vp.scrollHeight - vp.scrollTop - vp.clientHeight;
b.classList.toggle('show', dist > 120);
}
syncOutlineSpy(vp);
}
function notifyOutputUpdated(sessionId) {
if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) { schedule(); return; }
const vp = $('.term-viewport');
const s = currentSession();
if (!vp || !s || vp.dataset.sid !== s.session_id || vp.dataset.mode !== UI.outputMode) {
schedule();
return;
}
const built = buildOutputBody(s);
if (vp.dataset.sig === built.sig) {
updateOutputClock(sessionId);
return;
}
const atBottom = vp.scrollHeight - vp.scrollTop - vp.clientHeight < 44;
const prevTop = vp.scrollTop;
vp.dataset.sig = built.sig;
vp.className = `term-viewport${UI.outputMode === 'raw' ? ' raw-mode' : ''}`;
vp.replaceChildren(built.node);
vp.scrollTop = atBottom ? vp.scrollHeight : prevTop;
syncScrollLatestBtn(vp);
updateOutputClock(sessionId);
const pCnt = collectUserPrompts(s).length;
const rail = $('.outline-rail') || $('.outline-reopen-btn');
const prevCnt = rail ? Number(rail.dataset.cnt || 0) : 0;
if (pCnt !== prevCnt) schedule();
}
async function loadHandoffs(sessionId) {
if (!sessionId) return;
S.handoffs.set(sessionId, { state: 'loading', items: [] });
schedule();
try {
const items = await api('GET', `/api/handoffs${qs({ session: sessionId })}`);
S.handoffs.set(sessionId, { state: 'ok', items: items || [] });
} catch (e) {
S.handoffs.set(sessionId, { state: 'err', err: errText(e), items: [] });
}
schedule();
}
async function loadReviews() {
if (S.reviewsState === 'loading') return;
S.reviewsState = 'loading'; schedule();
try {
const list = await api('GET', '/api/reviews');
S.reviews = new Map((list || []).map((r) => [r.id, r]));
S.reviewsState = 'ok'; S.reviewsErr = null;
} catch (e) { S.reviewsState = 'err'; S.reviewsErr = errText(e); }
schedule();
}
async function refreshReview(id) {
try {
const r = await api('GET', `/api/reviews/${enc(id)}`);
if (r && r.id) S.reviews.set(r.id, r);
} catch (e) { console.warn('review refresh failed', id, e); }
schedule();
}
function ensureReview(id) {
if (!id || S.reviews.has(id) || UI.busy.has(`rv:${id}`)) return S.reviews.get(id);
UI.busy.add(`rv:${id}`);
refreshReview(id).finally(() => UI.busy.delete(`rv:${id}`));
return undefined;
}
async function loadPrecision() {
S.precisionState = 'loading'; schedule();
try {
S.precision = await api('GET', `/api/reviews/precision${qs({ agent: UI.precAgent })}`) || [];
S.precisionState = 'ok'; S.precisionErr = null;
} catch (e) { S.precisionState = 'err'; S.precisionErr = errText(e); }
schedule();
}
function artifact(id) {
let a = S.artifacts.get(id);
if (!a) {
a = { state: 'loading' };
S.artifacts.set(id, a);
api('GET', `/api/artifacts/${enc(id)}`)
.then((d) => { a.state = 'ok'; a.data = d; })
.catch((e) => { a.state = 'err'; a.err = errText(e); })
.finally(schedule);
}
return a;
}
function connState() { return S.healthFails >= 2 ? 'down' : S.conn; }
function connectSSE() {
clearTimeout(S.reconnectTimer);
if (S.es) S.es.close();
S.conn = 'connecting';
const es = new EventSource('/api/events');
S.es = es;
es.onopen = () => {
S.conn = 'live'; S.retry = 0;
if (Date.now() - S.lastLoad > 1500) loadAll();
schedule();
};
es.onerror = () => {
if (es.readyState === EventSource.CLOSED) {
S.conn = 'down';
const wait = Math.min(MAX_BACKOFF_MS, 1000 * 2 ** S.retry++);
S.reconnectTimer = setTimeout(connectSSE, wait);
} else S.conn = 'connecting';
schedule();
};
for (const t of SSE_TYPES) {
es.addEventListener(t, (ev) => {
let d;
try { d = JSON.parse(ev.data); } catch { return; }
handleEvent(t, d);
});
}
}
function handleEvent(type, d) {
if (d.seq) S.lastSeq = Math.max(S.lastSeq, d.seq);
switch (type) {
case 'session': {
const ns = d.session;
const prev = ns?.session_id ? S.sessions.get(ns.session_id) : null;
upsertSession(ns);
pickDefaultSession();
if (ns?.session_id === UI.selSession && ns.state !== 'ended') {
if (!prev || prev.state !== ns.state || prev.last_message !== ns.last_message || prev.last_error !== ns.last_error) {
loadSessionOutput(ns, { silent: true });
}
}
break;
}
case 'pipeline': upsertPipeline(d.pipeline); break;
case 'attention': S.attention = d.items || []; break;
case 'resync': syncStateSilent(); return;
case 'review': refreshReview(d.review_id); break;
case 'artifact': if (d.kind === 'review' && S.reviewsState === 'ok') loadReviews(); break;
case 'delivery': {
const r = d.report || {};
if (r.status === 'unconfirmed' || r.status === 'failed') toast(`投递${stateText(r.status)}：${deliveryText(r)}`, 'warn');
break;
}
case 'event': {
S.activity.unshift(d.event);
if (S.activity.length > MAX_ACTIVITY) S.activity.length = MAX_ACTIVITY;
break;
}
default: break;
}
schedule();
}
async function loadModels() {
try {
const data = await api('GET', '/api/models');
if (data && typeof data === 'object') { S.models = data; schedule(); }
} catch (e) { console.warn('GET /api/models fallback', e); }
}
async function loadSlashCommands(agent) {
const ag = AGENTS.includes(agent) ? agent : 'claude';
if (S.slashCmds.has(ag) || S.slashFetching.has(ag)) return;
S.slashFetching.add(ag);
try {
const data = await api('GET', `/api/slash-commands${qs({ agent: ag })}`);
const list = Array.isArray(data?.commands) ? data.commands : [];
if (list.length) {
const hasBaton = list.some((c) => c.group === 'baton');
S.slashCmds.set(ag, hasBaton ? list : [...list, ...BATON_SLASH]);
if (UI.slash.open && WS?.comp?.ta && WS.comp.agent === ag) updateSlashFromInput(WS.comp.ta, ag);
}
} catch { /* fallback to built-in slash list */ }
finally { S.slashFetching.delete(ag); }
}
function slashCommandsFor(agent) {
const ag = AGENTS.includes(agent) ? agent : 'claude';
if (S.slashCmds.has(ag)) return S.slashCmds.get(ag);
return [...(FALLBACK_SLASH[ag] || FALLBACK_SLASH.claude), ...BATON_SLASH];
}
function modelsForAgent(agent) {
const fb = FALLBACK_MODELS[agent] || { default: '', default_effort: 'high', efforts: [], models: [] };
const src = S.models?.[agent] || fb;
const def = src.default || fb.default || '';
const defEff = src.default_effort || fb.default_effort || 'high';
const agentEffs = Array.isArray(src.efforts) ? src.efforts : fb.efforts;
const list = [];
const seen = new Set();
const push = (id, label, efforts) => {
const k = String(id || '').trim();
if (!k || seen.has(k)) return;
seen.add(k);
const effs = Array.isArray(efforts) ? efforts : agentEffs;
list.push({ id: k, label: label || k, efforts: effs });
};
for (const item of src.models || []) {
if (typeof item === 'string') push(item, item, agentEffs);
else if (item?.id) push(item.id, item.label || item.id, item.efforts);
}
if (def && !seen.has(def)) list.unshift({ id: def, label: `${def}（默认）`, efforts: agentEffs });
return { default: def || list[0]?.id || '', default_effort: defEff, efforts: agentEffs, models: list };
}
function effortsForModel(mInfo, modelId) {
if (!modelId || modelId === '__custom__') return mInfo.efforts || [];
const found = (mInfo.models || []).find((m) => m.id === modelId);
return found && Array.isArray(found.efforts) ? found.efforts : (mInfo.efforts || []);
}
function openSpawnModal() {
loadModels();
const prefs = loadSpawnPrefs();
const ag = prefs.last_agent || 'claude';
UI.drafts.set('sp:agent', ag);
UI.drafts.delete('sp:dir');
hydrateSpawnAgent(ag, prefs);
UI.modal = 'spawn';
schedule();
}
function hydrateSpawnAgent(ag, prefs = loadSpawnPrefs()) {
const mInfo = modelsForAgent(ag);
const saved = prefs.by_agent?.[ag] || {};
const knownIds = new Set(mInfo.models.map((m) => m.id));
let pickedModel = saved.model || mInfo.default;
if (pickedModel && !knownIds.has(pickedModel) && saved.model) {
UI.drafts.set(`sp:modelSel:${ag}`, '__custom__');
UI.drafts.set('sp:modelCustom', saved.model);
} else {
if (!knownIds.has(pickedModel)) pickedModel = mInfo.default;
UI.drafts.set(`sp:modelSel:${ag}`, pickedModel);
}
const effs = effortsForModel(mInfo, UI.drafts.get(`sp:modelSel:${ag}`));
const wantEff = saved.effort || mInfo.default_effort;
const eff = effs.includes(wantEff) ? wantEff : (effs.includes(mInfo.default_effort) ? mInfo.default_effort : (effs[0] || ''));
UI.drafts.set(`sp:effort:${ag}`, eff);
}
async function pollHealth() {
try {
const r = await api('GET', '/api/health');
S.healthFails = 0; S.version = r.version || S.version; S.uptimeMs = r.uptime_ms ?? null;
if (typeof r.auth_enabled === 'boolean') {
S.authEnabled = r.auth_enabled;
if (!r.auth_enabled) S.authNeeded = false;
}
if (S.conn === 'down') connectSSE();
if (!S.loaded) loadAll();
} catch { S.healthFails++; }
schedule();
}
function pollActivePreview() {
if (document.hidden || !S.loaded || !UI.autoRefreshPreview || UI.view !== 'workspace') return;
const s = currentSession();
if (s && s.state !== 'ended') loadSessionOutput(s, { silent: true });
}
let renderTimer = 0;
let composing = false;
function schedule() {
if (renderTimer) return;
renderTimer = setTimeout(() => { renderTimer = 0; rerender(); }, 40);
}
document.addEventListener('compositionstart', () => { composing = true; });
document.addEventListener('compositionend', () => { composing = false; schedule(); });
function rerender() {
if (composing) return;
const a = document.activeElement;
const fk = a && a.dataset ? a.dataset.draft : null;
const sel = fk && a.selectionStart != null ? [a.selectionStart, a.selectionEnd] : null;
const termBox = $('.term-viewport');
const termScroll = termBox ? {
sid: termBox.dataset.sid,
mode: termBox.dataset.mode,
top: termBox.scrollTop,
atBottom: termBox.scrollHeight - termBox.scrollTop - termBox.clientHeight < 36,
} : null;
const drawerBody = $('.drawer-body');
const drawerTop = drawerBody ? drawerBody.scrollTop : 0;
const modalEl = $('#modal');
const modalTop = modalEl?.open ? modalEl.scrollTop : 0;
renderTop();
renderView();
renderDrawer();
renderModal();
const nextDrawer = $('.drawer-body');
if (nextDrawer && drawerTop) nextDrawer.scrollTop = drawerTop;
if (modalEl?.open && modalTop) modalEl.scrollTop = modalTop;
const alerts = alertCount();
document.title = `${alerts ? `(${alerts}) ` : ''}baton · 控制面`;
const nextTerm = $('.term-viewport');
if (nextTerm && nextTerm !== termBox) {
const samePane = termScroll && termScroll.sid === nextTerm.dataset.sid && termScroll.mode === nextTerm.dataset.mode;
nextTerm.scrollTop = (!samePane || termScroll.atBottom) ? nextTerm.scrollHeight : termScroll.top;
syncScrollLatestBtn(nextTerm);
}
if (fk) {
const n = document.querySelector(`[data-draft="${CSS.escape(fk)}"]`);
if (n && n !== document.activeElement) {
n.focus({ preventScroll: true });
if (sel) { try { n.setSelectionRange(sel[0], sel[1]); } catch { /* ignore */ } }
}
}
}
document.addEventListener('toggle', (e) => {
const d = e.target;
if (d instanceof HTMLDetailsElement && d.dataset.k) {
if (d.open) {
UI.open.add(d.dataset.k);
if (d.classList.contains('live-tail')) {
const cur = currentSession();
if (cur?.pane) loadPreview(cur.pane, { lines: PREVIEW_LINES, silent: true });
}
} else UI.open.delete(d.dataset.k);
}
}, true);
function go(view, arg) { location.hash = `#/${view}${arg ? `/${enc(arg)}` : ''}`; }
function openSession(id) {
UI.selSession = id;
UI.arg = id;
const s = S.sessions.get(id);
if (s) loadSessionOutput(s, { silent: false });
const wantHash = `#/workspace/${enc(id)}`;
if (UI.view !== 'workspace' || location.hash !== wantHash) go('workspace', id);
else rerender();
}
function route() {
let [v, a] = (location.hash.replace(/^#\/?/, '') || 'workspace').split('/');
if (v === 'sessions') v = 'workspace';
if (v === 'debug') { v = 'pipelines'; UI.pipeFilter = 'debug'; }
UI.view = VIEWS.includes(v) ? v : 'workspace';
UI.arg = a ? decodeURIComponent(a) : null;
if (UI.view === 'workspace' && UI.arg && S.sessions.has(UI.arg)) UI.selSession = UI.arg;
if (UI.view === 'reviews') {
if (S.reviewsState === 'idle' || S.reviewsState === 'err') loadReviews();
if (S.precisionState === 'idle') loadPrecision();
}
const s = currentSession();
if (UI.view === 'workspace' && s) {
if (!S.transcripts.has(s.session_id)) loadTranscript(s.session_id, { silent: false });
if (s.pane && !S.previews.has(`${s.pane}:${PREVIEW_LINES}`)) loadPreview(s.pane, { lines: PREVIEW_LINES, silent: false });
}
schedule();
}
window.addEventListener('hashchange', route);
document.addEventListener('keydown', (e) => {
if (e.key === 'Escape' && UI.muteMenu) { UI.muteMenu = false; schedule(); return; }
if (e.key === 'Escape' && UI.drawer) { closeDrawer(); return; }
if (e.metaKey || e.ctrlKey || e.altKey) return;
const t = e.target;
if (t && (/^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || t.isContentEditable)) return;
if (UI.modal || S.authNeeded) return;
const n = Number(e.key);
if (n >= 1 && n <= VIEWS.length) go(VIEWS[n - 1]);
else if (e.key === 'n') { openSpawnModal(); e.preventDefault(); }
});
function rootSessions() {
return [...S.sessions.values()]
.filter((s) => !s.is_subagent && (UI.showEnded || s.state !== 'ended'))
.sort((a, b) => (STATE_PRIO[a.state] ?? 5) - (STATE_PRIO[b.state] ?? 5) || (b.last_event_ms || 0) - (a.last_event_ms || 0));
}
function subagentsOf(parentId) {
return [...S.sessions.values()].filter((s) => s.is_subagent && s.parent_session_id === parentId);
}
function currentSession() {
if (UI.selSession && S.sessions.has(UI.selSession)) return S.sessions.get(UI.selSession);
return rootSessions()[0] || null;
}
const allPipelines = () => [...S.pipelines.values()].sort((a, b) => (b.updated_ms || 0) - (a.updated_ms || 0));
const isFinished = (p) => ['done', 'cancelled', 'failed'].includes(p.status);
const stageOf = (p, k) => (p.stages || []).find((s) => s.id === k);
function activeStage(p) {
const st = p.stages || [];
return st.find((s) => ['awaiting_human', 'blocked', 'failed'].includes(s.status))
|| st.find((s) => s.status === 'running')
|| st.find((s) => s.status === 'pending')
|| st[st.length - 1];
}
const lastArtifact = (st) => (st && st.artifacts && st.artifacts.length ? st.artifacts[st.artifacts.length - 1] : null);
function inboxItems() {
const items = []; const seen = new Set();
const add = (kind, o) => {
const key = `${kind}:${o.pid || o.sid || o.rid || ''}`;
if (seen.has(key)) return;
seen.add(key); items.push({ key, kind, ...o });
};
for (const a of S.attention) {
const kind = a.kind === 'blocked' && a.session_id ? 'session_alert' : a.kind;
add(kind, { pid: a.pipeline_id, sid: a.session_id, rid: a.review_id, title: a.title, since: a.since_ms });
}
for (const p of S.pipelines.values()) {
if (p.status === 'awaiting_binding_confirm') add('binding_confirm', { pid: p.id, title: `确认绑定：${p.goal}`, since: p.updated_ms });
const ah = (p.stages || []).find((s) => s.status === 'awaiting_human');
if (p.status === 'awaiting_human' && ah) {
if (ah.id === 'plan') add('plan_approval', { pid: p.id, title: `批准计划：${p.goal}`, since: p.updated_ms });
else if (ah.id === 'triage') add('triage_ready', { pid: p.id, title: `快筛结论待决：${p.goal}`, since: p.updated_ms });
else add('blocked', { pid: p.id, title: `${stageName(ah.id)} 需要人工处理：${p.goal}`, since: p.updated_ms });
}
if (p.status === 'blocked') add('blocked', { pid: p.id, title: `流程阻塞：${p.goal}`, since: p.updated_ms });
}
for (const s of S.sessions.values()) {
if (!s.is_subagent && ['needs_permission', 'needs_input', 'error'].includes(s.state)) {
add('session_alert', { sid: s.session_id, title: `${sessionLabel(s)} · ${stateText(s.state)}`, since: s.state_since_ms });
}
}
const prio = { session_alert: 0, binding_confirm: 1, plan_approval: 2, triage_ready: 3, blocked: 4, needs_human_finding: 5, delivery_unconfirmed: 6 };
return items.sort((a, b) => (prio[a.kind] ?? 9) - (prio[b.kind] ?? 9) || (a.since || 0) - (b.since || 0));
}
const alertCount = () => inboxItems().length;
const knownProjects = () => {
const set = new Set();
for (const s of S.sessions.values()) { if (s.git_root) set.add(s.git_root); else if (s.cwd) set.add(s.cwd); }
for (const p of S.pipelines.values()) if (p.project?.root) set.add(p.project.root);
return [...set];
};
function setMute(until) {
S.muteUntil = until || 0;
localStorage.setItem('baton_mute_until', String(S.muteUntil));
}
const muted = () => S.muteUntil > Date.now();
const statusInd = (state, text, soft = false) => span(`status-ind${soft ? ' pill-soft' : ''}`, h('i', { class: `dot dot-${state}` }), text || stateText(state));
const agentTag = (a) => span(`tag a-${a || 'x'}`, a || '?');
const modelTag = (m) => (m ? span('tag mono', m) : null);
const bar = (x, cls = '') => h('span', { class: `bar ${cls}` }, h('i', { style: `width:${Math.max(0, Math.min(1, x || 0)) * 100}%` }));
function skeleton(lines = 4) {
const widths = ['w-60', 'w-80', 'w-80', 'w-40'];
return h('div', { class: 'skel-wrap', 'aria-busy': 'true' },
Array.from({ length: lines }, (_, i) => h('div', { class: `skel ${widths[i % widths.length]}` })));
}
function emptyState(title, desc, action) {
return h('div', { class: 'empty-state' }, h('h3', null, title), desc ? h('p', null, desc) : null, action || null);
}
function errorBanner(msg, retryLabel, onRetry) {
return h('div', { class: 'callout err' }, h('span', null, msg), onRetry ? btn(retryLabel || '重试', { cls: 'sm', onclick: onRetry }) : null);
}
const kvRow = (k, v) => (v == null || v === '' || (Array.isArray(v) && !v.length) ? null : [h('div', { class: 'kv-k' }, k), h('div', { class: 'kv-v' }, v)]);
function kvGrid(pairs) {
const kids = pairs.flatMap(([k, v]) => kvRow(k, v) || []);
return kids.length ? h('div', { class: 'kv-grid' }, kids) : null;
}
const codeRefList = (l) => (l && l.length ? h('ul', { class: 'ref-list' }, l.map((c) => h('li', null, h('code', null, `${c.file}:${c.line}`), c.note ? span('muted', ` — ${c.note}`) : null))) : null);
const plainList = (l) => (l && l.length ? h('ul', { class: 'prose' }, l.map((x) => h('li', null, typeof x === 'string' ? x : x.title || JSON.stringify(x)))) : null);
function evBlock(e) {
const isLog = e.type === 'log';
const loc = isLog ? `${e.path}${e.line ? `:${e.line}` : ''}` : `${e.file}:${e.line_start}${e.line_end && e.line_end !== e.line_start ? `-${e.line_end}` : ''}`;
return h('div', { class: 'stack-xs' }, h('div', { class: 'row' }, span('tag', isLog ? 'log' : 'code'), h('code', null, loc)), h('pre', { class: 'code-block' }, e.quote || ''));
}
const evBlocks = (l) => (l && l.length ? h('div', { class: 'stack-sm' }, l.map((e) => evBlock(e.type ? e : { ...e, type: 'code' }))) : null);
function copyText(text, btnEl) {
const done = () => {
if (!btnEl) return;
const orig = btnEl.textContent;
btnEl.textContent = '已复制 ✓';
setTimeout(() => { btnEl.textContent = orig; }, 1600);
};
if (window.isSecureContext && navigator.clipboard?.writeText) {
navigator.clipboard.writeText(text).then(done).catch(() => fallbackCopy(text, done));
} else fallbackCopy(text, done);
}
function fallbackCopy(text, done) {
const ta = h('textarea', { readonly: true, style: 'position:fixed;top:-9999px;left:-9999px;opacity:0' });
ta.value = text;
document.body.appendChild(ta);
ta.focus();
ta.select();
try { ta.setSelectionRange(0, text.length); } catch { /* ignore */ }
try { if (document.execCommand('copy')) done(); else toast('复制失败，请手动选择复制', 'warn'); } catch { toast('复制失败，请手动选择复制', 'warn'); }
ta.remove();
}
function isSafeUrl(url) {
const u = String(url || '').trim();
return /^(https?:\/\/|file:\/\/)/i.test(u);
}
const INLINE_RE = /`([^`\n]+)`|\[((?:[^\]\n`]|`[^`\n]+`)+)\]\(([^)\s]+)\)|\*\*((?:[^*\n`]|\*(?!\*)|`[^`\n]+`)+?)\*\*|__([^_\n`]+?)__|~~((?:[^~\n`]|~(?!~)|`[^`\n]+`)+?)~~|\*((?:[^*\n`]|`[^`\n]+`)+?)\*/g;
function renderInlineMd(text, depth = 0) {
const out = [];
const s = String(text || '');
if (!s || depth > 4) return s ? [s] : out;
const re = new RegExp(INLINE_RE.source, 'g');
let last = 0; let m;
while ((m = re.exec(s)) !== null) {
if (m.index > last) out.push(s.slice(last, m.index));
if (m[1] !== undefined) {
out.push(h('code', { class: 'md-inline-code' }, m[1]));
} else if (m[2] !== undefined) {
const inner = renderInlineMd(m[2], depth + 1);
if (isSafeUrl(m[3])) {
const isHttp = /^https?:\/\//i.test(m[3]);
out.push(h('a', { href: m[3], ...(isHttp ? { target: '_blank', rel: 'noopener noreferrer' } : {}) }, inner));
} else {
out.push(...inner);
}
} else if (m[4] !== undefined || m[5] !== undefined) {
out.push(h('strong', null, renderInlineMd(m[4] ?? m[5], depth + 1)));
} else if (m[6] !== undefined) {
out.push(h('del', null, renderInlineMd(m[6], depth + 1)));
} else if (m[7] !== undefined) {
out.push(h('em', null, renderInlineMd(m[7], depth + 1)));
}
last = re.lastIndex;
}
if (last < s.length) out.push(s.slice(last));
return out;
}
function splitTableRow(line) {
let s = line.trim();
if (s.startsWith('|')) s = s.slice(1);
if (s.endsWith('|')) s = s.slice(0, -1);
return s.split('|').map((c) => c.trim());
}
function isTableSep(line) {
return /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line || '');
}
function tableAligns(line) {
return splitTableRow(line).map((c) => (c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : 'left'));
}
function renderCodeBlock(lang, codeText) {
const lineCount = codeText ? codeText.split('\n').length : 0;
const collapsible = lineCount > CODE_COLLAPSE_LINES;
const copyBtn = h('button', { type: 'button', class: 'md-copy-btn', onclick: (e) => copyText(codeText, e.currentTarget) }, '复制代码');
const wrap = h('div', { class: `md-code-wrap${collapsible ? ' is-collapsed' : ''}` },
h('div', { class: 'md-code-head' }, span('md-code-lang', lang || 'code'), copyBtn),
h('div', { class: 'md-code-body' }, h('pre', { class: 'md-code-pre' }, codeText)));
if (collapsible) {
const toggleBtn = h('button', {
type: 'button', class: 'md-code-expand',
onclick: () => {
const collapsed = wrap.classList.toggle('is-collapsed');
toggleBtn.textContent = collapsed ? `展开全部代码（共 ${lineCount} 行）` : '收起代码';
},
}, `展开全部代码（共 ${lineCount} 行）`);
wrap.appendChild(toggleBtn);
}
return wrap;
}
function renderMarkdown(src) {
const root = h('div', { class: 'md-body' });
const lines = String(src || '').replace(/\r\n?/g, '\n').split('\n');
let i = 0; let para = [];
const flushPara = () => {
if (!para.length) return;
const kids = [];
para.forEach((ln, idx) => {
if (idx > 0) kids.push(h('br'));
kids.push(...renderInlineMd(ln));
});
root.appendChild(h('p', null, kids));
para = [];
};
while (i < lines.length) {
const line = lines[i];
const fence = line.match(/^\s*(`{3,})([^`]*)$/);
if (fence) {
flushPara();
const fenceLen = fence[1].length;
const lang = (fence[2] || '').trim();
const closeRe = new RegExp(`^\\s*\`{${fenceLen},}\\s*$`);
const buf = [];
i++;
while (i < lines.length && !closeRe.test(lines[i])) { buf.push(lines[i]); i++; }
if (i < lines.length) i++;
root.appendChild(renderCodeBlock(lang, buf.join('\n')));
continue;
}
if (!line.trim()) { flushPara(); i++; continue; }
if (/^\s*(?:---+|\*\*\*+|___+)\s*$/.test(line)) {
flushPara();
root.appendChild(h('hr', { class: 'md-hr' }));
i++; continue;
}
const hd = line.match(/^\s*(#{1,4})\s+(.*)$/);
if (hd) {
flushPara();
const lvl = hd[1].length;
root.appendChild(h(`h${Math.min(lvl + 2, 6)}`, { class: `md-h${lvl}` }, renderInlineMd(hd[2].trim())));
i++; continue;
}
if (line.includes('|') && i + 1 < lines.length && isTableSep(lines[i + 1])) {
flushPara();
const headers = splitTableRow(line);
const aligns = tableAligns(lines[i + 1]);
i += 2;
const bodyRows = [];
while (i < lines.length && lines[i].includes('|') && lines[i].trim()) {
bodyRows.push(splitTableRow(lines[i]));
i++;
}
const tbl = h('div', { class: 'md-table-wrap' }, h('table', { class: 'md-table' },
h('thead', null, h('tr', null, headers.map((cell, ci) => h('th', { style: aligns[ci] !== 'left' ? `text-align:${aligns[ci]}` : null }, renderInlineMd(cell))))),
h('tbody', null, bodyRows.map((row) => h('tr', null, headers.map((_, ci) => h('td', { style: aligns[ci] !== 'left' ? `text-align:${aligns[ci]}` : null }, renderInlineMd(row[ci] ?? ''))))))));
root.appendChild(tbl);
continue;
}
if (/^\s*>\s?/.test(line)) {
flushPara();
const qLines = [];
while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
qLines.push(lines[i].replace(/^\s*>\s?/, ''));
i++;
}
root.appendChild(h('blockquote', { class: 'md-quote' }, renderMarkdown(qLines.join('\n'))));
continue;
}
const listMatch = line.match(/^(\s*)(?:([-*+])|(\d+)\.)\s+(.*)$/);
if (listMatch) {
flushPara();
const baseIndent = listMatch[1].length;
const ordered = !!listMatch[3];
const topList = h(ordered ? 'ol' : 'ul', { class: 'md-list' });
let lastLi = null; let subList = null; let subOrdered = null;
while (i < lines.length) {
const m = lines[i].match(/^(\s*)(?:([-*+])|(\d+)\.)\s+(.*)$/);
if (!m) break;
const ind = m[1].length;
const isOrd = !!m[3];
if (ind >= baseIndent + 2 && lastLi) {
if (!subList || subOrdered !== isOrd) {
subList = h(isOrd ? 'ol' : 'ul', { class: 'md-list' });
subOrdered = isOrd;
lastLi.appendChild(subList);
}
subList.appendChild(h('li', null, renderInlineMd(m[4])));
} else if (isOrd === ordered) {
subList = null;
lastLi = h('li', null, renderInlineMd(m[4]));
topList.appendChild(lastLi);
} else {
break;
}
i++;
}
root.appendChild(topList);
continue;
}
para.push(line.trim());
i++;
}
flushPara();
return root;
}
function stripAnsi(raw) {
return String(raw || '')
.replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '')
.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '')
.replace(/\x1b[@-Z\\-_]/g, '')
.replace(/\r\n?/g, '\n')
.replace(/\u00a0/g, ' ');
}
const PURE_BOX_RE = /^\s*[─━═╭╮╰╯┌┐└┘├┤┬┴┼│\s]{4,}\s*$/;
const TUI_CHROME_RE = /(?:^\s*\?\s*for shortcuts\b|shift\+tab to cycle|esc to interrupt|alt\+s\b|context left\b|←\s*(?:\d+\s+)?agents?\b|←\s*for agents\b|^\s*›\s*Ask Codex to do anything\s*$|^\s*⏵⏵\s*auto mode\b|^\s*●\s*\[\d{2}:\d{2}:\d{2}\]\s*cron\b|^\s*(?:Opus|Sonnet|Haiku|Claude|GPT-[^\s|]+)\s+[^|\n]*(?:\|\s*~\/|·\s*~\/))/i;
const EMPTY_PROMPT_RE = /^\s*[❯›>]\s*$/;
const WORKED_FOR_RE = /^\s*(?:Worked for\s+\S+\s*[•·]|[✻✳✶✢·*]\s+[A-Z][a-z]+\s+for\s+\d)/i;
function cleanTerminalToMarkdown(raw, agent) {
const cleaned = stripAnsi(raw);
let lines = cleaned.split('\n').map((l) => l.replace(/\s+$/, ''));
while (lines.length > 0) {
const t = lines[lines.length - 1];
if (!t.trim() || PURE_BOX_RE.test(t) || EMPTY_PROMPT_RE.test(t) || TUI_CHROME_RE.test(t) || WORKED_FOR_RE.test(t)) {
lines.pop();
} else break;
}
const out = [];
let inFence = false;
for (const orig of lines) {
let l = orig;
const boxWrap = l.match(/^\s*│\s?(.*?)\s?│\s*$/);
if (boxWrap) l = boxWrap[1];
if (/^\s*```/.test(l)) {
inFence = !inFence;
out.push(l.replace(/^ {1,2}(?=```)/, ''));
continue;
}
if (inFence) { out.push(l); continue; }
if (TUI_CHROME_RE.test(l) || EMPTY_PROMPT_RE.test(l)) continue;
if (WORKED_FOR_RE.test(l)) continue;
if (PURE_BOX_RE.test(l)) {
const ruleChars = l.replace(/\s+/g, '');
if (ruleChars.length >= 4 && ruleChars.length <= 18 && /^[─━═]+$/.test(ruleChars)) out.push('---');
continue;
}
if (l.startsWith('  ') && !/^ {4}/.test(l)) l = l.slice(2);
const subBullet = l.match(/^(\s{2,})[•●▪]\s+(.*)$/);
if (subBullet) { out.push(`  - ${subBullet[2]}`); continue; }
const topBullet = l.match(/^\s*[•●▪]\s+(.*)$/);
if (topBullet) { out.push(`- ${topBullet[1]}`); continue; }
const rec = l.match(/^\s*⏺\s+(.*)$/);
if (rec) { out.push(rec[1]); continue; }
const sub = l.match(/^\s*⎿\s+(.*)$/);
if (sub) { out.push(`> ${sub[1]}`); continue; }
const hd = l.match(/^\s{1,4}(#{1,4}\s+.*)$/);
if (hd) { out.push(hd[1]); continue; }
const usr = l.match(/^\s*[❯›]\s+(.+)$/);
if (usr && !/^Ask Codex to do anything$/i.test(usr[1].trim())) {
out.push('', `> **用户输入**：${usr[1].trim()}`, '');
continue;
}
out.push(l);
}
return out.join('\n').replace(/\n{3,}/g, '\n\n').trim();
}
function parseTerminalTurns(raw, agent) {
const cleaned = stripAnsi(raw);
let lines = cleaned.split('\n').map((l) => l.replace(/\s+$/, ''));
while (lines.length > 0) {
const t = lines[lines.length - 1];
if (!t.trim() || PURE_BOX_RE.test(t) || EMPTY_PROMPT_RE.test(t) || TUI_CHROME_RE.test(t) || WORKED_FOR_RE.test(t)) lines.pop();
else break;
}
const turns = [];
let buf = [];
const flushAsst = () => {
const md = cleanTerminalToMarkdown(buf.join('\n'), agent);
if (md) turns.push({ role: 'assistant', text: md });
buf = [];
};
for (const l of lines) {
const pm = l.match(/^\s*[❯›]\s+(.+)$/);
if (pm && !/^Ask Codex to do anything$/i.test(pm[1].trim())) {
flushAsst();
turns.push({ role: 'user', text: pm[1].trim() });
} else {
buf.push(l);
}
}
flushAsst();
return turns;
}
const mdProse = renderMarkdown;
const focusBtn = (id, cls = 'ghost sm') => btn('切到 tmux', {
cls, title: '切换 tmux 焦点到该 pane', busyKey: `focus:${id}`,
onclick: () => act(`focus:${id}`, () => api('POST', `/api/sessions/${enc(id)}/focus`, {}), '已切换 tmux 焦点'),
});
let TOP = null;
function applyMuteRemote(payload, label, fallbackUntil) {
return act('mute', async () => {
let r;
try { r = await api('POST', '/api/feishu/mute', payload); } catch (e) {
if (e instanceof ApiError && (e.status === 404 || e.status === 405)) {
throw new ApiError('unavailable', '当前 daemon 未启用 Web 静音接口，可在飞书发送「静音 30m」', e.status);
}
throw e;
}
const until = typeof r?.feishu_muted_until_ms === 'number'
? r.feishu_muted_until_ms
: (r?.feishu_muted_until_ms === null ? 0 : fallbackUntil);
setMute(until);
UI.muteMenu = false;
return until;
}, (until) => (until ? `${label}（至 ${muteUntilLabel(until)}，Web 不受影响）` : '已恢复飞书通知'));
}
function buildMutePopover() {
const m = muted();
if (!UI.muteTimeVal) UI.muteTimeVal = defaultMuteTimeStr();
const numInp = h('input', {
type: 'number', min: '1', max: '10080', class: 'inp sm tabular mute-num',
value: UI.muteCustomVal, 'aria-label': '自定义静音时长',
oninput: (e) => { UI.muteCustomVal = e.target.value; },
});
const unitSel = h('select', {
class: 'inp sm mute-unit', 'aria-label': '时长单位', value: UI.muteCustomUnit,
onchange: (e) => { UI.muteCustomUnit = e.target.value; },
}, h('option', { value: 'm', selected: UI.muteCustomUnit === 'm' }, '分钟'), h('option', { value: 'h', selected: UI.muteCustomUnit === 'h' }, '小时'));
const submitCustom = () => {
const raw = Number(numInp.value);
if (!Number.isFinite(raw) || raw <= 0) { toast('请输入大于 0 的时长', 'warn'); return; }
const mins = unitSel.value === 'h' ? Math.round(raw * 60) : Math.round(raw);
if (mins < 1 || mins > 10080) { toast('静音时长须在 1 分钟至 7 天之间', 'warn'); return; }
const lbl = unitSel.value === 'h' ? `飞书通知已静音 ${raw} 小时` : `飞书通知已静音 ${mins} 分钟`;
applyMuteRemote({ minutes: mins }, lbl, Date.now() + mins * 60000);
};
numInp.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); submitCustom(); } });
const timeHint = span('dim tabular', '');
const syncTimeHint = () => {
const ts = timeStrToUntilMs(UI.muteTimeVal);
timeHint.textContent = ts ? `将静音至 ${muteUntilLabel(ts)}` : '请选择有效时间';
};
const timeInp = h('input', {
type: 'time', class: 'inp sm tabular mute-time', value: UI.muteTimeVal, 'aria-label': '静音截止时刻',
oninput: (e) => { UI.muteTimeVal = e.target.value; syncTimeHint(); },
});
syncTimeHint();
const submitTime = () => {
const ts = timeStrToUntilMs(timeInp.value);
if (!ts) { toast('请选择有效的截止时刻', 'warn'); return; }
applyMuteRemote({ until_ms: ts }, '飞书通知已静音', ts);
};
const presets = [
['15 分钟', () => applyMuteRemote({ minutes: 15 }, '飞书通知已静音 15 分钟', Date.now() + 900000)],
['30 分钟', () => applyMuteRemote({ minutes: 30 }, '飞书通知已静音 30 分钟', Date.now() + 1800000)],
['1 小时', () => applyMuteRemote({ minutes: 60 }, '飞书通知已静音 1 小时', Date.now() + 3600000)],
['2 小时', () => applyMuteRemote({ minutes: 120 }, '飞书通知已静音 2 小时', Date.now() + 7200000)],
['至明早 09:00', () => { const ts = tomorrowAt9Ms(); applyMuteRemote({ until_ms: ts }, '飞书通知已静音', ts); }],
];
return h('div', { class: 'mute-pop', role: 'dialog', 'aria-label': '飞书通知静音设置' },
m ? h('div', { class: 'mute-status' },
h('span', { class: 'tabular' }, `已静音至 ${muteUntilLabel(S.muteUntil)}`),
btn('立即取消静音', { cls: 'xs', busyKey: 'mute', onclick: () => applyMuteRemote({ minutes: 0 }, '', 0) })) : null,
h('div', { class: 'mute-sec' },
span('mute-lbl', '快捷静音'),
h('div', { class: 'mute-presets' }, presets.map(([l, fn]) => h('button', { type: 'button', class: 'mute-preset', onclick: fn }, l)))),
h('div', { class: 'mute-sec' },
span('mute-lbl', '自定义时长'),
h('div', { class: 'mute-row' }, numInp, unitSel, btn('应用静音', { cls: 'primary sm', busyKey: 'mute', onclick: submitCustom }))),
h('div', { class: 'mute-sec' },
h('div', { class: 'row', style: 'justify-content:space-between' }, span('mute-lbl', '指定截止时刻'), timeHint),
h('div', { class: 'mute-row' }, timeInp, btn('静音至此', { cls: 'sm', busyKey: 'mute', onclick: submitTime }))),
h('div', { class: 'mute-sep' }),
h('button', {
type: 'button', class: 'mute-foot-btn',
onclick: () => { UI.muteMenu = false; schedule(); act('feishu-test', () => api('POST', '/api/feishu/test', { text: 'baton Web 控制台测试通知' }), '测试卡片已发送'); },
}, '发送飞书测试卡片'));
}
function renderTop() {
const cs = connState();
const liveSessions = rootSessions().filter((s) => !s.is_subagent && s.state !== 'ended' && !!s.pane).length;
const alerts = alertCount();
const activePipes = allPipelines().filter((p) => !isFinished(p)).length;
const pendingVerdicts = [...S.reviews.values()].reduce((n, r) => n + (r.findings || []).filter((f) => f.falsifier?.verdict === 'needs_human' && !f.feedback).length, 0);
const counts = { workspace: liveSessions, inbox: alerts, pipelines: activePipes, reviews: pendingVerdicts };
const connTxt = { live: '实时已连接', connecting: '重连中…', down: '连接已断开' }[cs];
const connTip = `baton ${S.version || '?'} · SSE ${S.conn} · seq ${S.lastSeq}${S.uptimeMs != null ? ` · 运行 ${dur(S.uptimeMs)}` : ''}`;
const bar = $('#topbar');
if (!TOP || TOP.bar !== bar) {
const nav = h('nav', { class: 'nav-seg', role: 'tablist', 'aria-label': '主导航' });
const connPill = h('span', { class: 'status-ind pill-soft' });
const muteBtn = h('button', {
type: 'button', class: 'btn sm pill',
onclick: () => { UI.muteMenu = !UI.muteMenu; rerender(); },
});
const mutePopSlot = h('div');
const muteWrap = h('div', { class: 'mute-wrap' }, muteBtn, mutePopSlot);
bar.replaceChildren(
h('a', { class: 'brand', href: '#/workspace' }, h('img', { class: 'logo', src: '/assets/logo.svg', alt: '', width: 26, height: 26 }), h('span', null, 'baton')),
nav,
h('div', { class: 'top-tools' }, connPill, muteWrap));
TOP = { bar, nav, connPill, muteBtn, mutePopSlot, popKey: '' };
}
TOP.nav.replaceChildren(...VIEWS.map((v, i) => h('a', {
class: `nav-tab${UI.view === v ? ' on' : ''}`, href: `#/${v}`, role: 'tab', title: `${VIEW_LABEL[v]}（快捷键 ${i + 1}）`,
}, VIEW_LABEL[v], counts[v] ? span(`nav-count${v === 'inbox' ? ' attn' : ''}`, counts[v]) : null)));
TOP.connPill.title = connTip;
TOP.connPill.replaceChildren(h('i', { class: `dot dot-${cs}` }), connTxt);
const m = muted();
TOP.muteBtn.title = m ? `飞书通知已静音至 ${muteUntilLabel(S.muteUntil)}` : '设置飞书通知静音';
TOP.muteBtn.className = `btn sm pill${m ? ' on' : ''}`;
TOP.muteBtn.replaceChildren(...(m
? [h('i', { class: 'dot dot-needs_permission' }), `已静音 · ${muteRemain(S.muteUntil)}`]
: ['飞书通知：开启']));
const popKey = UI.muteMenu ? `open:${m ? S.muteUntil : 0}` : 'closed';
if (TOP.popKey !== popKey) {
TOP.popKey = popKey;
if (UI.muteMenu) TOP.mutePopSlot.replaceChildren(buildMutePopover());
else TOP.mutePopSlot.replaceChildren();
}
}
document.addEventListener('click', (e) => { if (UI.muteMenu && !e.target.closest('.mute-wrap')) { UI.muteMenu = false; schedule(); } });
let WS = null;
function renderView() {
const el = $('#view');
const isWs = UI.view === 'workspace' && S.loaded;
el.className = `view${UI.view === 'workspace' ? ' full-h' : ''}`;
if (isWs) {
mountOrPatchWorkspace(el);
return;
}
let content;
if (S.authNeeded && !S.loaded) {
content = emptyState('需要认证 Token', '请在弹窗中输入本地 daemon 生成的 token，或通过 baton web 打开控制台。');
} else if (!S.loaded) {
content = S.loadError
? errorBanner(`无法连接 baton daemon：${S.loadError}`, '重新连接', loadAll)
: h('div', { class: 'card' }, skeleton(6));
} else {
const fn = { inbox: viewInbox, pipelines: viewPipelines, reviews: viewReviews }[UI.view] || viewInbox;
content = fn();
}
const nodes = [S.loaded && S.loadError ? errorBanner(`状态同步异常：${S.loadError}`, '重试同步', loadAll) : null, content].filter(Boolean);
el.replaceChildren(...nodes);
}
function subagentDisplayName(c) {
if (c.tmux_label) return c.tmux_label;
const id = String(c.session_id || '');
if (id.includes('/')) return id.split('/').slice(1).join('/');
return short(id, 14);
}
function autoGrow(ta) {
if (!ta) return;
const minH = 52; const maxH = 220;
if (ta.offsetHeight >= maxH - 1 && ta.scrollHeight > maxH + 2) {
ta.style.height = `${maxH}px`;
return;
}
const prevTop = ta.scrollTop;
ta.style.height = 'auto';
const nextH = Math.min(maxH, Math.max(minH, ta.scrollHeight));
ta.style.height = `${nextH}px`;
if (prevTop > 0) ta.scrollTop = prevTop;
}
function removeAttachment(sid, attId) {
const list = (UI.attachments.get(sid) || []).filter((x) => x.id !== attId);
if (list.length) UI.attachments.set(sid, list);
else UI.attachments.delete(sid);
renderAttachBar(sid);
}
function expandAttachmentToInput(sid, attId) {
const list = UI.attachments.get(sid) || [];
const item = list.find((x) => x.id === attId);
if (!item) return;
const dk = `ws:msg:${sid}`;
const ta = WS?.comp?.sid === sid ? WS.comp.ta : null;
const cur = ta ? ta.value : draft(dk);
const next = cur ? `${cur}\n${item.content}` : item.content;
UI.drafts.set(dk, next);
if (ta) {
ta.value = next;
ta.style.height = 'auto';
autoGrow(ta);
ta.focus();
ta.selectionStart = ta.selectionEnd = next.length;
}
removeAttachment(sid, attId);
}
function readFileAsBase64(file) {
return new Promise((resolve, reject) => {
const fr = new FileReader();
fr.onload = () => {
const res = String(fr.result || '');
const idx = res.indexOf(',');
resolve(idx >= 0 ? res.slice(idx + 1) : res);
};
fr.onerror = () => reject(new Error('读取图片文件失败'));
fr.readAsDataURL(file);
});
}
async function uploadImageFiles(sid, files) {
if (!sid) return;
const imgs = [...(files || [])].filter((f) => f && String(f.type || '').startsWith('image/'));
if (!imgs.length) return;
for (const file of imgs) {
if (file.size > IMG_MAX_BYTES) {
toast(`图片 ${file.name || ''} 超过 10 MB 限制`, 'warn');
continue;
}
const ext = (file.type.split('/')[1] || 'png').replace(/[^a-z0-9]/gi, '') || 'png';
const name = file.name && file.name !== 'image.png' ? file.name : `screenshot-${new Date().toTimeString().slice(0, 8).replace(/:/g, '')}.${ext}`;
const list = UI.attachments.get(sid) || [];
const item = { id: `att-${++attachSeq}`, kind: 'image', title: name, filename: name, mime: file.type || 'image/png', bytes: file.size || 0, status: 'uploading', previewUrl: '', path: '' };
list.push(item);
UI.attachments.set(sid, list);
renderAttachBar(sid);
try {
const data_base64 = await readFileAsBase64(file);
const r = await api('POST', '/api/uploads/image', { filename: name, mime: item.mime, data_base64 });
item.status = 'ready';
item.path = r.path || '';
item.previewUrl = r.preview_url || '';
item.bytes = r.size_bytes ?? item.bytes;
item.filename = r.filename || item.filename;
item.title = item.filename;
renderAttachBar(sid);
toast(`图片已添加（${fmtBytes(item.bytes)}）`, 'ok');
} catch (e) {
removeAttachment(sid, item.id);
toast(`图片上传失败：${errText(e)}`, 'err');
}
}
}
function renderAttachBar(sid) {
if (!WS?.comp) return;
const bar = WS.comp.attachBar;
const list = sid ? (UI.attachments.get(sid) || []) : [];
const sig = list.map((x) => `${x.id}:${x.status || 'ok'}:${x.previewUrl || ''}`).join(',');
if (bar.dataset.sid === (sid || '') && bar.dataset.sig === sig && bar.classList.contains('ws-hidden') === (list.length === 0)) return;
bar.dataset.sid = sid || '';
bar.dataset.sig = sig;
if (!list.length || UI.composerMode !== 'direct') {
bar.classList.add('ws-hidden');
bar.replaceChildren();
return;
}
bar.classList.remove('ws-hidden');
bar.replaceChildren(...list.map((att) => {
if (att.kind === 'image') {
const uploading = att.status === 'uploading';
const thumb = uploading
? h('span', { class: 'attach-thumb-load', title: '图片上传中…' })
: h('button', { type: 'button', class: 'attach-thumb-btn', title: '点击放大预览原图', onclick: () => openDrawer('paste', { sessionId: sid, attachmentId: att.id }) },
h('img', { class: 'attach-thumb', src: att.previewUrl, alt: att.title }));
return h('div', { class: 'attach-pill img-pill' },
thumb,
h('span', { class: 'attach-info' },
span('attach-meta', att.title),
span('attach-sub', uploading ? '上传中…' : fmtBytes(att.bytes))),
h('span', { class: 'attach-acts' },
!uploading ? h('button', { type: 'button', class: 'attach-act', onclick: () => openDrawer('paste', { sessionId: sid, attachmentId: att.id }) }, '预览') : null,
h('button', { type: 'button', class: 'attach-act del', title: '移除图片', 'aria-label': '移除图片', onclick: () => removeAttachment(sid, att.id) }, '×')));
}
return h('div', { class: 'attach-pill' },
span('attach-meta', `📋 ${att.title} · ${att.lines} 行 · ${fmtBytes(att.bytes)}`),
h('span', { class: 'attach-acts' },
h('button', { type: 'button', class: 'attach-act', onclick: () => openDrawer('paste', { sessionId: sid, attachmentId: att.id }) }, '预览'),
h('button', { type: 'button', class: 'attach-act', onclick: () => expandAttachmentToInput(sid, att.id) }, '转入输入框'),
h('button', { type: 'button', class: 'attach-act del', title: '移除粘贴片段', 'aria-label': '移除粘贴片段', onclick: () => removeAttachment(sid, att.id) }, '×')));
}));
}
function closeSlashMenu() {
UI.slash.open = false;
UI.slash.items = [];
UI.slash.idx = 0;
if (WS?.comp?.slashEl) {
WS.comp.slashEl.classList.add('ws-hidden');
WS.comp.slashEl.replaceChildren();
}
}
function renderSlashMenu(agent) {
const box = WS?.comp?.slashEl;
if (!box) return;
const { open, items, idx } = UI.slash;
if (!open || !items.length || UI.composerMode !== 'direct') {
box.classList.add('ws-hidden');
box.replaceChildren();
return;
}
const agLabel = AGENT_DISPLAY[agent] || agent || 'Agent';
const nativeItems = [];
const batonItems = [];
items.forEach((it, flatIdx) => {
if (it.group === 'baton') batonItems.push([it, flatIdx]);
else nativeItems.push([it, flatIdx]);
});
const mkRow = ([it, flatIdx]) => h('button', {
type: 'button',
class: `slash-item${flatIdx === idx ? ' active' : ''}`,
'data-sidx': String(flatIdx),
onmousedown: (e) => e.preventDefault(),
onmouseenter: () => { UI.slash.idx = flatIdx; syncSlashActive(); },
onclick: () => applySlashItem(it),
},
h('span', { class: 'slash-left' },
span('slash-cmd', it.cmd),
it.group === 'skill' ? span('tag', 'skill') : null,
it.args_hint ? span('slash-args', it.args_hint) : null),
span('slash-desc', it.desc || ''));
const listNodes = [];
if (nativeItems.length) {
listNodes.push(h('div', { class: 'slash-group' }, 'Agent 原生命令', agentTag(agent)));
listNodes.push(...nativeItems.map(mkRow));
}
if (batonItems.length) {
listNodes.push(h('div', { class: 'slash-group' }, 'Baton 控制台快捷命令'));
listNodes.push(...batonItems.map(mkRow));
}
box.classList.remove('ws-hidden');
box.replaceChildren(
h('div', { class: 'slash-list', role: 'listbox', 'aria-label': '斜杠命令菜单' }, listNodes),
h('div', { class: 'slash-foot' },
h('span', null, '↑↓ 选择 · Enter / Tab 确认 · Esc 关闭'),
span('tabular', `${items.length} 条命令`)));
}
function syncSlashActive() {
const box = WS?.comp?.slashEl;
if (!box) return;
for (const el of box.querySelectorAll('.slash-item[data-sidx]')) {
const on = Number(el.dataset.sidx) === UI.slash.idx;
el.classList.toggle('active', on);
if (on) el.scrollIntoView({ block: 'nearest' });
}
}
function updateSlashFromInput(ta, agent) {
if (!ta || UI.composerMode !== 'direct') { closeSlashMenu(); return; }
const pos = ta.selectionStart;
if (pos == null || pos !== ta.selectionEnd) { closeSlashMenu(); return; }
const before = ta.value.slice(0, pos);
const m = before.match(/^\s*(\/[^\s/]*)$/);
if (!m) { closeSlashMenu(); return; }
const token = m[1];
const start = before.indexOf(token);
const q = token.slice(1).toLowerCase();
const ag = agent || WS?.comp?.agent || 'claude';
loadSlashCommands(ag);
const all = slashCommandsFor(ag);
const filtered = q ? all.filter((c) => c.cmd.toLowerCase().includes(q) || (c.desc || '').toLowerCase().includes(q)) : all;
if (!filtered.length) { closeSlashMenu(); return; }
const prevCmd = UI.slash.open && UI.slash.items[UI.slash.idx]?.cmd;
let nextIdx = prevCmd ? filtered.findIndex((c) => c.cmd === prevCmd) : 0;
if (nextIdx < 0) nextIdx = 0;
UI.slash = { open: true, start, end: pos, query: q, items: filtered, idx: nextIdx };
renderSlashMenu(ag);
}
function runBatonSlashAction(action) {
if (action === 'mode_feature') { UI.composerMode = 'feature'; rerender(); }
else if (action === 'mode_debug') { UI.composerMode = 'debug'; rerender(); }
else if (action === 'open_mute') { UI.muteMenu = true; rerender(); }
else if (action === 'toggle_terminal') { UI.outputMode = UI.outputMode === 'raw' ? 'reading' : 'raw'; if (UI.outputMode === 'raw') { const cur = currentSession(); if (cur?.pane) loadPreview(cur.pane, { lines: PREVIEW_LINES, silent: true }); } rerender(); }
else if (action === 'open_drawer') { const cur = currentSession(); if (cur) openDrawer('session', { sessionId: cur.session_id }); }
}
function applySlashItem(item) {
const ta = WS?.comp?.ta;
if (!ta || !item) return;
const sid = WS.comp.sid;
const { start, end } = UI.slash;
const head = ta.value.slice(0, start);
const tail = ta.value.slice(end);
closeSlashMenu();
if (item.group === 'baton' || item.action) {
const cleaned = (head + tail.replace(/^\s/, '')).trimEnd();
ta.value = cleaned;
if (sid) UI.drafts.set(`ws:msg:${sid}`, cleaned);
ta.style.height = 'auto';
autoGrow(ta);
runBatonSlashAction(item.action);
return;
}
const ins = item.needs_args ? `${item.cmd} ` : item.cmd;
const nextVal = head + ins + tail;
const caret = start + ins.length;
ta.value = nextVal;
if (sid) UI.drafts.set(`ws:msg:${sid}`, nextVal);
autoGrow(ta);
ta.focus({ preventScroll: true });
ta.setSelectionRange(caret, caret);
}
function createWorkspaceShell() {
const sideCount = span('tag tabular', '0');
const sideSearch = h('input', {
class: 'inp sm', type: 'search', placeholder: '搜索会话、tmux 窗口或目录…', autocomplete: 'off',
'data-draft': 'ws:filter', value: draft('ws:filter'),
oninput: (e) => { UI.drafts.set('ws:filter', e.target.value); schedule(); },
});
const sideToggleSlot = h('span');
const sideChips = h('div', { class: 'chips' });
const sideShowEnded = h('input', {
type: 'checkbox', checked: UI.showEnded,
onchange: async (e) => {
UI.showEnded = e.target.checked;
if (UI.showEnded) {
try {
const list = await api('GET', `/api/sessions${qs({ subagents: 1, ended: 1 })}`);
(list || []).forEach(upsertSession);
} catch { /* fallback */ }
} else {
for (const [id, s] of S.sessions.entries()) if (s.state === 'ended') S.sessions.delete(id);
pickDefaultSession();
}
schedule();
},
});
const sideList = h('div', { class: 'ws-side-list' });
const sidebar = h('aside', { class: 'ws-sidebar', 'aria-label': '会话列表' },
h('div', { class: 'ws-side-head' },
h('div', { class: 'ws-side-top' },
h('div', { class: 'ws-side-title' }, '会话列表', sideCount),
btn('+ 新建交互会话', { cls: 'sm', title: '在 tmux 中新开 claude / codex / agy 交互会话（快捷键 n）', onclick: openSpawnModal })),
h('div', { class: 'ws-search-row' }, sideSearch, sideToggleSlot),
h('div', { class: 'row' },
sideChips,
h('label', { class: 'row muted', style: 'font-size:12px;margin-left:auto;cursor:pointer' }, sideShowEnded, '含已结束'))),
sideList);
const headEl = h('header', { class: 'ws-chat-head' });
const crumbEl = h('div', { class: 'sub-crumb ws-hidden' });
const reopenBtn = h('button', {
type: 'button', class: 'outline-reopen-btn ws-hidden', title: '展开提问目录',
onclick: () => setOutlineCollapsed(false),
});
const vp = h('div', {
class: 'term-viewport',
onscroll: (e) => syncScrollLatestBtn(e.currentTarget),
});
const latestBtn = h('button', {
type: 'button', class: 'scroll-latest-btn',
onclick: () => { vp.scrollTo({ top: vp.scrollHeight, behavior: 'smooth' }); },
}, '↓ 回到最新');
const chatStage = h('div', { class: 'chat-stage' }, vp, latestBtn);
const slashEl = h('div', { class: 'slash-menu ws-hidden' });
const modeBtns = [['direct', '直接发送'], ['feature', '新功能协作'], ['debug', '两速查错']].map(([k, l]) => h('button', {
type: 'button', role: 'tab', class: 'mode-btn', 'data-mode': k,
onclick: () => { UI.composerMode = k; closeSlashMenu(); rerender(); },
}, l));
const modeHint = h('span', { class: 'dim', style: 'font-size:12px' });
const modeBar = h('div', { class: 'mode-bar' },
h('div', { class: 'mode-seg', role: 'tablist', 'aria-label': '输入模式' }, modeBtns),
modeHint);
const attachBar = h('div', { class: 'attach-bar ws-hidden' });
const ta = h('textarea', {
id: 'composer-input', rows: '2', class: 'composer-ta',
placeholder: '发消息给当前会话…（输入 / 唤出命令，Cmd+Enter 发送）',
});
const sendDirect = () => {
const sid = comp.sid;
const s = sid ? S.sessions.get(sid) : null;
if (!s) return;
const dk = `ws:msg:${sid}`;
const text = (ta.value || '').trim();
const atts = UI.attachments.get(sid) || [];
if (atts.some((a) => a.status === 'uploading')) { toast('图片仍在上传中，请稍候再发送', 'warn'); return; }
if (!text && !atts.length) { toast('请输入要发送的内容、图片或粘贴片段', 'warn'); return; }
closeSlashMenu();
act(`send:${sid}`, async () => {
const payload = { target: { type: 'session', id: sid }, text, interactive: true };
if (atts.length) {
payload.attachments = atts.map((a) => (a.kind === 'image'
? { kind: 'image', title: a.title, path: a.path }
: { kind: 'text', title: a.title, content: a.content }));
}
const r = await api('POST', '/api/dispatch', payload);
UI.drafts.delete(dk);
UI.attachments.delete(sid);
if (comp.sid === sid) {
ta.value = '';
ta.style.height = 'auto';
autoGrow(ta);
renderAttachBar(sid);
}
if (s.pane) {
setTimeout(() => loadSessionOutput(s, { silent: true }), 550);
setTimeout(() => loadSessionOutput(s, { silent: true }), 1600);
}
return r;
}, (r) => ({ msg: `已发送：${deliveryText(r)}`, kind: r && (r.status === 'failed' || r.status === 'unconfirmed') ? 'warn' : 'ok' }));
};
ta.addEventListener('input', () => {
if (comp.sid) UI.drafts.set(`ws:msg:${comp.sid}`, ta.value);
autoGrow(ta);
updateSlashFromInput(ta, comp.agent);
});
ta.addEventListener('click', () => updateSlashFromInput(ta, comp.agent));
ta.addEventListener('keyup', (e) => {
if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) updateSlashFromInput(ta, comp.agent);
});
ta.addEventListener('paste', (e) => {
const cd = e.clipboardData;
if (cd) {
const imgFiles = [];
for (const it of [...(cd.items || [])]) {
if (it.kind === 'file' && String(it.type || '').startsWith('image/')) {
const f = it.getAsFile();
if (f) imgFiles.push(f);
}
}
if (!imgFiles.length && cd.files?.length) {
for (const f of [...cd.files]) if (String(f.type || '').startsWith('image/')) imgFiles.push(f);
}
if (imgFiles.length) {
e.preventDefault();
uploadImageFiles(comp.sid, imgFiles);
return;
}
}
const txt = cd?.getData('text/plain') ?? '';
if (!txt) return;
const lines = txt.split(/\r?\n/).length;
if (txt.length >= PASTE_CHAR_LIMIT || lines >= PASTE_LINE_LIMIT) {
e.preventDefault();
const sid = comp.sid;
if (!sid) return;
const list = UI.attachments.get(sid) || [];
const bytes = new TextEncoder().encode(txt).length;
list.push({ id: `att-${++attachSeq}`, kind: 'text', title: `粘贴片段 #${list.length + 1}`, content: txt, lines, bytes });
UI.attachments.set(sid, list);
renderAttachBar(sid);
toast(`已折叠为粘贴附件（${lines} 行 · ${fmtBytes(bytes)}），可继续补充说明`, 'ok');
}
});
ta.addEventListener('keydown', (e) => {
if (UI.slash.open && UI.slash.items.length > 0) {
const len = UI.slash.items.length;
if (e.key === 'ArrowDown') { e.preventDefault(); UI.slash.idx = (UI.slash.idx + 1) % len; syncSlashActive(); return; }
if (e.key === 'ArrowUp') { e.preventDefault(); UI.slash.idx = (UI.slash.idx - 1 + len) % len; syncSlashActive(); return; }
if ((e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey)) && !e.isComposing) {
e.preventDefault();
applySlashItem(UI.slash.items[UI.slash.idx]);
return;
}
if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closeSlashMenu(); return; }
}
if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !e.isComposing) { e.preventDefault(); sendDirect(); }
});
const imgInput = h('input', {
type: 'file', accept: 'image/*', multiple: true, class: 'ws-hidden', 'aria-label': '选择图片上传',
onchange: () => {
if (imgInput.files?.length) uploadImageFiles(comp.sid, imgInput.files);
imgInput.value = '';
},
});
const imgBtn = h('button', {
type: 'button', class: 'btn ghost sm', title: '上传图片或截图（支持粘贴与拖拽）',
onclick: () => imgInput.click(),
}, '图片');
const sendBtn = h('button', { type: 'button', class: 'btn primary sm', onclick: sendDirect }, '发送');
const directRow = h('div', { class: 'composer-direct-row' }, ta, h('div', { class: 'composer-actions' }, imgInput, imgBtn, sendBtn));
const pipeSlot = h('div', { class: 'stack-sm ws-hidden' });
const readonlyHint = h('div', { class: 'comp-readonly ws-hidden' });
const card = h('div', { class: 'composer-card' }, slashEl, readonlyHint, modeBar, attachBar, directRow, pipeSlot);
let dragDepth = 0;
card.addEventListener('dragenter', (e) => {
if (UI.composerMode !== 'direct' || !e.dataTransfer?.types?.includes('Files')) return;
e.preventDefault();
dragDepth++;
card.classList.add('is-dragover');
});
card.addEventListener('dragover', (e) => {
if (UI.composerMode !== 'direct' || !e.dataTransfer?.types?.includes('Files')) return;
e.preventDefault();
e.dataTransfer.dropEffect = 'copy';
});
card.addEventListener('dragleave', () => {
dragDepth = Math.max(0, dragDepth - 1);
if (!dragDepth) card.classList.remove('is-dragover');
});
card.addEventListener('drop', (e) => {
dragDepth = 0;
card.classList.remove('is-dragover');
if (UI.composerMode !== 'direct') return;
const imgs = [...(e.dataTransfer?.files || [])].filter((f) => String(f.type || '').startsWith('image/'));
if (imgs.length) {
e.preventDefault();
uploadImageFiles(comp.sid, imgs);
}
});
const dock = h('div', { class: 'composer-dock' }, card);
const comp = { dock, card, slashEl, readonlyHint, modeBtns, modeHint, attachBar, directRow, ta, imgBtn, sendBtn, pipeSlot, sid: null, agent: null };
const convCol = h('div', { class: 'ws-conv-col' }, reopenBtn, chatStage, dock);
const bodySplit = h('div', { class: 'ws-body-split' }, convCol);
const emptyMain = emptyState('选择一个会话开始对话', '从左侧点击任意正在运行的 tmux 会话，即可查看实时终端并直接发送指令。',
btn('新建交互会话', { cls: 'primary', onclick: openSpawnModal }));
emptyMain.classList.add('ws-hidden');
const main = h('section', { class: 'ws-main' }, headEl, crumbEl, bodySplit, emptyMain);
const grid = h('div', { class: 'ws-grid' }, sidebar, main);
const errSlot = h('div', { class: 'ws-hidden', style: 'padding:10px 16px 0' });
return { errSlot, grid, sideCount, sideSearch, sideToggleSlot, sideChips, sideShowEnded, sideList, headEl, crumbEl, bodySplit, convCol, reopenBtn, vp, emptyMain, comp, rail: null, railSig: '' };
}
document.addEventListener('click', (e) => {
if (UI.slash.open && !e.target.closest('.composer-card')) closeSlashMenu();
});
function mountOrPatchWorkspace(el) {
if (!WS) WS = createWorkspaceShell();
if (WS.grid.parentNode !== el) {
el.replaceChildren(WS.errSlot, WS.grid);
}
if (S.loadError) {
WS.errSlot.classList.remove('ws-hidden');
WS.errSlot.replaceChildren(errorBanner(`状态同步异常：${S.loadError}`, '重试同步', loadAll));
} else {
WS.errSlot.classList.add('ws-hidden');
WS.errSlot.replaceChildren();
}
patchWorkspace(WS);
}
function patchWorkspace(W) {
const roots = rootSessions().filter((s) => UI.showEnded || s.state !== 'ended');
const q = draft('ws:filter').trim().toLowerCase();
const hayOf = (s) => [s.tmux_label, s.tmux_target, s.pane, s.agent, s.model, s.git_root, s.cwd, s.last_message, s.session_id].join(' ').toLowerCase();
const match = (s) => (!UI.agentFilter || s.agent === UI.agentFilter)
&& (!q || hayOf(s).includes(q) || subagentsOf(s.session_id).some((c) => hayOf(c).includes(q)));
const shown = roots.filter(match);
const groups = new Map();
for (const s of shown) {
const g = tmuxGroupOf(s);
if (!groups.has(g)) groups.set(g, []);
groups.get(g).push(s);
}
const sortedGroups = [...groups.entries()].sort((a, b) => {
if (a[0] === NON_TMUX_GROUP) return 1;
if (b[0] === NON_TMUX_GROUP) return -1;
const pa = Math.min(...a[1].map((s) => STATE_PRIO[s.state] ?? 5));
const pb = Math.min(...b[1].map((s) => STATE_PRIO[s.state] ?? 5));
return pa - pb || a[0].localeCompare(b[0]);
});
const active = currentSession();
const activeRootId = active?.is_subagent && active.parent_session_id ? active.parent_session_id : active?.session_id;
if (!UI.groupsInited && sortedGroups.length >= 3) {
UI.groupsInited = true;
for (const [g, list] of sortedGroups) {
if (g === NON_TMUX_GROUP) { UI.collapsedGroups.add(g); continue; }
const hasSel = list.some((s) => s.session_id === activeRootId);
const hasBusy = list.some((s) => ['working', 'needs_permission', 'needs_input', 'error'].includes(s.state));
if (!hasSel && !hasBusy) UI.collapsedGroups.add(g);
}
saveCollapsedGroups();
}
const allGroupNames = sortedGroups.map(([g]) => g);
const allCollapsed = allGroupNames.length > 0 && allGroupNames.every((g) => UI.collapsedGroups.has(g));
const toggleAllGroups = () => {
if (allCollapsed) { UI.collapsedGroups.clear(); try { localStorage.setItem(NON_TMUX_OPEN_KEY, '1'); } catch { /* ignore */ } }
else { allGroupNames.forEach((g) => UI.collapsedGroups.add(g)); try { localStorage.setItem(NON_TMUX_OPEN_KEY, '0'); } catch { /* ignore */ } }
saveCollapsedGroups();
rerender();
};
W.sideCount.textContent = String(shown.length);
if (document.activeElement !== W.sideSearch && W.sideSearch.value !== draft('ws:filter')) W.sideSearch.value = draft('ws:filter');
W.sideShowEnded.checked = UI.showEnded;
W.sideToggleSlot.replaceChildren(allGroupNames.length > 1 ? btn(allCollapsed ? '全部展开' : '全部收起', { cls: 'ghost xs', onclick: toggleAllGroups }) : '');
W.sideChips.replaceChildren(...['', ...AGENTS].map((a) => h('button', {
type: 'button', class: `chip${UI.agentFilter === a ? ' on' : ''}`,
onclick: () => { UI.agentFilter = a; schedule(); },
}, a || '全部')));
const prevSideTop = W.sideList.scrollTop;
W.sideList.replaceChildren(...(sortedGroups.length ? sortedGroups.map(([g, list]) => {
const isCollapsed = !q && UI.collapsedGroups.has(g);
const attnSess = list.find((s) => ['needs_permission', 'needs_input', 'error'].includes(s.state))
|| list.find((s) => s.state === 'working');
return h('div', { class: 'ws-group' },
h('button', {
type: 'button', class: 'ws-group-btn', 'aria-expanded': String(!isCollapsed),
onclick: () => {
const willExpand = UI.collapsedGroups.has(g);
if (willExpand) UI.collapsedGroups.delete(g);
else UI.collapsedGroups.add(g);
if (g === NON_TMUX_GROUP) { try { localStorage.setItem(NON_TMUX_OPEN_KEY, willExpand ? '1' : '0'); } catch { /* ignore */ } }
saveCollapsedGroups();
rerender();
},
},
h('span', { class: 'row-nowrap', style: 'gap:4px;min-width:0' },
span('ws-group-caret', isCollapsed ? '▸' : '▾'),
h('span', { class: 'trunc' }, g),
isCollapsed && attnSess ? h('i', { class: `dot dot-${attnSess.state}`, title: stateText(attnSess.state) }) : null),
span('tag tabular', list.length)),
!isCollapsed ? list.map((s) => sidebarSessionItem(s)) : null);
}) : [emptyState(roots.length ? '没有匹配的会话' : '暂未发现活跃会话',
roots.length ? '尝试清空搜索关键词或切换 Agent 过滤条件。' : '启动中的 tmux 会话会自动同步到这里，也可直接新建一个交互会话。',
!roots.length ? btn('新建交互会话', { cls: 'primary sm', onclick: openSpawnModal }) : null)]));
W.sideList.scrollTop = prevSideTop;
if (!active) {
W.headEl.classList.add('ws-hidden');
W.crumbEl.classList.add('ws-hidden');
W.bodySplit.classList.add('ws-hidden');
W.emptyMain.classList.remove('ws-hidden');
return;
}
W.headEl.classList.remove('ws-hidden');
W.bodySplit.classList.remove('ws-hidden');
W.emptyMain.classList.add('ws-hidden');
const s = active;
const dir = s.git_root || s.cwd || '';
const meLabel = modelEffortLabel(s.model, s.effort);
const titleTip = [dir, s.tmux_target, s.pane, meLabel].filter(Boolean).join(' · ');
const parent = s.is_subagent && s.parent_session_id ? S.sessions.get(s.parent_session_id) : null;
const displayTitle = s.is_subagent ? subagentDisplayName(s) : sessionLabel(s);
const viewToggle = h('div', { class: 'view-seg', role: 'tablist', 'aria-label': '显示模式' },
h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'reading' ? 'on' : '', onclick: () => { UI.outputMode = 'reading'; rerender(); } }, '阅读'),
h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'raw' ? 'on' : '', onclick: () => { UI.outputMode = 'raw'; if (s.pane) loadPreview(s.pane, { lines: PREVIEW_LINES, silent: true }); rerender(); } }, '终端'));
W.headEl.replaceChildren(
h('div', { class: 'ws-head-left', title: titleTip },
h('h2', { class: 'ws-head-title' }, displayTitle),
agentTag(s.agent),
meLabel ? span('tag mono ws-model-pill', meLabel) : null,
statusInd(s.state, stateText(s.state))),
h('div', { class: 'ws-head-right' },
buildTelemetryStrip(s),
viewToggle,
btn('详情', { cls: 'sm', title: '展开右侧检查抽屉（切到 tmux / 刷新 / 性能与上下文 / 元数据）', onclick: () => openDrawer('session', { sessionId: s.session_id }) })));
if (s.is_subagent) {
W.crumbEl.classList.remove('ws-hidden');
W.crumbEl.replaceChildren(
h('span', { class: 'row' }, span('tag', '子会话'), h('span', null, `所属 ${parent ? sessionLabel(parent) : short(s.parent_session_id, 16)}`)),
s.parent_session_id ? btn('← 返回主会话', { cls: 'ghost xs', onclick: () => openSession(s.parent_session_id) }) : '');
} else {
W.crumbEl.classList.add('ws-hidden');
}
if (!S.transcripts.has(s.session_id)) loadTranscript(s.session_id, { silent: false });
if (s.pane && !S.previews.has(`${s.pane}:${PREVIEW_LINES}`)) loadPreview(s.pane, { lines: PREVIEW_LINES, silent: false });
loadSlashCommands(s.agent);
const built = buildOutputBody(s);
const sameView = W.vp.dataset.sid === s.session_id && W.vp.dataset.mode === UI.outputMode;
if (!sameView) {
W.vp.dataset.sid = s.session_id;
W.vp.dataset.mode = UI.outputMode;
W.vp.dataset.sig = built.sig;
W.vp.className = `term-viewport${UI.outputMode === 'raw' ? ' raw-mode' : ''}`;
W.vp.replaceChildren(built.node);
W.vp.scrollTop = W.vp.scrollHeight;
syncScrollLatestBtn(W.vp);
} else if (W.vp.dataset.sig !== built.sig) {
const atBottom = W.vp.scrollHeight - W.vp.scrollTop - W.vp.clientHeight < 44;
const prevTop = W.vp.scrollTop;
W.vp.dataset.sig = built.sig;
W.vp.className = `term-viewport${UI.outputMode === 'raw' ? ' raw-mode' : ''}`;
W.vp.replaceChildren(built.node);
W.vp.scrollTop = atBottom ? W.vp.scrollHeight : prevTop;
syncScrollLatestBtn(W.vp);
}
const prompts = collectUserPrompts(s);
const railSig = `${s.session_id}|${UI.outputMode}|${UI.outlineCollapsed}|${prompts.map((p) => `${p.idx}:${p.text}`).join('|')}`;
if (W.railSig !== railSig || (!UI.outlineCollapsed && prompts.length > 0 && UI.outputMode === 'reading' && !W.rail?.isConnected)) {
W.railSig = railSig;
const showReopen = UI.outputMode === 'reading' && prompts.length > 0 && UI.outlineCollapsed;
W.reopenBtn.classList.toggle('ws-hidden', !showReopen);
if (showReopen) {
W.reopenBtn.dataset.cnt = String(prompts.length);
W.reopenBtn.textContent = `« 目录 (${prompts.length})`;
}
if (W.rail) { W.rail.remove(); W.rail = null; }
const nextRail = outlineRailPane(s, prompts);
if (nextRail) { W.rail = nextRail; W.bodySplit.appendChild(nextRail); }
}
const C = W.comp;
const readOnlyMsg = s.is_subagent ? '只读子会话 · 无法直接在此发送消息，请返回主会话操作'
: (s.state === 'ended' ? '该会话已结束 · 请切换至活跃会话发送指令' : '');
if (readOnlyMsg) {
C.readonlyHint.textContent = readOnlyMsg;
C.readonlyHint.classList.remove('ws-hidden');
} else {
C.readonlyHint.classList.add('ws-hidden');
}
for (const b of C.modeBtns) {
b.classList.toggle('on', b.dataset.mode === UI.composerMode);
b.disabled = !!readOnlyMsg;
}
C.modeHint.textContent = readOnlyMsg || (UI.composerMode === 'direct' ? '输入 / 唤出命令 · Cmd+Enter 发送' : '可选模板 · 留空使用默认配置');
if (UI.composerMode === 'direct' || readOnlyMsg) {
C.directRow.classList.remove('ws-hidden');
C.pipeSlot.classList.add('ws-hidden');
if (C.sid !== s.session_id) {
if (C.sid) UI.drafts.set(`ws:msg:${C.sid}`, C.ta.value);
C.sid = s.session_id;
C.agent = s.agent;
C.ta.dataset.draft = `ws:msg:${s.session_id}`;
C.ta.value = draft(`ws:msg:${s.session_id}`);
C.ta.style.height = 'auto';
autoGrow(C.ta);
closeSlashMenu();
} else {
C.agent = s.agent;
}
const ph = readOnlyMsg || `发消息给 ${sessionLabel(s)}…（输入 / 唤出命令，Cmd+Enter 发送）`;
if (C.ta.placeholder !== ph) C.ta.placeholder = ph;
const sending = UI.busy.has(`send:${s.session_id}`);
C.ta.disabled = !!readOnlyMsg;
C.imgBtn.disabled = !!readOnlyMsg;
C.sendBtn.classList.toggle('is-busy', sending);
C.sendBtn.disabled = sending || !!readOnlyMsg;
renderAttachBar(s.session_id);
} else {
C.directRow.classList.add('ws-hidden');
C.attachBar.classList.add('ws-hidden');
C.pipeSlot.classList.remove('ws-hidden');
closeSlashMenu();
C.pipeSlot.replaceChildren(workspacePipelineComposer(s, UI.composerMode));
}
}
function sidebarSessionItem(s) {
const curId = currentSession()?.session_id;
const sel = curId === s.session_id;
const subs = subagentsOf(s.session_id).filter((c) => UI.showEnded || c.state !== 'ended');
const subCount = subs.length || s.subagent_count || 0;
const subOpenKey = `subs:${s.session_id}`;
const childSelected = subs.some((c) => c.session_id === curId);
const subsOpen = childSelected || UI.open.has(subOpenKey);
const pctVal = ctxPctOf(s.metrics);
const tpsVal = tpsOf(s.metrics);
const tone = ctxTone(pctVal);
const primaryTitle = s.tmux_label || s.tmux_target || base(s.git_root || s.cwd);
const subMeta = s.tmux_label && s.tmux_target ? s.tmux_target : base(s.git_root || s.cwd);
const meLabel = modelEffortLabel(s.model, s.effort);
const card = h('button', {
type: 'button', class: `ws-item${sel ? ' sel' : ''}`,
onclick: () => openSession(s.session_id),
},
h('div', { class: 'ws-item-top' },
h('i', { class: `dot dot-${s.state}`, title: stateText(s.state) }),
agentTag(s.agent),
h('span', { class: 'ws-item-target grow', title: primaryTitle }, primaryTitle),
s.pane ? span('mono dim', s.pane) : null),
h('div', { class: 'ws-item-sub' },
h('span', { class: 'trunc grow' }, subMeta),
s.discovered_only ? span('tag legacy', 'tmux 发现') : null,
subCount ? span('tag tabular', `${subCount} 子会话`) : null),
meLabel ? h('div', { class: 'ws-item-model', title: meLabel }, meLabel) : null,
(pctVal != null || tpsVal != null) ? h('div', { class: 'ws-item-metrics' },
pctVal != null ? h('span', { class: `row-nowrap${tone === 'high' ? ' ws-tel-high' : tone === 'warn' ? ' ws-tel-warn' : ''}`, style: 'gap:5px' },
`CTX ${Math.round(pctVal)}%`, ctxBar(pctVal, true)) : null,
tpsVal != null ? span('muted', `${tpsVal.toFixed(1)} tok/s`) : null) : null,
s.last_message ? h('div', { class: 'ws-item-msg' }, s.last_message) : null);
if (!subs.length) return card;
const subTree = h('div', { class: 'ws-sub-wrap' },
h('button', {
type: 'button', class: 'ws-sub-toggle',
onclick: () => {
if (UI.open.has(subOpenKey)) UI.open.delete(subOpenKey);
else UI.open.add(subOpenKey);
rerender();
},
}, `↳ ${subs.length} 个子会话 ${subsOpen ? '▾' : '▸'}`),
subsOpen ? subs.map((c) => h('button', {
type: 'button', class: `ws-sub-item${curId === c.session_id ? ' sel' : ''}`,
onclick: () => openSession(c.session_id),
},
h('i', { class: `dot dot-${c.state}`, title: stateText(c.state) }),
h('span', { class: 'trunc grow' }, subagentDisplayName(c)),
h('span', { class: 'dim', style: 'font-size:11px' }, stateText(c.state)))) : null);
return h('div', { class: 'stack-xs', style: 'gap:2px' }, card, subTree);
}
const AGENT_DISPLAY = { claude: 'Claude', codex: 'Codex', agy: 'AGY' };
function collectUserPrompts(s) {
const tr = S.transcripts.get(s.session_id);
let msgs = [];
if (tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0) {
msgs = tr.messages;
} else if (s.pane) {
const pv = S.previews.get(`${s.pane}:${PREVIEW_LINES}`);
if (pv?.text?.trim()) msgs = parseTerminalTurns(pv.text, s.agent);
}
const out = [];
let uIdx = 0;
for (const m of msgs) {
if (m.role === 'user') {
uIdx++;
const preview = String(m.text || '').replace(/\s+/g, ' ').trim();
out.push({ idx: uIdx, text: preview || '（空消息）', ts_ms: m.ts_ms || null });
}
}
return out;
}
function jumpToPrompt(userIdx) {
if (UI.outputMode !== 'reading') { UI.outputMode = 'reading'; rerender(); }
UI.jumpLockUntil = Date.now() + 700;
for (const it of document.querySelectorAll('.outline-item[data-turn-idx]')) {
it.classList.toggle('active', it.dataset.turnIdx === String(userIdx));
}
requestAnimationFrame(() => {
const target = document.getElementById(`turn-u-${userIdx}`);
if (!target) return;
target.scrollIntoView({ behavior: 'smooth', block: 'center' });
target.classList.remove('turn-flash');
void target.offsetWidth;
target.classList.add('turn-flash');
setTimeout(() => target.classList.remove('turn-flash'), 1250);
});
}
function setOutlineCollapsed(val) {
UI.outlineCollapsed = !!val;
try { localStorage.setItem(OUTLINE_COLLAPSED_KEY, val ? '1' : '0'); } catch { /* ignore */ }
rerender();
}
function outlineRailPane(s, prompts) {
if (UI.outputMode !== 'reading' || !prompts.length || UI.outlineCollapsed) return null;
const fk = `pjump:${s.session_id}`;
const listEl = h('div', { class: 'outline-list' });
const renderList = () => {
const q = draft(fk).trim().toLowerCase();
const filtered = q ? prompts.filter((p) => p.text.toLowerCase().includes(q) || `#${p.idx}`.includes(q)) : prompts;
listEl.replaceChildren(...(filtered.length ? filtered.map((p, i) => h('button', {
type: 'button',
class: `outline-item${i === filtered.length - 1 ? ' active' : ''}`,
'data-turn-idx': String(p.idx),
onclick: () => jumpToPrompt(p.idx),
},
h('div', { class: 'outline-item-top' },
span('outline-item-idx', `#${p.idx}`),
p.ts_ms ? span('tabular', clock(p.ts_ms)) : null),
h('div', { class: 'outline-item-text' }, p.text)))
: [h('div', { class: 'muted', style: 'padding:12px;font-size:12px;text-align:center' }, '无匹配提问')]));
};
renderList();
const filterInp = prompts.length > 6 ? (() => {
const inp = field(fk, { class: 'inp sm', type: 'search', placeholder: '过滤提问…', autocomplete: 'off' });
inp.addEventListener('input', renderList);
return inp;
})() : null;
return h('aside', { class: 'outline-rail', 'aria-label': '提问目录', 'data-cnt': String(prompts.length) },
h('div', { class: 'outline-head' },
h('div', { class: 'outline-head-row' },
span('outline-title', `提问目录 · ${prompts.length}`),
btn('»', { cls: 'ghost xs', title: '收起提问目录', onclick: () => setOutlineCollapsed(true) })),
filterInp),
listEl);
}
function ackSessionError(s) {
if (!s) return;
return act(`ack:${s.session_id}`, async () => {
const updated = await api('POST', `/api/sessions/${enc(s.session_id)}/ack`, {});
if (updated?.session_id) upsertSession(updated);
syncStateSilent();
return updated;
}, '已清除异常状态');
}
function sessionAlertBanner(s) {
if (!s || (s.state !== 'error' && !s.last_error)) return null;
const detail = s.last_error || 'Agent 上报了错误状态，请查看下方终端实时行或事件记录。';
return h('div', { class: 'callout err' },
h('div', { class: 'stack-xs grow' },
h('strong', null, `会话${stateText(s.state)}`),
h('span', null, detail)),
btn('清除异常状态', { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => ackSessionError(s) }));
}
function buildOutputBody(s) {
const mode = UI.outputMode;
const pane = s.pane;
const pvKey = pane ? `${pane}:${PREVIEW_LINES}` : null;
const pv = pvKey ? S.previews.get(pvKey) : null;
const tr = S.transcripts.get(s.session_id);
const errSig = `${s.state}:${hashStr(s.last_error || '')}`;
const alertBanner = sessionAlertBanner(s);
if (mode === 'raw') {
if (!pane) return { sig: `raw:nopane:${s.session_id}:${errSig}`, sourceLabel: '原始终端', node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, emptyState('该会话未绑定 tmux pane', s.last_message || '无法抓屏预览原始终端。')) };
if (!pv || (pv.state === 'loading' && !pv.text)) return { sig: `raw:loading:${pane}:${errSig}`, sourceLabel: '原始终端', node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, skeleton(6)) };
if (pv.state === 'err' && !pv.text) {
return {
sig: `raw:err:${pane}:${pv.err}:${errSig}`, sourceLabel: '原始终端',
node: h('div', { class: 'raw-shell stack-sm' },
alertBanner,
errorBanner(`读取终端失败：${pv.err}`, '重试获取终端', () => loadPreview(pane, { lines: PREVIEW_LINES, silent: false })),
s.last_message ? h('div', { class: 'callout' }, h('div', { class: 'sec-h' }, '最近事件摘要'), renderMarkdown(s.last_message)) : null),
};
}
if (!pv.text.trim()) return { sig: `raw:empty:${pane}:${errSig}`, sourceLabel: '原始终端', node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, emptyState('终端暂无可见输出', s.last_message || '向下方输入框发送一条消息试试。')) };
return { sig: `raw:ok:${pane}:${pv.hash}:${errSig}`, sourceLabel: '原始终端', node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, h('pre', { class: 'term-pre' }, stripAnsi(pv.text))) };
}
if (tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0) {
const tailText = pv?.text ? stripAnsi(pv.text).trim().split('\n').slice(-10).join('\n') : '';
const tailKey = `tail:${s.session_id}`;
const autoOpenTail = sessionNeedsLiveTail(s, tr);
const timeline = h('div', { class: 'timeline' },
alertBanner,
renderTimelineCards(s, tr.messages),
tailText ? h('details', { class: 'live-tail', 'data-k': tailKey, open: UI.open.has(tailKey) || autoOpenTail },
h('summary', null, `当前终端实时行（${pane || 'pane'}）`),
h('pre', { class: 'term-pre' }, tailText)) : null);
return { sig: `read:tr:${s.session_id}:${tr.hash}:${hashStr(tailText)}:${errSig}:${autoOpenTail ? 1 : 0}`, sourceLabel: '对话记录', node: timeline };
}
if (!pane) {
if (tr?.state === 'loading') return { sig: `read:trload:${s.session_id}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, skeleton(5)) };
if (s.last_message) return { sig: `read:last:${s.session_id}:${hashStr(s.last_message)}:${errSig}`, sourceLabel: '最近摘要', node: h('div', { class: 'timeline' }, alertBanner, renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0)) };
return { sig: `read:empty:${s.session_id}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, emptyState('暂无可阅读内容', '该会话尚未产生对话记录且未绑定 tmux pane。')) };
}
if ((!pv || (pv.state === 'loading' && !pv.text)) && (!tr || tr.state === 'loading')) return { sig: `read:loading:${s.session_id}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, skeleton(6)) };
if (pv?.state === 'err' && !pv.text) {
return {
sig: `read:err:${pane}:${pv.err}:${errSig}`, sourceLabel: '终端 Markdown',
node: h('div', { class: 'timeline' },
alertBanner,
errorBanner(`读取终端失败：${pv.err}`, '重试获取终端', () => loadSessionOutput(s, { silent: false })),
s.last_message ? renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0) : null),
};
}
if (!pv?.text?.trim()) return { sig: `read:empty:${pane}:${errSig}`, sourceLabel: '终端 Markdown', node: h('div', { class: 'timeline' }, alertBanner, emptyState('终端暂无可见输出', s.last_message || '向下方输入框发送一条消息试试。')) };
const turns = parseTerminalTurns(pv.text, s.agent);
if (!turns.length) return { sig: `read:rawfallback:${pane}:${pv.hash}:${errSig}`, sourceLabel: '终端 Markdown', node: h('div', { class: 'timeline' }, alertBanner, h('article', { class: 'msg-card assistant' }, h('pre', { class: 'term-pre' }, stripAnsi(pv.text)))) };
return { sig: `read:pv:${pane}:${pv.hash}:${errSig}`, sourceLabel: '终端 Markdown', node: h('div', { class: 'timeline' }, alertBanner, renderTimelineCards(s, turns)) };
}
function renderTimelineCards(s, list) {
let uIdx = 0;
return list.map((msg, idx) => renderMessageCard(s, msg, idx, msg.role === 'user' ? ++uIdx : null));
}
function renderMessageCard(s, msg, idx, userIdx = null) {
const isUser = msg.role === 'user';
const tools = Array.isArray(msg.tools) ? msg.tools.filter(Boolean) : [];
const tKey = `tools:${s.session_id}:${idx}:${msg.ts_ms || 0}`;
const agentName = AGENT_DISPLAY[s.agent] || s.agent || 'Agent';
return h('article', {
class: `msg-card ${isUser ? 'user' : 'assistant'}`,
id: isUser && userIdx != null ? `turn-u-${userIdx}` : null,
},
h('div', { class: 'msg-head' },
isUser
? h('span', { class: 'row' }, span('msg-user-tag', '你'), userIdx != null ? span('turn-idx', `#${userIdx}`) : null)
: h('span', { class: 'row' },
h('i', { class: `dot agent-dot-${s.agent || 'x'}` }),
span(`tag a-${s.agent || 'x'}`, agentName),
modelEffortLabel(s.model, s.effort) ? span('mono muted', modelEffortLabel(s.model, s.effort)) : null),
h('span', { class: 'row' }, msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null)),
msg.text ? renderMarkdown(msg.text) : (tools.length ? h('div', { class: 'muted', style: 'font-size:13px' }, '正在调用工具…') : null),
tools.length ? h('details', { class: 'msg-tools', 'data-k': tKey, open: UI.open.has(tKey) },
h('summary', null, `调用了 ${tools.length} 个工具`),
h('div', { class: 'msg-tool-list' }, tools.map((t) => span('tag mono', t)))) : null);
}
function workspacePipelineComposer(s, mode) {
const isDebug = mode === 'debug';
const scope = `ws:${mode}`;
const goalKey = `${scope}:goal`;
const bindKey = `${scope}:bind`;
const projKey = `${scope}:proj`;
const defaultProj = s.git_root || s.cwd || knownProjects()[0] || '';
const pv = UI.parsePreviews[scope];
const bindInp = field(bindKey, {
class: 'inp sm', autocomplete: 'off',
placeholder: isDebug ? '选填：如 快速排查用 agy，深挖根因用 codex' : '选填：如 计划用 claude，实现用 codex，Review 用 agy',
});
bindInp.addEventListener('input', () => queueParse(scope, mode, projKey, bindKey, defaultProj));
const projInp = field(projKey, {
class: 'inp sm mono', autocomplete: 'off', placeholder: '选填项目目录（留空使用当前会话目录）',
}, defaultProj);
const ta = field(goalKey, {
tag: 'textarea', rows: 2, class: 'composer-ta',
placeholder: isDebug ? '描述故障现象或报错（Cmd+Enter 启动两速查错）…' : '描述需求目标（Cmd+Enter 启动新功能协作）…',
});
const launch = () => act(`launch:${scope}`, async () => {
const goal = draft(goalKey).trim();
const utt = draft(bindKey).trim();
const proj = draft(projKey, defaultProj).trim();
const combined = [goal, utt].filter(Boolean).join('；');
if (!combined) throw new Error(isDebug ? '请填写故障现象或排查说明' : '请填写需求目标或协作说明');
const parsed = pv?.data && pv.text === utt ? pv.data : null;
let p;
if (isDebug) {
p = await api('POST', '/api/debug', {
project_root: proj,
symptom: goal || parsed?.goal || combined,
utterance: utt || combined,
auto_confirm: UI.autoConfirm,
});
} else {
p = await api('POST', '/api/pipelines', {
req: {
playbook: 'feature',
project_root: proj,
goal: goal || parsed?.goal || combined,
bindings: parsed?.bindings || {},
source: { via: 'web', utterance: combined },
},
auto_confirm: UI.autoConfirm,
});
}
p = upsertPipeline(p.pipeline || p);
if (UI.autoConfirm && p.status === 'awaiting_binding_confirm') {
p = upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, {}));
}
UI.drafts.delete(goalKey); UI.drafts.delete(bindKey); delete UI.parsePreviews[scope];
go('pipelines', p.id);
return p;
}, (p) => `${p.id} 已启动`);
ta.addEventListener('keydown', (e) => {
if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !e.isComposing) { e.preventDefault(); launch(); }
});
const parsedStrip = pv?.data ? h('div', { class: 'stack-xs' },
h('div', { class: 'row' },
span('muted', '解析预览：'),
...PLAYBOOK_STAGES[mode].map((k) => span('tag', `${stageName(k)}: ${shorthand(pv.data.resolved?.[k])}`))),
(pv.data.warnings || []).length ? h('div', { class: 'callout warn' }, pv.data.warnings.map((w) => h('div', null, warnText(w)))) : null) : null;
return h('div', { class: 'stack-sm' },
h('div', { class: 'opt-panel' },
h('div', { class: 'opt-grid' },
h('div', null, h('label', { class: 'lbl' }, '模型绑定话术（选填）'), bindInp),
h('div', null, h('label', { class: 'lbl' }, '项目目录（选填）'), projInp)),
parsedStrip,
h('label', { class: 'row muted', style: 'font-size:12px;cursor:pointer' },
h('input', { type: 'checkbox', checked: UI.autoConfirm, onchange: (e) => { UI.autoConfirm = e.target.checked; schedule(); } }),
'跳过二次绑定确认，创建后直接启动首阶段')),
ta,
h('div', { class: 'composer-foot' },
btn('切回直接发送', { cls: 'ghost sm', onclick: () => { UI.composerMode = 'direct'; rerender(); } }),
btn(isDebug ? '启动两速查错' : '启动新功能协作', { cls: 'primary sm', busyKey: `launch:${scope}`, onclick: launch })));
}
const parseTimers = {}; const parseSeq = {};
function queueParse(scope, playbook, projKey, textKey, defaultProj) {
clearTimeout(parseTimers[scope]);
const text = draft(textKey).trim();
if (!text) { delete UI.parsePreviews[scope]; schedule(); return; }
parseTimers[scope] = setTimeout(async () => {
const seq = (parseSeq[scope] = (parseSeq[scope] || 0) + 1);
const project = draft(projKey, defaultProj || '').trim();
try {
const data = await api('POST', '/api/pipelines/parse', { text, playbook, ...(project ? { project } : {}) });
if (seq === parseSeq[scope]) UI.parsePreviews[scope] = { text, data };
} catch (e) {
if (seq === parseSeq[scope]) UI.parsePreviews[scope] = { text, err: errText(e) };
}
schedule();
}, PARSE_DEBOUNCE_MS);
}
function pipelineAction(p, payload, okMsg) {
return act(`pa:${p.id}:${payload.action}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/action`, payload)), okMsg);
}
function debugAction(p, verb, okMsg) {
return act(`pa:${p.id}:${verb}`, async () => upsertPipeline(await api('POST', `/api/debug/${enc(p.id)}/${verb}`, {})), okMsg);
}
const cancelPipeBtn = (p) => btn('取消流程', {
cls: 'ghost sm danger', busyKey: `pa:${p.id}:cancel`,
onclick: () => { if (window.confirm(`确认取消流程 ${p.id}？`)) pipelineAction(p, { action: 'cancel' }, '流程已取消'); },
});
function bindingEditor(p, scope = 'bind') {
const rows = (p.stages || []).map((st) => {
const b = st.binding || {}; const k = `${scope}:${p.id}:${st.id}`;
const agentOpts = [...(b.agent ? [] : [['', b.same_as ? `同 ${stageName(b.same_as)}` : '默认']]), ...AGENTS.map((a) => [a, a])];
return h('tr', null,
h('td', { style: 'font-weight:600;white-space:nowrap' }, stageName(st.id)),
h('td', null, selectField(`${k}:agent`, agentOpts, b.agent || '', { class: 'inp sm' })),
h('td', null, field(`${k}:model`, { class: 'inp sm mono', placeholder: '默认模型', autocomplete: 'off' }, b.model || '')),
h('td', null, selectField(`${k}:effort`, [['', '—'], ...EFFORTS.map((e) => [e, e])], b.effort || '', { class: 'inp sm' })),
h('td', null, h('div', { class: 'row' },
st.id === 'test_review' ? selectField(`${k}:depth`, [['', 'D1'], ...DEPTHS.map((d) => [d, d])], b.depth || '', { class: 'inp sm' }) : null,
b.native ? span('tag soft-plum', `/${b.native}`) : null)));
});
return h('div', { class: 'table-wrap' }, h('table', { class: 'tbl' },
h('thead', null, h('tr', null, ['阶段', 'Agent', 'Model', 'Effort', 'Depth / 原生模式'].map((x) => h('th', null, x)))),
h('tbody', null, rows)));
}
function collectOverrides(p, scope = 'bind') {
const out = {};
for (const st of p.stages || []) {
const b = st.binding || {}; const k = `${scope}:${p.id}:${st.id}`;
const agent = draft(`${k}:agent`, b.agent || ''); const model = draft(`${k}:model`, b.model || '').trim();
const effort = draft(`${k}:effort`, b.effort || ''); const depth = draft(`${k}:depth`, b.depth || '');
if (agent !== (b.agent || '') || model !== (b.model || '') || effort !== (b.effort || '') || depth !== (b.depth || '')) {
out[st.id] = { ...b, agent: agent || null, model: model || null, effort: effort || null, depth: depth || null, same_as: agent ? null : (b.same_as ?? null) };
}
}
return out;
}
function confirmBindingBar(p, scope = 'bind') {
return h('div', { class: 'row', style: 'justify-content:space-between;margin-top:12px' },
span('muted', '可直接微调各阶段绑定，未修改项保持默认'),
h('div', { class: 'row' },
cancelPipeBtn(p),
btn('确认并启动流程', {
cls: 'primary sm', busyKey: `confirm:${p.id}`,
onclick: () => {
const overrides = collectOverrides(p, scope);
act(`confirm:${p.id}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, Object.keys(overrides).length ? { overrides } : {})), '流程已启动');
},
})));
}
function planActionBar(p) {
const nk = `chg:${p.id}`;
return h('div', { class: 'stack-sm', style: 'margin-top:12px' },
field(nk, { class: 'inp sm', placeholder: '如需退回修改，请在此填写修改说明…' }),
h('div', { class: 'row', style: 'justify-content:flex-end' },
cancelPipeBtn(p),
btn('退回并要求修改', {
cls: 'sm', busyKey: `pa:${p.id}:request_changes`,
onclick: () => {
const note = draft(nk).trim();
if (!note) { toast('请先填写修改说明', 'warn'); return; }
pipelineAction(p, { action: 'request_changes', note }, '已退回计划并附修改意见').then((r) => { if (r) UI.drafts.delete(nk); });
},
}),
btn('批准计划并开始实现', { cls: 'primary sm', busyKey: `pa:${p.id}:approve_plan`, onclick: () => pipelineAction(p, { action: 'approve_plan' }, '计划已批准，进入实现阶段') })));
}
function triageActionBar(p) {
return h('div', { class: 'row', style: 'justify-content:flex-end;margin-top:12px' },
cancelPipeBtn(p),
btn('停止深挖', { cls: 'ghost sm', busyKey: `pa:${p.id}:stop`, onclick: () => debugAction(p, 'stop', '已停止深挖') }),
btn('采用止血方案', { cls: 'sm', busyKey: `pa:${p.id}:mitigate`, onclick: () => debugAction(p, 'mitigate', '已采用止血建议') }),
btn('继续深挖根因', { cls: 'primary sm', busyKey: `pa:${p.id}:continue`, onclick: () => debugAction(p, 'continue', '已进入 Phase B 深挖') }));
}
function quickDispatchRow(key, target, placeholder = '直接向该会话发送回复…', btnCls = 'primary sm') {
const k = `qmsg:${key}`;
const inp = field(k, { class: 'inp sm', placeholder, autocomplete: 'off' });
const send = () => {
const text = (draft(k) || inp.value || '').trim();
if (!text) return;
act(k, async () => {
const r = await api('POST', '/api/dispatch', { target, text, interactive: true });
UI.drafts.delete(k);
return r;
}, (r) => ({ msg: `已发送：${deliveryText(r)}`, kind: r && (r.status === 'failed' || r.status === 'unconfirmed') ? 'warn' : 'ok' }));
};
inp.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) { e.preventDefault(); send(); } });
return h('div', { class: 'row-nowrap', style: 'margin-top:10px' }, inp, btn('发送到会话', { cls: btnCls, busyKey: k, onclick: send }));
}
const KIND_INFO = {
session_alert: ['会话等待响应', 'needs_permission'],
binding_confirm: ['绑定待确认', 'awaiting_binding_confirm'],
plan_approval: ['计划待批准', 'awaiting_human'],
triage_ready: ['快筛结论待决', 'awaiting_human'],
blocked: ['流程阻塞', 'blocked'],
needs_human_finding: ['评审待裁决', 'needs_human'],
delivery_unconfirmed: ['投递未确认', 'needs_permission'],
};
function viewInbox() {
const items = inboxItems();
return h('div', { class: 'stack' },
h('div', { class: 'page-head' },
h('div', null, h('h1', null, '待处理'), h('p', null, '需要人工确认或响应的事项 —— 权限审批、计划确认、快筛决策与评审裁决')),
span('status-ind pill-soft', h('i', { class: `dot ${items.length ? 'dot-needs_permission' : 'dot-done'}` }), `${items.length} 项待处理`)),
items.length
? items.map(inboxCard)
: h('div', { class: 'card' }, emptyState('当前没有待处理事项', '当会话等待审批、计划待确认或排查得出初步假设时，会实时出现在这里。',
btn('返回会话对话', { cls: 'sm', onclick: () => go('workspace') }))));
}
function inboxCard(it) {
const [kLabel, dotKind] = KIND_INFO[it.kind] || [it.kind, 'awaiting_human'];
const p = it.pid ? S.pipelines.get(it.pid) : null;
const s = it.sid ? S.sessions.get(it.sid) : null;
let body = null;
if (it.kind === 'binding_confirm' && p) {
body = [bindingEditor(p), confirmBindingBar(p)];
} else if (it.kind === 'plan_approval' && p) {
const aid = lastArtifact(stageOf(p, 'plan'));
const a = aid ? artifact(aid) : null;
body = [
a?.state === 'ok' && a.data
? h('div', { class: 'stack-sm' },
kvGrid([['目标', a.data.goal], ['任务数', `${(a.data.tasks || []).length} 项`], ['验收标准', plainList(a.data.acceptance)]]),
h('div', { class: 'row' }, btn('在抽屉查看完整设计', { cls: 'sm', onclick: () => openDrawer('artifact', { artifactId: aid, title: `计划设计 · ${p.id}` }) })))
: (aid ? skeleton(3) : h('div', { class: 'muted' }, '计划工件尚未生成')),
planActionBar(p),
];
} else if (it.kind === 'triage_ready' && p) {
const aid = lastArtifact(stageOf(p, 'triage'));
const a = aid ? artifact(aid) : null;
body = [
a?.state === 'ok' && a.data
? h('div', { class: 'stack-sm' },
...(a.data.hypotheses || []).map(hypothesisCompact),
a.data.mitigation ? h('div', { class: 'mitig-box' }, h('strong', null, '止血建议：'), a.data.mitigation) : null,
h('div', { class: 'row' }, btn('查看完整快筛证据', { cls: 'sm', onclick: () => openDrawer('artifact', { artifactId: aid, title: `快筛报告 · ${p.id}` }) })))
: (aid ? skeleton(3) : h('div', { class: 'muted' }, '快筛工件加载中…')),
triageActionBar(p),
];
} else if (it.kind === 'needs_human_finding') {
const rev = ensureReview(it.rid) || S.reviews.get(it.rid);
const pend = rev ? (rev.findings || []).filter((f) => f.falsifier?.verdict === 'needs_human' && !f.feedback) : [];
body = rev
? h('div', { class: 'stack-sm' },
pend.length ? pend.map((af) => findingRow(rev, af)) : h('div', { class: 'muted' }, '该评审下待裁决条目已全部处理'),
h('div', { class: 'row' }, btn('打开代码评审页', { cls: 'sm', onclick: () => go('reviews', it.rid) })))
: skeleton(3);
} else if (s) {
body = [
s.last_error ? h('div', { class: 'callout err' }, span(null, s.last_error)) : null,
s.last_message ? h('div', { class: 'callout' }, renderMarkdown(s.last_message)) : null,
h('div', { class: 'row' },
btn('在会话对话中打开', { cls: 'sm', onclick: () => openSession(s.session_id) }),
s.pane ? focusBtn(s.session_id) : null,
(s.state === 'error' || s.last_error) ? btn('清除异常状态', { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => ackSessionError(s) }) : null),
quickDispatchRow(`inb:${s.session_id}`, { type: 'session', id: s.session_id }),
];
} else if (p) {
const st = activeStage(p);
body = [
stageStepper(p, st?.id, null),
h('div', { class: 'row', style: 'justify-content:flex-end' },
cancelPipeBtn(p),
st ? btn(`重试 ${stageName(st.id)}`, { cls: 'primary sm', onclick: () => pipelineAction(p, { action: 'retry_stage', stage: st.id }, `已重试 ${stageName(st.id)}`) }) : null),
];
}
return h('article', { class: 'card' },
h('div', { class: 'card-head' },
h('div', { class: 'grow' },
h('div', { class: 'card-title' }, it.title || kLabel),
h('div', { class: 'card-meta' },
statusInd(dotKind, kLabel),
p ? h('a', { class: 'mono', href: `#/pipelines/${enc(p.id)}` }, p.id) : null,
p ? span('muted', base(p.project?.root)) : null,
s ? [agentTag(s.agent), s.pane ? span('mono', s.pane) : null] : null,
span('tabular', `已等待 ${dur(Date.now() - (it.since || Date.now()))}`)))),
body);
}
function viewPipelines() {
const all = allPipelines();
const f = UI.pipeFilter;
const list = all.filter((p) => {
if (f === 'done') return isFinished(p);
if (isFinished(p)) return false;
if (f === 'feature') return p.playbook === 'feature';
if (f === 'debug') return p.playbook === 'debug';
return true;
});
const activeCount = all.filter((p) => !isFinished(p)).length;
const doneCount = all.filter(isFinished).length;
if (UI.arg) {
setTimeout(() => {
const n = document.getElementById(`pl-${UI.arg}`);
if (n && !n.dataset.scrolled) { n.dataset.scrolled = '1'; n.scrollIntoView({ block: 'start' }); }
}, 60);
}
const filters = [
['all', `进行中（${activeCount}）`],
['feature', '功能协作'],
['debug', '两速查错'],
['done', `已结束（${doneCount}）`],
];
return h('div', { class: 'stack' },
h('div', { class: 'page-head' },
h('div', null, h('h1', null, '流水线'), h('p', null, 'Feature Pipeline 与 Two-Speed Debug 的阶段进度与产物记录')),
h('div', { class: 'row' },
btn('发起新功能协作', { cls: 'sm', onclick: () => { UI.modal = 'feature'; schedule(); } }),
btn('发起两速查错', { cls: 'sm', onclick: () => { UI.modal = 'debug'; schedule(); } }))),
h('div', { class: 'chips' }, filters.map(([k, l]) => h('button', {
type: 'button', class: `chip${f === k ? ' on' : ''}`,
onclick: () => { UI.pipeFilter = k; schedule(); },
}, l))),
list.length
? list.map((p) => (p.playbook === 'debug' ? debugPipelineCard(p) : featurePipelineCard(p)))
: h('div', { class: 'card' }, emptyState('当前筛选下没有流水线', '你可以在「会话对话」底部直接切换候选模式发起协作或查错，也可以点右上角按钮创建。')));
}
function stageStepper(p, selKind, onSelect) {
return h('ol', { class: 'stepper' }, (p.stages || []).map((st) => {
const end = st.ended_ms || (['running', 'awaiting_human'].includes(st.status) ? Date.now() : null);
return h('li', {
class: `step${st.id === selKind ? ' sel' : ''}`,
tabindex: onSelect ? 0 : null,
onclick: onSelect ? () => onSelect(st.id) : null,
onkeydown: onSelect ? (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect(st.id); } } : null,
},
h('div', { class: 'step-top' }, h('span', { class: 'step-name' }, stageName(st.id)), h('i', { class: `dot dot-${st.status}`, title: stateText(st.status) })),
h('div', { class: 'step-bind' }, shorthand(st.binding)),
h('div', { class: 'step-meta' }, stateText(st.status), st.round ? ` · R${st.round}` : '', st.started_ms && end ? ` · ${dur(end - st.started_ms)}` : '', st.pane ? ` · ${st.pane}` : ''));
}));
}
function featurePipelineCard(p) {
const sel = UI.selStage[p.id] || activeStage(p)?.id;
const st = stageOf(p, sel);
const aid = lastArtifact(st);
const target = st && (st.session_id || st.pane) ? { type: 'stage', pipeline_id: p.id, stage: st.id } : null;
let actionArea = null;
if (p.status === 'awaiting_binding_confirm') actionArea = [bindingEditor(p), confirmBindingBar(p)];
else if (p.status === 'awaiting_human' && stageOf(p, 'plan')?.status === 'awaiting_human') actionArea = planActionBar(p);
else if (p.status === 'running') actionArea = h('div', { class: 'row', style: 'justify-content:flex-end;margin-top:10px' }, cancelPipeBtn(p));
return h('article', { class: `card${UI.arg === p.id ? ' focus-ring' : ''}`, id: `pl-${p.id}` },
h('div', { class: 'card-head' },
h('div', { class: 'grow' },
h('div', { class: 'card-title' }, p.goal || '未命名功能协作'),
h('div', { class: 'card-meta' },
h('code', null, p.id),
statusInd(p.status),
span('tag', '功能协作'),
span('muted', base(p.project?.root)),
p.options?.review_depth ? span('tag soft-plum mono', p.options.review_depth) : null,
span('tabular', `更新于 ${ago(p.updated_ms)}`))),
btn('流程元数据', { cls: 'ghost sm', onclick: () => openDrawer('pipeline', { pipelineId: p.id }) })),
stageStepper(p, sel, (k) => { UI.selStage[p.id] = k; schedule(); }),
st ? h('div', { class: 'stage-strip' },
h('div', { class: 'row' },
h('strong', null, stageName(st.id)),
statusInd(st.status),
st.binding?.agent ? agentTag(st.binding.agent) : null,
modelTag(st.binding?.model),
st.binding?.native ? span('tag soft-plum', `/${st.binding.native}`) : null,
st.pane ? span('tag mono', st.pane) : null),
h('div', { class: 'row' },
st.session_id ? btn('打开会话', { cls: 'ghost sm', onclick: () => openSession(st.session_id) }) : null,
aid ? btn('查看阶段工件', { cls: 'sm', onclick: () => openDrawer('artifact', { artifactId: aid, title: `${stageName(st.id)} · ${p.id}` }) }) : span('muted', '暂无工件'))) : null,
target && ['running', 'awaiting_human', 'blocked'].includes(st.status)
? quickDispatchRow(`st:${p.id}:${st.id}`, target, `给 ${stageName(st.id)} 阶段会话发消息…`, p.status === 'awaiting_human' ? 'sm' : 'primary sm') : null,
actionArea);
}
function hypothesisCompact(hy, i) {
return h('div', { class: 'hyp-card' },
h('div', { class: 'row', style: 'justify-content:space-between' },
span('tag soft-amber', `假设 #${i + 1} · 未证实`),
span('muted tabular', bar(hy.confidence, hy.confidence >= 0.7 ? 'hi' : hy.confidence >= 0.4 ? 'mid' : 'low'), ` ${pct(hy.confidence)}`)),
renderMarkdown(hy.claim),
hy.how_to_confirm ? h('div', { class: 'muted', style: 'font-size:12.5px' }, '验证思路：', renderInlineMd(hy.how_to_confirm)) : null);
}
function debugPipelineCard(p) {
const tri = stageOf(p, 'triage'); const deep = stageOf(p, 'deep');
const triId = lastArtifact(tri); const deepId = lastArtifact(deep);
const triArt = triId ? artifact(triId) : null;
const deepArt = deepId ? artifact(deepId) : null;
const canDecide = (p.status === 'awaiting_human' && tri?.status === 'awaiting_human') || (tri?.status === 'done' && deep?.status === 'pending' && p.status === 'awaiting_human');
const phaseA = h('div', { class: 'phase' },
h('div', { class: 'phase-head' },
h('div', { class: 'row' }, span('tag soft-amber', 'Phase A'), h('strong', null, '快筛 Fast Triage'), statusInd(tri?.status || 'pending')),
tri?.binding?.agent ? agentTag(tri.binding.agent) : null),
triId
? (triArt?.state === 'ok' && triArt.data
? h('div', { class: 'stack-sm' },
...(triArt.data.hypotheses || []).map(hypothesisCompact),
triArt.data.mitigation ? h('div', { class: 'mitig-box' }, h('strong', null, '止血建议：'), triArt.data.mitigation) : null,
h('div', { class: 'row' }, btn('抽屉查看完整快筛', { cls: 'sm', onclick: () => openDrawer('artifact', { artifactId: triId, title: `快筛报告 · ${p.id}` }) })))
: skeleton(3))
: h('div', { class: 'muted' }, tri?.status === 'running' ? '正在快速收集线索与初步假设…' : '等待启动快筛'),
canDecide ? triageActionBar(p) : null);
const phaseB = h('div', { class: 'phase' },
h('div', { class: 'phase-head' },
h('div', { class: 'row' }, span('tag soft-plum', 'Phase B'), h('strong', null, '深挖 Deep Root-Cause'), statusInd(deep?.status || 'pending')),
deep?.binding?.agent ? agentTag(deep.binding.agent) : null),
deepId
? (deepArt?.state === 'ok' && deepArt.data
? h('div', { class: 'stack-sm' },
h('div', { style: 'font-weight:600;color:var(--ink)' }, deepArt.data.root_cause?.claim || '—'),
h('div', { class: 'row' },
deepArt.data.repro_before ? statusInd(deepArt.data.repro_before.outcome, `修复前复现：${deepArt.data.repro_before.outcome}`, true) : null,
deepArt.data.verification_after ? statusInd(deepArt.data.verification_after.outcome, `修复后验证：${deepArt.data.verification_after.outcome}`, true) : null),
deepArt.data.ruled_out?.length ? span('muted tabular', `已排除 ${deepArt.data.ruled_out.length} 条假设`) : null,
h('div', { class: 'row' }, btn('抽屉查看完整 RCA', { cls: 'sm', onclick: () => openDrawer('artifact', { artifactId: deepId, title: `根因分析 · ${p.id}` }) })))
: skeleton(3))
: h('div', { class: 'muted' }, deep?.status === 'running' ? '正在复现并验证根本原因…' : '等待 Phase A 决策后启动'));
return h('article', { class: `card${UI.arg === p.id ? ' focus-ring' : ''}`, id: `pl-${p.id}` },
h('div', { class: 'card-head' },
h('div', { class: 'grow' },
h('div', { class: 'card-title' }, p.goal || '未命名排查'),
h('div', { class: 'card-meta' }, h('code', null, p.id), statusInd(p.status), span('tag soft-amber', '两速查错'), span('muted', base(p.project?.root)), span('tabular', `更新于 ${ago(p.updated_ms)}`))),
btn('流程元数据', { cls: 'ghost sm', onclick: () => openDrawer('pipeline', { pipelineId: p.id }) })),
p.status === 'awaiting_binding_confirm' ? [bindingEditor(p), confirmBindingBar(p)] : null,
h('div', { class: 'phases' }, phaseA, phaseB));
}
const g2Of = (af) => af.g2?.verdict || null;
const g3Of = (af) => af.falsifier?.verdict || (af.disposition === 'pending_falsifier' ? 'pending_falsifier' : null);
const effSev = (af) => (g3Of(af) === 'downgraded' && af.falsifier?.to ? af.falsifier.to : af.finding.severity);
const isDropped = (af) => ['dropped', 'fabricated', 'returned_g1'].includes(af.disposition) || g3Of(af) === 'disproved' || g2Of(af) === 'fabricated';
const sevRank = (af) => SEVERITIES.indexOf(effSev(af));
const sevTagCls = (s) => (s === 'blocker' ? 'soft-red' : s === 'major' ? 'soft-amber' : s === 'minor' ? 'soft-blue' : '');
function dropReasons(af) {
const r = [];
if (g2Of(af) === 'fabricated' || af.disposition === 'fabricated') r.push('G2 证据不可验证或伪造');
if (g3Of(af) === 'disproved') r.push('G3 被 Falsifier 证伪');
if (af.disposition === 'returned_g1') r.push('G1 结构退回');
return r.length ? r.join('；') : '已过滤';
}
function viewReviews() {
const revs = [...S.reviews.values()].sort((a, b) => (b.report?.header?.created_ms || 0) - (a.report?.header?.created_ms || 0));
const dropped = revs.flatMap((r) => (r.findings || []).filter(isDropped).map((af) => [r, af]));
const tabs = [
['reports', `评审报告（${revs.length}）`],
['dropped', `Dropped Findings · 可审计（${dropped.length}）`],
['precision', 'Precision 准确率遥测'],
];
let body;
if (S.reviewsState === 'loading' && !revs.length) body = h('div', { class: 'card' }, skeleton(5));
else if (S.reviewsState === 'err' && !revs.length) body = errorBanner(`评审列表加载失败：${S.reviewsErr}`, '重试加载', loadReviews);
else if (UI.revTab === 'reports') {
body = revs.length ? h('div', { class: 'stack' }, revs.map(reviewCard)) : h('div', { class: 'card' }, emptyState('暂无代码评审报告', '当 Reviewer 提交 review.v1 报告后，会经过 G1 / G2 / G3 门禁并显示在这里。'));
} else if (UI.revTab === 'dropped') {
body = droppedList(dropped);
} else {
body = precisionTable();
}
return h('div', { class: 'stack' },
h('div', { class: 'page-head' },
h('div', null, h('h1', null, '代码评审'), h('p', null, 'review.v1 反误报审计 —— G1 结构校验 · G2 证据核实 · G3 Falsifier 证伪，被过滤条目全量可审计')),
btn('刷新评审数据', { cls: 'sm', onclick: () => { loadReviews(); if (UI.revTab === 'precision') loadPrecision(); } })),
h('div', { class: 'subtabs', role: 'tablist' }, tabs.map(([k, l]) => h('button', {
type: 'button', role: 'tab', class: `subtab${UI.revTab === k ? ' on' : ''}`,
onclick: () => { UI.revTab = k; if (k === 'precision') loadPrecision(); schedule(); },
}, l))),
body);
}
function reviewCard(rev) {
const rp = rev.report || {}; const fs = rev.findings || [];
const kept = fs.filter((f) => !isDropped(f)).sort((a, b) => sevRank(a) - sevRank(b) || b.finding.confidence - a.finding.confidence);
const major = kept.filter((f) => sevRank(f) <= 1);
const minor = kept.filter((f) => sevRank(f) > 1);
const droppedCnt = fs.filter(isDropped).length;
const cnt = (fn) => fs.filter(fn).length;
const au = rp.header?.author;
return h('article', { class: `card${UI.arg === rev.id ? ' focus-ring' : ''}` },
h('div', { class: 'card-head' },
h('div', { class: 'grow' },
h('div', { class: 'row' },
span('tag soft-plum mono', rp.depth || 'D1'),
h('code', null, rev.id),
au ? [agentTag(au.agent), modelTag(au.model)] : null,
rp.header?.pipeline_id ? h('a', { class: 'mono', href: `#/pipelines/${enc(rp.header.pipeline_id)}` }, rp.header.pipeline_id) : null,
span('mono muted', `${short(rp.base, 7)} → ${short(rp.head, 7)}`)),
rp.summary ? h('div', { style: 'margin-top:8px' }, renderMarkdown(rp.summary)) : null),
h('div', { class: 'row' },
statusInd(rev.complete ? 'done' : 'pending_falsifier', rev.complete ? '已完成' : '等待 Falsifier', true),
span('tag tabular', `G2 验证 ${cnt((f) => g2Of(f) === 'verified')} / 伪造 ${cnt((f) => g2Of(f) === 'fabricated')}`))),
h('div', { class: 'stack-sm' },
major.length ? major.map((af) => findingRow(rev, af))
: (fs.length ? h('div', { class: 'muted' }, '无 blocker / major 级发现') : h('div', { class: 'callout ok' }, '未发现问题 —— Reviewer 已记录检查范围（checked_areas）。')),
minor.length ? h('details', { 'data-k': `rv:min:${rev.id}`, open: UI.open.has(`rv:min:${rev.id}`) },
h('summary', null, `折叠显示 ${minor.length} 条 minor / nit（不推送飞书）`),
h('div', { class: 'stack-sm', style: 'margin-top:8px' }, minor.map((af) => findingRow(rev, af)))) : null,
droppedCnt ? h('div', { class: 'row muted', style: 'font-size:12.5px;margin-top:4px' },
`另有 ${droppedCnt} 条被门禁过滤的发现`,
btn('在 Dropped Findings 中审计 →', { cls: 'ghost xs', onclick: () => { UI.revTab = 'dropped'; schedule(); } })) : null));
}
function findingRow(rev, af, { dropped = false } = {}) {
const f = af.finding; const sev = effSev(af);
const g2 = g2Of(af); const g3 = g3Of(af);
const fb = af.feedback?.label;
const postFb = (lbl, labelVal) => act(`fb:${rev.id}:${f.id}`, async () => {
await api('POST', `/api/reviews/${enc(rev.id)}/findings/${enc(f.id)}/feedback`, { label: labelVal });
await refreshReview(rev.id);
}, `已标记为 ${lbl}`);
return h('div', { class: `finding-row${dropped ? ' dropped' : ''}` },
span(`tag ${sevTagCls(sev)}`, sev),
span('tag', f.category),
g2 ? statusInd(g2, `G2 ${g2}`) : null,
g3 ? statusInd(g3, `G3 ${g3 === 'pending_falsifier' ? '待证伪' : g3}`) : null,
h('div', { class: 'finding-claim' }, f.claim, dropped ? h('div', { class: 'muted', style: 'font-size:12px;margin-top:2px' }, `过滤原因：${dropReasons(af)}`) : null),
h('code', null, `${f.location?.file}:${f.location?.line_start}`),
span('muted tabular', pct(f.confidence)),
h('div', { class: 'row' },
btn('有效', { cls: `xs${fb === 'valid' ? ' on' : ''}`, onclick: () => postFb('有效', 'valid') }),
btn('误报', { cls: `xs${fb === 'false_positive' ? ' on' : ''}`, onclick: () => postFb('误报', 'false_positive') }),
btn('暂不修', { cls: `xs ghost${fb === 'wontfix' ? ' on' : ''}`, onclick: () => postFb('暂不修复', 'wontfix') }),
btn('抽屉详情', { cls: 'xs', onclick: () => openDrawer('finding', { reviewId: rev.id, findingId: f.id, dropped }) })));
}
function droppedList(dropped) {
return h('div', { class: 'stack' },
h('div', { class: 'callout' }, '所有被 G1 / G2 / G3 门禁过滤的发现（含 disproved 与 fabricated）均在此保留可审计，绝不静默丢弃。如认为存在误杀，点开「抽屉详情」即可查看反证并申诉恢复。'),
dropped.length
? h('div', { class: 'card stack-sm' }, dropped.map(([r, af]) => h('div', { class: 'stack-xs' },
h('div', { class: 'row muted', style: 'font-size:12px' }, h('code', null, r.id), span('tag soft-plum mono', r.report?.depth || 'D1'), r.report?.header?.author ? agentTag(r.report.header.author.agent) : null),
findingRow(r, af, { dropped: true }))))
: h('div', { class: 'card' }, emptyState('没有被过滤的发现', '目前所有评审条目均已正常交付或折叠。')));
}
function precisionTable() {
const rows = (S.precision || []).slice().sort((a, b) => (a.agent + (a.model || '')).localeCompare(b.agent + (b.model || '')) || a.depth.localeCompare(b.depth));
const sum = (k) => rows.reduce((n, r) => n + (r[k] || 0), 0);
const v = sum('valid'); const fp = sum('false_positive');
const filterBar = h('div', { class: 'row', style: 'justify-content:space-between' },
h('div', { class: 'chips' }, ['', ...AGENTS].map((a) => h('button', {
type: 'button', class: `chip${UI.precAgent === a ? ' on' : ''}`,
onclick: () => { UI.precAgent = a; loadPrecision(); },
}, a || '全部 Agent'))),
span('muted', 'Precision = valid / (valid + false_positive)'));
if (S.precisionState === 'loading' && !S.precision) return h('div', { class: 'card' }, skeleton(4));
if (S.precisionState === 'err') return errorBanner(`准确率遥测加载失败：${S.precisionErr}`, '重试', loadPrecision);
if (!rows.length) return h('div', { class: 'stack' }, filterBar, h('div', { class: 'card' }, emptyState('暂无反馈遥测数据', '对评审条目点击「有效 / 误报 / 暂不修」后，将按模型与深度统计 Precision。')));
return h('div', { class: 'stack' }, filterBar,
h('div', { class: 'card table-wrap' }, h('table', { class: 'tbl' },
h('thead', null, h('tr', null, ['Agent', 'Model', 'Depth', 'Category', '交付', '有效', '误报', '暂不修', '伪造', '证伪', 'Precision'].map((x, i) => h('th', { class: i > 3 ? 'num' : '' }, x)))),
h('tbody', null, rows.map((r) => h('tr', null,
h('td', null, agentTag(r.agent)), h('td', { class: 'mono' }, r.model || '—'),
h('td', null, span('tag soft-plum mono', r.depth)), h('td', null, r.category),
...['delivered', 'valid', 'false_positive', 'wontfix', 'fabricated', 'disproved'].map((k) => h('td', { class: 'num' }, r[k] ?? 0)),
h('td', { class: 'num' }, r.precision == null ? span('muted', '—') : h('span', { class: 'row', style: 'justify-content:flex-end' }, bar(r.precision, r.precision < 0.5 ? 'low' : r.precision < 0.8 ? 'mid' : 'hi'), ` ${pct(r.precision)}`))))),
h('tfoot', null, h('tr', null,
h('td', { colspan: 4 }, '合计'),
...['delivered', 'valid', 'false_positive', 'wontfix', 'fabricated', 'disproved'].map((k) => h('td', { class: 'num' }, sum(k))),
h('td', { class: 'num' }, v + fp ? pct(v / (v + fp)) : '—'))))));
}
function openDrawer(kind, params) {
UI.drawer = { kind, ...params };
if (kind === 'session' && params.sessionId) loadHandoffs(params.sessionId);
if (kind === 'terminal' && params.pane) loadPreview(params.pane, { lines: 200, silent: false });
schedule();
}
function closeDrawer() { UI.drawer = null; schedule(); }
function renderDrawer() {
const root = $('#drawer-root');
if (!UI.drawer) {
root.className = 'drawer-root';
root.setAttribute('aria-hidden', 'true');
root.replaceChildren();
return;
}
root.className = 'drawer-root open';
root.setAttribute('aria-hidden', 'false');
const d = UI.drawer;
let title = '详情检查器'; let sub = ''; let body = null;
if (d.kind === 'session') {
const s = S.sessions.get(d.sessionId);
title = s ? `会话详情 · ${sessionLabel(s)}` : '会话详情';
sub = s ? s.session_id : '';
body = s ? drawerSession(s) : emptyState('会话不存在', '该会话可能已被清理。');
} else if (d.kind === 'terminal') {
title = `完整终端输出 · ${d.title || d.pane}`;
sub = `${d.pane}（最近 200 行）`;
const pv = S.previews.get(`${d.pane}:200`);
body = !pv || pv.state === 'loading' ? skeleton(8)
: pv.state === 'err' ? errorBanner(`读取终端失败：${pv.err}`, '重试', () => loadPreview(d.pane, { lines: 200 }))
: h('div', { class: 'stack-sm' },
h('div', { class: 'row', style: 'justify-content:space-between' },
span('muted tabular', `共 ${pv.data?.lines || 200} 行 · 更新于 ${clock(pv.updatedMs)}`),
btn('刷新 200 行', { cls: 'sm', onclick: () => loadPreview(d.pane, { lines: 200 }) })),
h('pre', { class: 'code-block', style: 'max-height:none' }, pv.text || '（无输出）'));
} else if (d.kind === 'artifact') {
title = d.title || `工件 · ${d.artifactId}`;
sub = d.artifactId;
body = drawerArtifact(d.artifactId);
} else if (d.kind === 'finding') {
const rev = S.reviews.get(d.reviewId);
const af = rev?.findings?.find((x) => x.finding.id === d.findingId);
title = af ? `评审条目 · ${af.finding.id}` : '评审详情';
sub = `${d.reviewId}`;
body = rev && af ? drawerFinding(rev, af, d.dropped) : skeleton(4);
} else if (d.kind === 'pipeline') {
const p = S.pipelines.get(d.pipelineId);
title = p ? `流程元数据 · ${p.id}` : '流程元数据';
sub = p ? p.goal : '';
body = p ? drawerPipeline(p) : emptyState('未找到流程');
} else if (d.kind === 'paste') {
const list = UI.attachments.get(d.sessionId) || [];
const att = list.find((x) => x.id === d.attachmentId);
title = att ? att.title : '附件预览';
sub = att ? (att.kind === 'image' ? `${att.mime || 'image'} · ${fmtBytes(att.bytes)}` : `${att.lines} 行 · ${fmtBytes(att.bytes)}`) : '';
body = att ? drawerPaste(d.sessionId, att) : emptyState('附件已移除', '该附件已被发送或删除。');
}
root.replaceChildren(
h('div', { class: 'drawer-backdrop', onclick: closeDrawer }),
h('aside', { class: 'drawer-panel', role: 'dialog', 'aria-label': title },
h('div', { class: 'drawer-head' },
h('div', { class: 'grow' }, h('h3', null, title), sub ? h('div', { class: 'muted mono', style: 'font-size:12px;margin-top:2px' }, sub) : null),
btn('关闭', { cls: 'ghost sm', onclick: closeDrawer })),
h('div', { class: 'drawer-body' }, body)));
}
function drawerMetricsCard(m) {
const pctVal = ctxPctOf(m);
const tpsVal = tpsOf(m);
const hasUsed = isFin(m?.context_used_tokens);
const hasWin = isFin(m?.context_window_tokens) && m.context_window_tokens > 0;
let ctxVal = '—';
if (hasUsed && hasWin && pctVal != null) ctxVal = `${fmtTokens(m.context_used_tokens)} / ${fmtTokens(m.context_window_tokens)}（${Math.round(pctVal)}%）`;
else if (pctVal != null) ctxVal = `${Math.round(pctVal)}%`;
else if (hasUsed) ctxVal = fmtTokens(m.context_used_tokens);
const lastTurn = isFin(m?.last_output_tokens) || isFin(m?.last_duration_ms)
? `${isFin(m?.last_output_tokens) ? `${fmtTokens(m.last_output_tokens)} tokens` : '—'} · ${isFin(m?.last_duration_ms) ? fmtMs(m.last_duration_ms) : '—'}`
: '—';
const totals = isFin(m?.total_input_tokens) || isFin(m?.total_output_tokens)
? `${fmtTokens(m?.total_input_tokens)} 输入 / ${fmtTokens(m?.total_output_tokens)} 输出`
: '—';
const srcLine = m?.source && m.source !== 'none'
? `${metricsSrcLabel(m.source)}${m.updated_ms ? ` · ${clock(m.updated_ms)}` : ''}`
: '暂无遥测数据';
return h('div', { class: 'metrics-card' },
h('div', { class: 'sec-h', style: 'margin:0' }, '性能与上下文'),
kvGrid([
['上下文占用', h('span', { class: 'row tabular' }, ctxVal, pctVal != null ? ctxBar(pctVal, false) : null)],
['生成速度 (TPS)', span('tabular', tpsVal != null ? `${tpsVal.toFixed(1)} tok/s` : '—')],
['首字延迟 (TTFT)', span('tabular', isFin(m?.ttft_ms) ? fmtMs(m.ttft_ms) : '—')],
['最近回合', span('tabular', lastTurn)],
['累计用量', span('tabular', totals)],
['数据来源', span('tabular muted', srcLine)],
]));
}
function drawerSession(s) {
const subs = subagentsOf(s.session_id);
const ho = S.handoffs.get(s.session_id);
const evs = S.activity.filter((e) => e.session_id === s.session_id || (s.pane && e.pane === s.pane)).slice(0, 15);
return h('div', { class: 'stack' },
h('div', { class: 'drawer-controls' },
s.pane ? focusBtn(s.session_id, 'sm') : null,
(s.state === 'error' || s.state === 'needs_permission' || s.state === 'needs_input' || s.last_error)
? btn('清除异常状态', { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => ackSessionError(s) }) : null,
btn('手动刷新', { cls: 'sm', onclick: () => { loadSessionOutput(s, { silent: false }); toast('已刷新会话输出'); } }),
s.pane ? btn('完整 200 行终端', { cls: 'ghost sm', onclick: () => openDrawer('terminal', { pane: s.pane, title: sessionLabel(s) }) }) : null,
h('label', { class: 'row muted', style: 'cursor:pointer;font-size:12.5px;margin-left:auto' },
h('input', { type: 'checkbox', checked: UI.autoRefreshPreview, onchange: (e) => { UI.autoRefreshPreview = e.target.checked; rerender(); } }), '3s 自动刷新')),
drawerMetricsCard(s.metrics),
kvGrid([
['会话 ID', h('code', null, s.session_id)],
['窗口标识', s.tmux_label ? h('code', null, s.tmux_label) : null],
['Agent / 模型', h('div', { class: 'row' }, agentTag(s.agent), modelTag(modelBaseOf(s.model, s.effort) || s.model))],
['思考强度', s.effort ? span('tag mono', s.effort) : span('muted', '—')],
['状态', statusInd(s.state, `${stateText(s.state)}（持续 ${dur(Date.now() - (s.state_since_ms || Date.now()))}）`)],
s.last_error ? ['最近错误', span('mono', s.last_error)] : null,
['来源', s.discovered_only ? span('tag legacy', '存量 tmux 扫描发现') : span('tag soft-green', 'Hook 实时直连')],
['tmux 位置', s.tmux_target ? h('code', null, `${s.tmux_target} (${s.pane || '—'})`) : h('code', null, s.pane || '—')],
['工作目录', h('code', null, s.cwd || '—')],
['Git 根目录', h('code', null, s.git_root || '—')],
['最近活动', ago(s.last_event_ms)],
].filter(Boolean)),
h('div', { class: 'stack-xs' },
h('div', { class: 'sec-h' }, `子会话 / Swarm（${subs.length || s.subagent_count || 0}）`),
subs.length ? subs.map((c) => h('button', {
type: 'button', class: 'callout row sub-drawer-btn', style: 'justify-content:space-between',
onclick: () => { closeDrawer(); openSession(c.session_id); },
},
h('div', { class: 'row' }, statusInd(c.state), agentTag(c.agent), h('code', null, subagentDisplayName(c))),
span('muted trunc', c.last_message || '点击查看对话 →')))
: h('div', { class: 'muted', style: 'font-size:13px' }, s.subagent_count ? `累计 ${s.subagent_count} 个子会话（当前活跃 ${s.active_subagents || 0}）` : '暂无嵌套子会话')),
h('div', { class: 'stack-xs' },
h('div', { class: 'sec-h' }, 'Handoff 交接记录'),
!ho || ho.state === 'loading' ? skeleton(2)
: ho.state === 'err' ? errorBanner(`加载交接失败：${ho.err}`, '重试', () => loadHandoffs(s.session_id))
: ho.items.length ? ho.items.map((x) => h('div', { class: 'callout stack-xs' },
h('div', { class: 'row' }, span('tag', x.intent), span('tag', x.status), h('code', null, x.id), span('muted tabular', clock(x.created_ms))),
h('div', { style: 'font-weight:500' }, x.summary)))
: h('div', { class: 'muted', style: 'font-size:13px' }, '暂无 Handoff 交接记录')),
evs.length ? h('div', { class: 'stack-xs' },
h('div', { class: 'sec-h' }, `近期实时事件（${evs.length}）`),
evs.map((e) => h('div', { class: 'row', style: 'font-size:12.5px' }, span('mono dim', clock(e.ts_ms)), span('tag', e.kind), span('muted trunc grow', e.detail?.error || e.detail?.tool_summary || e.detail?.last_message || e.native_event || '')))) : null,
h('details', null, h('summary', null, '查看原始 Session JSON'), h('pre', { class: 'code-block' }, JSON.stringify(s, null, 2))));
}
function drawerArtifact(id) {
const a = artifact(id);
if (a.state === 'loading') return skeleton(6);
if (a.state === 'err') return errorBanner(`工件加载失败：${a.err}`, '重试', () => { S.artifacts.delete(id); schedule(); });
const d = a.data || {}; const hd = d.header || {};
let content;
if (d.kind === 'plan') {
content = h('div', { class: 'stack' },
kvGrid([['目标', d.goal], ['原生策略', d.native_strategy ? span('tag soft-plum', d.native_strategy) : null]]),
h('div', null, h('div', { class: 'sec-h' }, '设计方案'), mdProse(d.design_md)),
h('div', null, h('div', { class: 'sec-h' }, `任务列表（${(d.tasks || []).length}）`), plainList((d.tasks || []).map((t) => `${t.title}${t.files?.length ? ` (${t.files.join(', ')})` : ''}`))),
h('div', null, h('div', { class: 'sec-h' }, '验收标准'), plainList(d.acceptance)),
h('div', null, h('div', { class: 'sec-h' }, '测试计划'), plainList(d.test_plan)),
d.risks?.length ? h('div', null, h('div', { class: 'sec-h' }, '风险点'), plainList(d.risks)) : null);
} else if (d.kind === 'triage') {
content = h('div', { class: 'stack' },
kvGrid([['故障现象', d.symptom], ['快筛耗时', d.elapsed_secs != null ? dur(d.elapsed_secs * 1000) : null]]),
...(d.hypotheses || []).map((hy, i) => h('div', { class: 'hyp-card' },
h('div', { class: 'row', style: 'justify-content:space-between' }, span('tag soft-amber', `假设 #${i + 1} · 未证实`), span('tabular muted', pct(hy.confidence))),
h('div', { style: 'font-weight:600' }, hy.claim),
evBlocks(hy.evidence_refs),
kvGrid([['确认方法', hy.how_to_confirm]]))),
d.mitigation ? h('div', { class: 'mitig-box' }, h('div', { class: 'sec-h' }, '临时止血方案'), renderMarkdown(d.mitigation)) : null,
d.probes?.length ? h('div', null, h('div', { class: 'sec-h' }, '建议检查命令'), plainList(d.probes)) : null);
} else if (d.kind === 'rca') {
const rc = d.root_cause || {};
const testCard = (lbl, t) => (t ? h('div', { class: 'callout stack-xs' },
h('div', { class: 'row', style: 'justify-content:space-between' }, h('strong', null, lbl), statusInd(t.outcome)),
h('code', null, `$ ${t.cmd}`),
t.output_tail ? h('pre', { class: 'code-block' }, t.output_tail) : null) : null);
content = h('div', { class: 'stack' },
h('div', null, h('div', { class: 'sec-h' }, '已验证根因'), renderMarkdown(rc.claim || '—')),
rc.trigger_path?.length ? h('div', null, h('div', { class: 'sec-h' }, '触发路径'), codeRefList(rc.trigger_path)) : null,
rc.evidence?.length ? h('div', null, h('div', { class: 'sec-h' }, '证据链'), evBlocks(rc.evidence)) : null,
testCard('修复前复现', d.repro_before),
d.fix ? kvGrid([['修复分支', h('code', null, d.fix.branch)], ['Commit', d.fix.commit ? h('code', null, short(d.fix.commit, 10)) : null], ['修改文件', d.fix.files?.join(', ')]]) : null,
testCard('修复后验证', d.verification_after),
d.ruled_out?.length ? h('div', null, h('div', { class: 'sec-h' }, '已排除假设'), d.ruled_out.map((r) => h('div', { class: 'callout' }, h('s', null, r.hypothesis), evBlocks(r.evidence)))) : null);
} else if (d.kind === 'report') {
content = h('div', { class: 'stack' }, mdProse(d.summary_md),
d.changes?.length ? h('div', null, h('div', { class: 'sec-h' }, '变更摘要'), plainList(d.changes)) : null,
d.decisions?.length ? h('div', null, h('div', { class: 'sec-h' }, '关键决策'), plainList(d.decisions)) : null,
d.follow_ups?.length ? h('div', null, h('div', { class: 'sec-h' }, '后续跟进'), plainList(d.follow_ups)) : null);
} else {
content = h('pre', { class: 'code-block' }, JSON.stringify(d, null, 2));
}
return h('div', { class: 'stack' },
h('div', { class: 'row' }, span('tag', d.kind || 'artifact'), hd.author ? [agentTag(hd.author.agent), modelTag(hd.author.model)] : null, hd.commit ? span('mono muted', `@${short(hd.commit, 8)}`) : null),
content,
h('details', null, h('summary', null, '查看原始 Artifact JSON'), h('pre', { class: 'code-block' }, JSON.stringify(d, null, 2))));
}
function drawerFinding(rev, af, dropped) {
const f = af.finding; const g2 = g2Of(af); const g3 = g3Of(af);
const nk = `dfn:${rev.id}:${f.id}`;
const note = () => draft(nk).trim() || undefined;
const post = (verb, payload, okMsg) => act(`df:${verb}:${f.id}`, async () => {
await api('POST', `/api/reviews/${enc(rev.id)}/findings/${enc(f.id)}/${verb}`, payload);
await refreshReview(rev.id);
}, okMsg);
const dgKey = `dfdg:${rev.id}:${f.id}`;
const dgSel = selectField(dgKey, SEVERITIES.slice(1).map((x) => [x, x]), 'minor', { class: 'inp sm' });
return h('div', { class: 'stack' },
dropped ? h('div', { class: 'callout warn' }, `该条目已被过滤（${dropReasons(af)}），保留在此供审计与申诉。`) : null,
h('div', { class: 'prose', style: 'font-weight:600;font-size:15px' }, f.claim),
kvGrid([
['严重级别 / 类别', h('div', { class: 'row' }, span(`tag ${sevTagCls(effSev(af))}`, effSev(af)), span('tag', f.category), span('tabular muted', `置信度 ${pct(f.confidence)}`))],
['代码位置', h('code', null, `${f.location?.file}:${f.location?.line_start}-${f.location?.line_end}${f.location?.symbol ? ` · ${f.location.symbol}` : ''}`)],
['验证方式', span('tag', f.verification_method)],
['前置条件', f.preconditions],
['可观察影响', f.observable_impact || span('muted', '未提供')],
['触发路径', codeRefList(f.trigger_path) || span('muted', '未提供')],
['已检查 Guard', codeRefList(f.checked_guards) || span('muted', '未提供')],
['修复建议', f.suggested_fix],
]),
h('div', null, h('div', { class: 'sec-h' }, 'G2 证据核实'), g2 ? statusInd(g2, `G2 ${g2}`, true) : null, evBlocks(f.evidence)),
af.falsifier ? h('div', { class: 'callout stack-xs' },
h('div', { class: 'row' }, h('strong', null, 'G3 Falsifier 裁决'), statusInd(g3 || 'pending_falsifier', g3)),
renderMarkdown(af.falsifier.rationale),
af.falsifier.counter_evidence?.length ? h('div', null, h('div', { class: 'sec-h' }, '反证代码'), evBlocks(af.falsifier.counter_evidence)) : null) : null,
h('div', { class: 'stack-sm', style: 'border-top:1px solid var(--border);padding-top:14px' },
field(nk, { class: 'inp sm', placeholder: '添加反馈或裁决备注（可选）…' }),
h('div', { class: 'row' },
btn('标记有效', { cls: 'sm', onclick: () => post('feedback', { label: 'valid', note: note() }, '已标记有效') }),
btn('标记误报', { cls: 'sm', onclick: () => post('feedback', { label: 'false_positive', note: note() }, '已标记误报') }),
btn('暂不修复', { cls: 'ghost sm', onclick: () => post('feedback', { label: 'wontfix', note: note() }, '已标记暂不修复') })),
(g3 === 'needs_human' || dropped) ? h('div', { class: 'row' },
btn(dropped ? '申诉恢复为 confirmed' : '人工确认 confirmed', { cls: 'primary sm', onclick: () => post('verdict', { verdict: 'confirmed', note: note() }, '已确认 confirmed') }),
!dropped ? [dgSel, btn('降级', { cls: 'sm', onclick: () => post('verdict', { verdict: 'downgraded', to: draft(dgKey, 'minor'), note: note() }, '已降级') }),
btn('证伪 disproved', { cls: 'danger sm', onclick: () => post('verdict', { verdict: 'disproved', note: note() }, '已标记 disproved') })] : null) : null));
}
function drawerPipeline(p) {
return h('div', { class: 'stack' },
kvGrid([
['Pipeline ID', h('code', null, p.id)],
['类型', p.playbook === 'debug' ? '两速查错' : '功能协作'],
['状态', statusInd(p.status)],
['项目路径', h('code', null, p.project?.root || '—')],
['Worktree', typeof p.project?.worktree === 'string' ? p.project.worktree : 'auto'],
['发起渠道', p.source?.via || '—'],
['原始话术', p.source?.utterance || '—'],
['创建时间', p.created_ms ? new Date(p.created_ms).toLocaleString('zh-CN', { hour12: false }) : '—'],
]),
h('details', null, h('summary', null, '查看原始 Pipeline JSON'), h('pre', { class: 'code-block' }, JSON.stringify(p, null, 2))));
}
function drawerPaste(sid, att) {
if (att.kind === 'image') {
return h('div', { class: 'stack-sm' },
h('div', { class: 'drawer-controls' },
att.path ? btn('复制图片路径', { cls: 'sm', onclick: (e) => copyText(att.path, e.currentTarget) }) : null,
btn('移除图片', { cls: 'ghost sm danger', onclick: () => { removeAttachment(sid, att.id); closeDrawer(); } })),
att.previewUrl ? h('div', { class: 'drawer-img-wrap' }, h('img', { class: 'drawer-img', src: att.previewUrl, alt: att.title })) : skeleton(4),
att.path ? kvGrid([['服务端路径', h('code', null, att.path)], ['大小', fmtBytes(att.bytes)]]) : null);
}
return h('div', { class: 'stack-sm' },
h('div', { class: 'drawer-controls' },
btn('复制全文', { cls: 'sm', onclick: (e) => copyText(att.content, e.currentTarget) }),
btn('转入输入框', { cls: 'sm', onclick: () => { expandAttachmentToInput(sid, att.id); closeDrawer(); } }),
btn('移除附件', { cls: 'ghost sm danger', onclick: () => { removeAttachment(sid, att.id); closeDrawer(); } })),
h('pre', { class: 'code-block', style: 'max-height:none' }, att.content));
}
function renderModal() {
const d = $('#modal');
const want = S.authNeeded ? 'auth' : UI.modal;
if (!want) { if (d.open) d.close(); return; }
if (!d.open) d.showModal();
if (want === 'auth') d.replaceChildren(authModalBody());
else if (want === 'spawn') d.replaceChildren(spawnModalBody());
else d.replaceChildren(pipelineModalBody(want));
}
function closeModal() { UI.modal = null; schedule(); }
$('#modal').addEventListener('close', () => { if (!S.authNeeded) UI.modal = null; });
$('#modal').addEventListener('cancel', (e) => { if (S.authNeeded) e.preventDefault(); });
$('#modal').addEventListener('click', (e) => { if (e.target.id === 'modal' && !S.authNeeded) closeModal(); });
function spawnModalBody() {
const projects = knownProjects();
const prefs = loadSpawnPrefs();
const agent = draft('sp:agent', prefs.last_agent || 'claude');
const curDir = '';
const mInfo = modelsForAgent(agent);
const selKey = `sp:modelSel:${agent}`;
const effKey = `sp:effort:${agent}`;
if (!UI.drafts.has(selKey)) hydrateSpawnAgent(agent, prefs);
const curSel = UI.drafts.get(selKey) || mInfo.default;
const effs = effortsForModel(mInfo, curSel);
const fixedEffort = effs.length === 0;
let curEff = UI.drafts.get(effKey) || mInfo.default_effort;
if (!fixedEffort && !effs.includes(curEff)) {
curEff = effs.includes(mInfo.default_effort) ? mInfo.default_effort : effs[0];
UI.drafts.set(effKey, curEff);
}
const modelOpts = [...mInfo.models.map((m) => [m.id, m.label]), ['__custom__', '自定义模型…']];
const effortOpts = fixedEffort ? [['', '固定配置']] : effs.map((e) => [e, EFFORT_LABEL[e] || e]);
const dlId = 'sp-projects';
const persistCurrent = (ag, mSel, eSel) => {
const modelVal = mSel === '__custom__' ? draft('sp:modelCustom').trim() : String(mSel || '').trim();
saveSpawnPrefs(ag, { model: modelVal || undefined, effort: eSel || '' });
};
const create = () => act('spawn-sess', async () => {
const ag = draft('sp:agent', 'claude');
const info = modelsForAgent(ag);
const picked = UI.drafts.get(`sp:modelSel:${ag}`) || info.default;
const model = picked === '__custom__' ? draft('sp:modelCustom').trim() : String(picked || '').trim();
const availEffs = effortsForModel(info, picked);
const pickedEff = availEffs.length ? (UI.drafts.get(`sp:effort:${ag}`) || info.default_effort || availEffs[0]) : '';
const proj = draft('sp:dir', curDir).trim();
const payload = {
agent: ag,
project_root: proj || undefined,
model: model || undefined,
effort: pickedEff || undefined,
name: draft('sp:name').trim() || undefined,
prompt: draft('sp:prompt').trim() || undefined,
};
const r = await api('POST', '/api/sessions/spawn', payload);
saveSpawnPrefs(ag, { model: model || undefined, effort: pickedEff || '' });
['sp:dir', 'sp:name', 'sp:prompt'].forEach((k) => UI.drafts.delete(k));
UI.modal = null;
await loadAll();
if (r?.session_id) openSession(r.session_id);
return r;
}, (r) => `已在 tmux 新建会话 ${r?.tmux_target || r?.pane_id || ''}`);
const modelSelect = selectField(selKey, modelOpts, curSel, { class: 'inp', style: 'width:100%' });
modelSelect.addEventListener('change', () => {
const nextModel = modelSelect.value;
const nextEffs = effortsForModel(mInfo, nextModel);
let nextEff = UI.drafts.get(effKey) || mInfo.default_effort;
if (!nextEffs.length) nextEff = '';
else if (!nextEffs.includes(nextEff)) nextEff = nextEffs.includes(mInfo.default_effort) ? mInfo.default_effort : nextEffs[0];
UI.drafts.set(effKey, nextEff);
persistCurrent(agent, nextModel, nextEff);
rerender();
});
const effortSelect = selectField(effKey, effortOpts, fixedEffort ? '' : curEff, {
class: 'inp', style: 'width:100%', disabled: fixedEffort,
});
effortSelect.addEventListener('change', () => {
persistCurrent(agent, curSel, effortSelect.value);
});
const customModelInp = curSel === '__custom__' ? field('sp:modelCustom', { class: 'inp sm mono', placeholder: '输入自定义模型 ID…', autocomplete: 'off' }) : null;
if (customModelInp) customModelInp.addEventListener('input', () => persistCurrent(agent, '__custom__', fixedEffort ? '' : curEff));
return h('div', { class: 'stack' },
h('div', { class: 'modal-head' }, h('h3', null, '新建交互会话'), btn('关闭', { cls: 'ghost sm', onclick: closeModal })),
h('datalist', { id: dlId }, projects.map((p) => h('option', { value: p }))),
h('div', null,
h('label', { class: 'lbl' }, '选择 Agent'),
h('div', { class: 'chips' }, AGENTS.map((a) => h('button', {
type: 'button', class: `chip${agent === a ? ' on' : ''}`,
onclick: () => {
UI.drafts.set('sp:agent', a);
hydrateSpawnAgent(a);
const nextModel = UI.drafts.get(`sp:modelSel:${a}`);
const nextEff = UI.drafts.get(`sp:effort:${a}`);
persistCurrent(a, nextModel, nextEff);
rerender();
},
}, a)))),
h('div', { class: 'opt-grid' },
h('div', { class: 'stack-xs' },
h('label', { class: 'lbl', style: 'margin:0' }, '模型'),
modelSelect,
customModelInp),
h('div', { class: 'stack-xs' },
h('label', { class: 'lbl', style: 'margin:0' }, '思考强度'),
effortSelect)),
h('div', { class: 'opt-grid' },
h('div', null, h('label', { class: 'lbl' }, '工作目录（默认 ~/.agent_session）'),
field('sp:dir', { class: 'inp mono', list: dlId, placeholder: '留空默认 ~/.agent_session', autocomplete: 'off' }, curDir)),
h('div', null, h('label', { class: 'lbl' }, 'tmux 窗口名（选填）'),
field('sp:name', { class: 'inp', placeholder: '默认自动命名', autocomplete: 'off' }))),
h('div', null, h('label', { class: 'lbl' }, '首条消息（选填）'),
field('sp:prompt', { tag: 'textarea', rows: 2, class: 'inp', placeholder: '留空则仅打开干净的交互式会话…' })),
h('div', { class: 'row', style: 'justify-content:flex-end' },
btn('取消', { cls: 'ghost', onclick: closeModal }),
btn('新建交互会话', { cls: 'primary', busyKey: 'spawn-sess', onclick: create })));
}
function pipelineModalBody(pb) {
const isDebug = pb === 'debug';
const scope = `modal:${pb}`;
const goalKey = `${scope}:goal`; const bindKey = `${scope}:bind`; const projKey = `${scope}:proj`;
const curDir = currentSession()?.git_root || currentSession()?.cwd || knownProjects()[0] || '';
const pv = UI.parsePreviews[scope];
const bindInp = field(bindKey, { class: 'inp', placeholder: isDebug ? '选填：如 快速排查用 agy，深挖根因用 codex' : '选填：如 计划用 claude，实现用 codex，Review 用 agy' });
bindInp.addEventListener('input', () => queueParse(scope, pb, projKey, bindKey, curDir));
const submit = () => act(`mcreate:${pb}`, async () => {
const goal = draft(goalKey).trim(); const utt = draft(bindKey).trim(); const proj = draft(projKey, curDir).trim();
const combined = [goal, utt].filter(Boolean).join('；');
if (!combined) throw new Error(isDebug ? '请描述故障现象' : '请描述需求目标');
const parsed = pv?.data && pv.text === utt ? pv.data : null;
let p = isDebug
? await api('POST', '/api/debug', { project_root: proj, symptom: goal || combined, utterance: utt || combined, auto_confirm: UI.autoConfirm })
: await api('POST', '/api/pipelines', { req: { playbook: 'feature', project_root: proj, goal: goal || combined, bindings: parsed?.bindings || {}, source: { via: 'web', utterance: combined } }, auto_confirm: UI.autoConfirm });
p = upsertPipeline(p.pipeline || p);
if (UI.autoConfirm && p.status === 'awaiting_binding_confirm') p = upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, {}));
UI.drafts.delete(goalKey); UI.drafts.delete(bindKey); delete UI.parsePreviews[scope];
UI.modal = null;
go('pipelines', p.id);
return p;
}, (p) => `${p.id} 已创建`);
return h('div', { class: 'stack' },
h('div', { class: 'modal-head' }, h('h3', null, isDebug ? '发起两速查错' : '发起新功能协作'), btn('关闭', { cls: 'ghost sm', onclick: closeModal })),
h('div', null, h('label', { class: 'lbl' }, isDebug ? '故障现象 / 报错描述' : '需求目标'),
field(goalKey, { tag: 'textarea', rows: 3, class: 'inp', placeholder: isDebug ? '例如：POST /login 偶发 500' : '例如：给设置页增加导出 CSV 能力' })),
h('div', null, h('label', { class: 'lbl' }, '模型绑定话术（选填，留空用默认配置）'), bindInp),
pv?.data ? h('div', { class: 'row' }, span('muted', '解析预览：'), ...PLAYBOOK_STAGES[pb].map((k) => span('tag', `${stageName(k)}: ${shorthand(pv.data.resolved?.[k])}`))) : null,
h('div', null, h('label', { class: 'lbl' }, '项目目录（选填）'), field(projKey, { class: 'inp mono', placeholder: '留空自动使用当前会话目录' }, curDir)),
h('label', { class: 'row muted', style: 'font-size:13px;cursor:pointer' },
h('input', { type: 'checkbox', checked: UI.autoConfirm, onchange: (e) => { UI.autoConfirm = e.target.checked; schedule(); } }),
'自动确认绑定并立即启动首阶段'),
h('div', { class: 'row', style: 'justify-content:flex-end' },
btn('取消', { cls: 'ghost', onclick: closeModal }),
btn(isDebug ? '启动两速查错' : '启动新功能协作', { cls: 'primary', busyKey: `mcreate:${pb}`, onclick: submit })));
}
function authModalBody() {
const save = () => {
const t = draft('auth:token').trim();
if (!t) { toast('请输入 token', 'warn'); return; }
localStorage.setItem(TOKEN_KEY, t);
document.cookie = `baton_token=${enc(t)}; path=/; SameSite=Strict`;
UI.drafts.delete('auth:token');
S.authNeeded = false; S.healthFails = 0;
connectSSE(); loadAll();
};
const inp = field('auth:token', { class: 'inp mono', type: 'password', placeholder: '粘贴 baton_token…', autocomplete: 'off' });
inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') save(); });
return h('div', { class: 'stack' },
h('div', { class: 'modal-head' }, h('h3', null, '输入访问 Token')),
h('p', { class: 'muted' }, '请粘贴本地 daemon 生成的 token（位于 ', h('code', null, '<state>/web.token'), '），或直接运行 ', h('code', null, 'baton web'), ' 自动登录。'),
inp,
h('div', { class: 'row', style: 'justify-content:flex-end' }, btn('保存并连接', { cls: 'primary', onclick: save })));
}
async function boot() {
const u = new URL(location.href);
const tk = u.searchParams.get('token');
if (tk) {
localStorage.setItem(TOKEN_KEY, tk);
u.searchParams.delete('token');
history.replaceState(null, '', u.pathname + u.search + u.hash);
}
route();
rerender();
await pollHealth();
await loadAll();
connectSSE();
loadModels();
setInterval(pollHealth, HEALTH_POLL_MS);
setInterval(pollActivePreview, PREVIEW_POLL_MS);
setInterval(() => {
syncStateSilent();
const sel = window.getSelection?.();
if (sel && !sel.isCollapsed && $('.term-viewport')?.contains(sel.anchorNode)) return;
schedule();
}, TICK_MS);
document.addEventListener('visibilitychange', () => { if (!document.hidden) syncStateSilent(); });
}
boot();
