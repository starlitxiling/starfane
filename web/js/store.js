// starfane web console — reactive state store, host helpers, and module callback registry
import {
  AGENTS,
  COLLAPSED_KEY,
  FALLBACK_MODELS,
  GLYPH_MODE_KEY,
  GROUPS_INIT_KEY,
  NON_TMUX_GROUP,
  NON_TMUX_OPEN_KEY,
  OUTLINE_COLLAPSED_KEY,
  PREVIEW_LINES,
  SPAWN_PREFS_KEY,
  STATE_PRIO,
  THEME_KEY,
  TRANSCRIPT_LIMIT,
  fallbackSlash,
  stageLabel,
  starfaneSlash,
  stateLabel,
} from './constants.js';
import { L } from './i18n.js';

export function loadSpawnPrefs() {
  try {
    const o = JSON.parse(localStorage.getItem(SPAWN_PREFS_KEY) || '{}');
    return {
      last_agent: AGENTS.includes(o.last_agent) ? o.last_agent : 'claude',
      by_agent: o.by_agent && typeof o.by_agent === 'object' ? o.by_agent : {},
    };
  } catch {
    return { last_agent: 'claude', by_agent: {} };
  }
}

export function saveSpawnPrefs(agent, patch) {
  try {
    const p = loadSpawnPrefs();
    p.last_agent = agent;
    p.by_agent[agent] = { ...(p.by_agent[agent] || {}), ...patch };
    localStorage.setItem(SPAWN_PREFS_KEY, JSON.stringify(p));
  } catch { /* ignore storage quota errors */ }
}

export function loadCollapsedGroups() {
  try {
    const raw = localStorage.getItem(COLLAPSED_KEY);
    const set = raw ? new Set(JSON.parse(raw)) : new Set();
    if (localStorage.getItem(NON_TMUX_OPEN_KEY) !== '1') set.add(NON_TMUX_GROUP);
    return set;
  } catch {
    return new Set([NON_TMUX_GROUP]);
  }
}

const COLLAPSED_HOSTS_KEY = 'starfane_collapsed_hosts';

export function loadCollapsedHosts() {
  try {
    const raw = localStorage.getItem(COLLAPSED_HOSTS_KEY);
    return raw ? new Set(JSON.parse(raw)) : new Set();
  } catch {
    return new Set();
  }
}

export function saveCollapsedHosts() {
  try {
    localStorage.setItem(COLLAPSED_HOSTS_KEY, JSON.stringify([...UI.collapsedHosts]));
  } catch { /* ignore */ }
}

export function saveCollapsedGroups() {
  try {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...UI.collapsedGroups]));
    localStorage.setItem(GROUPS_INIT_KEY, '1');
  } catch { /* ignore */ }
}

function loadGlyphMode() {
  try {
    const v = localStorage.getItem(GLYPH_MODE_KEY);
    return v === 'compat' || v === 'raw' || v === 'auto' ? v : 'auto';
  } catch {
    return 'auto';
  }
}

export const S = {
  conn: 'connecting',
  restarting: false,
  healthFails: 0,
  version: '',
  uiHash: '',
  uiUpdateAvailable: false,
  uiUpdateHash: '',
  uptimeMs: null,
  lastSeq: 0,
  lastLoad: 0,
  lastSseAt: 0,
  loaded: false,
  loadError: null,
  authEnabled: true,
  authNeeded: false,
  retry: 0,
  es: null,
  reconnectTimer: 0,
  sessions: new Map(),
  hosts: new Map(),
  pipelines: new Map(),
  attention: [],
  models: null,
  tmuxSessions: [],
  tmuxSessionsByHost: new Map(),
  terminalFont: '',
  slashCmds: new Map(),
  slashFetching: new Set(),
  reviews: new Map(),
  reviewsState: 'idle',
  reviewsErr: null,
  precision: null,
  precisionState: 'idle',
  precisionErr: null,
  artifacts: new Map(),
  handoffs: new Map(),
  previews: new Map(),
  transcripts: new Map(),
  liveTails: new Map(),
  todayUsage: null,
  usage: null,
  usageDays: 14,
  usageState: 'idle',
  usageErr: null,
  usageSelDate: null,
  activity: [],
  muteUntil: Number(localStorage.getItem('starfane_mute_until') || 0),
  channelMutes: {},
  notifiers: [],
  notifiersLoaded: false,
};

const THEME_MEDIA = typeof window.matchMedia === 'function'
  ? window.matchMedia('(prefers-color-scheme: dark)')
  : null;

export function loadThemePref() {
  try {
    const raw = localStorage.getItem(THEME_KEY);
    if (raw === 'dark' || raw === 'light') return raw;
  } catch (err) {
    console.debug('theme storage read skipped:', err);
  }
  return 'auto';
}

export function resolveTheme(pref) {
  if (pref === 'dark' || pref === 'light') return pref;
  return THEME_MEDIA?.matches ? 'dark' : 'light';
}

const INIT_THEME_PREF = loadThemePref();

export const UI = {
  themePref: INIT_THEME_PREF,
  theme: resolveTheme(INIT_THEME_PREF),
  view: 'workspace',
  arg: null,
  modal: null,
  drawer: null,
  renameHostId: null,
  suspendFreshSessionId: null,
  spawnMissingDir: null,
  spawnErr: '',
  selSession: null,
  composerMode: 'direct',
  outputMode: 'reading',
  autoRefreshPreview: true,
  autoConfirm: true,
  showEnded: false,
  agentFilter: '',
  pipeFilter: 'all',
  revTab: 'reports',
  precAgent: '',
  selStage: {},
  parsePreviews: {},
  drafts: new Map(),
  attachments: new Map(),
  optimisticTurns: new Map(),
  forceScrollBottomSid: null,
  busy: new Set(),
  open: new Set(),
  trLimit: new Map(),
  pvLines: new Map(),
  loadingEarlierSid: null,
  loadingEarlierPane: null,
  drawerPvLines: 1000,
  termGlyphMode: loadGlyphMode(),
  promptWriteIn: new Map(),
  slash: { open: false, start: 0, end: 0, query: '', items: [], idx: 0 },
  collapsedGroups: loadCollapsedGroups(),
  collapsedHosts: loadCollapsedHosts(),
  groupsInited: !!localStorage.getItem(GROUPS_INIT_KEY),
  outlineCollapsed: localStorage.getItem(OUTLINE_COLLAPSED_KEY) === '1',
  mobileSessionsOpen: false,
  jumpLockUntil: 0,
  muteMenu: false,
  muteTarget: 'all',
  muteCustomVal: '45',
  muteCustomUnit: 'm',
  muteTimeVal: '',
  wsShell: null,
  topShell: null,
};

export const resolvedTheme = () => resolveTheme(UI.themePref);

export function applyTheme(pref = UI.themePref, { persist = true } = {}) {
  const cleanPref = pref === 'dark' || pref === 'light' ? pref : 'auto';
  const nextTheme = resolveTheme(cleanPref);
  UI.themePref = cleanPref;
  UI.theme = nextTheme;
  const root = document.documentElement;
  if (root) {
    root.dataset.theme = nextTheme;
    root.dataset.themePref = cleanPref;
  }
  const meta = document.querySelector?.('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', nextTheme === 'dark' ? '#141413' : '#FAF9F6');
  if (persist) {
    try {
      if (cleanPref === 'auto') localStorage.removeItem(THEME_KEY);
      else localStorage.setItem(THEME_KEY, cleanPref);
    } catch (err) {
      console.debug('theme storage write skipped:', err);
    }
  }
  return nextTheme;
}

export function toggleTheme({ resetAuto = false } = {}) {
  if (resetAuto) return applyTheme('auto', { persist: true });
  return applyTheme(resolvedTheme() === 'dark' ? 'light' : 'dark', { persist: true });
}

if (THEME_MEDIA) {
  const onSystemThemeChange = () => {
    if (UI.themePref !== 'auto') return;
    applyTheme('auto', { persist: false });
    APP.rerender();
  };
  if (typeof THEME_MEDIA.addEventListener === 'function') {
    THEME_MEDIA.addEventListener('change', onSystemThemeChange);
  } else if (typeof THEME_MEDIA.addListener === 'function') {
    THEME_MEDIA.addListener(onSystemThemeChange);
  }
}

// Cross-module callback registry wired at boot in web/app.js (prevents circular ES module imports).
export const APP = {
  schedule: () => {},
  schedulePassive: () => {},
  rerender: () => {},
  go: () => {},
  openSession: () => {},
  openDrawer: () => {},
  closeDrawer: () => {},
  openSpawnModal: () => {},
  openRenameHostModal: () => {},
  closeModal: () => {},
  notifyOutputUpdated: () => {},
  updateOutputClock: () => {},
  patchTelemetryStrip: () => {},
  compactTranscript: () => {},
  collectUserPrompts: () => [],
  jumpToPrompt: () => {},
  loadAll: async () => {},
  syncStateSilent: async () => {},
  loadSessionOutput: () => {},
  loadTranscript: async () => {},
  loadEarlierTranscript: async () => {},
  loadAllTranscript: async () => {},
  loadPreview: async () => {},
  loadEarlierPreview: async () => {},
  loadModels: async () => {},
  loadTmuxSessions: async () => {},
  loadSlashCommands: async () => {},
  loadHandoffs: async () => {},
  loadReviews: async () => {},
  refreshReview: async () => {},
  loadPrecision: async () => {},
  connectSSE: () => {},
  renameHost: async () => {},
  updateSlashFromInput: () => {},
  renderAttachBar: () => {},
  closeSlashMenu: () => {},
  ackSessionError: () => {},
};

let attachSeq = 0;
export const nextAttachId = () => `att-${++attachSeq}`;
export const trReqSeq = new Map();
export const pvReqSeq = new Map();
export const trLimitOf = (sid) => UI.trLimit.get(sid) || TRANSCRIPT_LIMIT;
export const pvModeOf = (pane) => UI.pvLines.get(pane) || PREVIEW_LINES;
export const pvKeyOf = (pane, mode = pvModeOf(pane)) => `${pane}:${mode}`;

export const MAX_CACHED_TRANSCRIPTS = 12;
export const MAX_CACHED_PREVIEWS = 12;
export const MAX_CACHED_LIVE_TAILS = 24;
export const COMPACT_TRANSCRIPT_THRESHOLD = 120;
export const OPTIMISTIC_EXPIRE_MS = 45000;
const HEAVY_PREVIEW_CHAR_LIMIT = 512 * 1024;

export function setLiveTailEntry(sessionId, text, updatedMs = Date.now()) {
  if (!sessionId || typeof text !== 'string') return;
  S.liveTails.delete(sessionId);
  S.liveTails.set(sessionId, { text, hash: hashStr(text), updatedMs });
  const activeSid = UI.selSession;
  while (S.liveTails.size > MAX_CACHED_LIVE_TAILS) {
    let victim = null;
    for (const sid of S.liveTails.keys()) {
      if (sid !== sessionId && sid !== activeSid) { victim = sid; break; }
    }
    if (!victim) break;
    S.liveTails.delete(victim);
  }
}

export function normalizeTurnText(str) {
  return String(str || '').replace(/\r\n?/g, '\n').replace(/\s+/g, ' ').trim();
}

function isSyntheticUserPrompt(norm) {
  return (
    norm.startsWith('[后台任务')
    || norm.startsWith('[Swarm 通知]')
    || /^\[来自 [^\]]+ 消息\]/.test(norm)
    || norm.startsWith('Task id "')
  );
}

function stripAttachTokens(norm) {
  return norm.replace(/\[(?:图片|Image|粘贴|Paste)\s+#\d+\]/g, ' ').replace(/\s+/g, ' ').trim();
}

function optimisticMatchesMessage(item, msg, absSeq) {
  if (!item || !msg || msg.role !== 'user') return false;
  if (absSeq < (Number.isFinite(item.anchor_total) ? item.anchor_total : 0)) return false;
  const mNorm = normalizeTurnText(msg.text);
  if (isSyntheticUserPrompt(mNorm)) return false;
  const itemNorm = normalizeTurnText(item.text);
  if (itemNorm) {
    if (mNorm === itemNorm) return true;
    if (mNorm.includes(itemNorm) || itemNorm.includes(mNorm)) return true;
    const plain = stripAttachTokens(itemNorm);
    if (plain.length >= 4 && mNorm.includes(plain)) return true;
  }
  const atts = Array.isArray(item.attachments) ? item.attachments : [];
  if (atts.length) {
    for (let i = 0; i < atts.length; i++) {
      const a = atts[i];
      if (a.path && mNorm.includes(String(a.path).trim())) return true;
      if (a.content) {
        const snippet = normalizeTurnText(a.content).slice(0, 32);
        if (snippet.length >= 8 && mNorm.includes(snippet)) return true;
      }
    }
    const plain = stripAttachTokens(itemNorm);
    if (!plain && mNorm) return true;
  }
  if (item.status === 'sent' && typeof msg.ts_ms === 'number' && Number.isFinite(msg.ts_ms) && msg.ts_ms >= (item.created_ms || 0) - 5000) {
    return true;
  }
  return false;
}

export function addOptimisticTurn(sid, entry) {
  if (!sid || !entry || !entry.nonce) return null;
  const list = UI.optimisticTurns.get(sid) || [];
  list.push(entry);
  UI.optimisticTurns.set(sid, list);
  return entry;
}

export function updateOptimisticTurnStatus(sid, nonce, status) {
  if (!sid || !nonce) return false;
  const list = UI.optimisticTurns.get(sid);
  if (!list || !list.length) return false;
  const item = list.find((x) => x.nonce === nonce);
  if (!item || item.status === status) return false;
  item.status = status;
  return true;
}

export function removeOptimisticTurn(sid, nonce) {
  if (!sid || !nonce) return null;
  const list = UI.optimisticTurns.get(sid);
  if (!list || !list.length) return null;
  const idx = list.findIndex((x) => x.nonce === nonce);
  if (idx < 0) return null;
  const [removed] = list.splice(idx, 1);
  if (removed?._expireTimer) {
    clearTimeout(removed._expireTimer);
    removed._expireTimer = 0;
  }
  if (!list.length) UI.optimisticTurns.delete(sid);
  return removed;
}

export function hasOptimisticTurn(sid, nonce) {
  const list = sid ? UI.optimisticTurns.get(sid) : null;
  if (!list || !list.length) return false;
  return nonce ? list.some((x) => x.nonce === nonce) : true;
}

export function reconcileOptimisticTurns(sid, tr = S.transcripts.get(sid)) {
  if (!sid) return false;
  const list = UI.optimisticTurns.get(sid);
  if (!list || !list.length) return false;
  const now = Date.now();
  const hasMsgs = tr?.state === 'ok' && tr.source === 'transcript' && Array.isArray(tr.messages) && tr.messages.length > 0;
  const startSeq = hasMsgs && Number.isFinite(tr.startSeq) ? tr.startSeq : 0;
  const consumedSeqs = new Set();
  const kept = [];
  let changed = false;
  for (let i = 0; i < list.length; i++) {
    const item = list[i];
    if (item.status === 'sent' && (now - (item.created_ms || 0)) >= OPTIMISTIC_EXPIRE_MS) {
      if (item._expireTimer) { clearTimeout(item._expireTimer); item._expireTimer = 0; }
      changed = true;
      continue;
    }
    let matched = false;
    if (hasMsgs) {
      for (let j = 0; j < tr.messages.length; j++) {
        const absSeq = startSeq + j;
        if (consumedSeqs.has(absSeq)) continue;
        if (optimisticMatchesMessage(item, tr.messages[j], absSeq)) {
          consumedSeqs.add(absSeq);
          matched = true;
          break;
        }
      }
    }
    if (matched) {
      if (item._expireTimer) { clearTimeout(item._expireTimer); item._expireTimer = 0; }
      changed = true;
      continue;
    }
    kept.push(item);
  }
  if (changed) {
    if (kept.length) UI.optimisticTurns.set(sid, kept);
    else UI.optimisticTurns.delete(sid);
  }
  return changed;
}

export function hashStr(str) {
  let h = 2166136261;
  const s = String(str || '');
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0).toString(36);
}

/**
 * Memoized per-message content hash stored as a non-enumerable `_h` property so repeated
 * tail polls, "Load earlier" merges, and incremental DOM card reconciliation avoid re-hashing
 * megabytes of unchanged messages.
 */
export function hashMessage(msg) {
  if (!msg) return '0';
  if (typeof msg._h === 'string' && msg._h) return msg._h;
  const tools = Array.isArray(msg.tools) && msg.tools.length ? msg.tools.join(',') : '';
  const txt = String(msg.text || '');
  const h = hashStr(`${msg.role || ''}|${msg.kind || ''}|${msg.ts_ms || 0}|${msg.delegation || ''}|${tools}|${txt.length}|${txt}`);
  try {
    Object.defineProperty(msg, '_h', { value: h, configurable: true, enumerable: false });
  } catch {
    msg._h = h;
  }
  return h;
}

export function hashTranscriptPayload(source, startSeq, totalMessages, hasMore, msgs) {
  let acc = 2166136261;
  const feed = (s) => {
    for (let i = 0; i < s.length; i++) {
      acc ^= s.charCodeAt(i);
      acc = Math.imul(acc, 16777619);
    }
  };
  feed(`${source}|${startSeq}|${totalMessages}|${hasMore ? 1 : 0}|${msgs.length}`);
  for (let i = 0; i < msgs.length; i++) {
    feed('|');
    feed(hashMessage(msgs[i]));
  }
  return (acc >>> 0).toString(36);
}

/**
 * Compact a loaded transcript back to the default trailing `TRANSCRIPT_LIMIT` window
 * so full-history views don't permanently retain thousands of messages in RAM.
 */
export function compactTranscriptEntry(sessionId) {
  if (!sessionId) return false;
  const tr = S.transcripts.get(sessionId);
  UI.trLimit.delete(sessionId);
  if (!tr || tr.state !== 'ok' || tr.source !== 'transcript') return false;
  if (tr.messages.length <= TRANSCRIPT_LIMIT && !tr.explicitAll && !tr.autoPollPaused) return false;
  const tail = tr.messages.length > TRANSCRIPT_LIMIT ? tr.messages.slice(-TRANSCRIPT_LIMIT) : tr.messages;
  const dropped = tr.messages.length - tail.length;
  const nextStartSeq = (Number.isFinite(tr.startSeq) ? tr.startSeq : 0) + dropped;
  const nextHasMore = nextStartSeq > 0;
  const totalMessages = Math.max(tr.totalMessages || 0, nextStartSeq + tail.length);
  const nextHash = hashTranscriptPayload(tr.source, nextStartSeq, totalMessages, nextHasMore, tail);
  S.transcripts.delete(sessionId);
  S.transcripts.set(sessionId, {
    ...tr,
    messages: tail,
    startSeq: nextStartSeq,
    totalMessages,
    hasMore: nextHasMore,
    nextBefore: nextHasMore ? nextStartSeq : null,
    explicitAll: false,
    loadedAll: !nextHasMore,
    autoPollPaused: false,
    latestTotalMessages: null,
    limit: TRANSCRIPT_LIMIT,
    hash: nextHash,
  });
  return true;
}

/**
 * Insert/update a transcript cache entry with MRU ordering, bounded LRU capacity,
 * and automatic compaction of inactive oversized histories.
 */
export function setTranscriptEntry(sessionId, entry) {
  if (!sessionId) return;
  S.transcripts.delete(sessionId);
  S.transcripts.set(sessionId, entry);
  const activeSid = UI.selSession;
  // Compact any inactive heavy entries first so RAM stays bounded even before eviction.
  for (const [sid, tr] of S.transcripts.entries()) {
    if (sid === sessionId || sid === activeSid) continue;
    if (tr?.state === 'ok' && (tr.messages?.length > COMPACT_TRANSCRIPT_THRESHOLD || tr.explicitAll)) {
      compactTranscriptEntry(sid);
    }
  }
  while (S.transcripts.size > MAX_CACHED_TRANSCRIPTS) {
    let victim = null;
    for (const sid of S.transcripts.keys()) {
      if (sid !== sessionId && sid !== activeSid) { victim = sid; break; }
    }
    if (!victim) break;
    S.transcripts.delete(victim);
    UI.trLimit.delete(victim);
  }
}

/**
 * Insert/update a terminal preview entry with MRU ordering, bounded LRU capacity,
 * and immediate reclamation of inactive `all` / >512 KiB scrollback buffers.
 */
export function setPreviewEntry(key, entry) {
  if (!key) return;
  S.previews.delete(key);
  S.previews.set(key, entry);
  const cur = UI.selSession ? S.sessions.get(UI.selSession) : null;
  const activeKey = cur?.pane ? pvKeyOf(cur.pane) : null;
  const drawerKey = UI.drawer?.kind === 'terminal' && UI.drawer.pane
    ? pvKeyOf(UI.drawer.pane, UI.drawerPvLines)
    : null;
  for (const [k, pv] of [...S.previews.entries()]) {
    if (k === key || k === activeKey || k === drawerKey) continue;
    if (k.endsWith(':all') || pv?.paginated || (pv?.text?.length || 0) > HEAVY_PREVIEW_CHAR_LIMIT) {
      S.previews.delete(k);
    }
  }
  while (S.previews.size > MAX_CACHED_PREVIEWS) {
    let victim = null;
    for (const k of S.previews.keys()) {
      if (k !== key && k !== activeKey && k !== drawerKey) { victim = k; break; }
    }
    if (!victim) break;
    S.previews.delete(victim);
  }
}

/**
 * Release heavy transcript (>120 messages or `all`) and heavy terminal preview (`all` / >512 KiB)
 * buffers when the user switches away from a session so one full-history window never bloats RAM.
 */
export function onLeaveSession(prevSid, nextSid) {
  if (!prevSid || prevSid === nextSid) return;
  const prevTr = S.transcripts.get(prevSid);
  if (prevTr && (prevTr.messages?.length > COMPACT_TRANSCRIPT_THRESHOLD || prevTr.explicitAll)) {
    compactTranscriptEntry(prevSid);
  }
  const prevSess = S.sessions.get(prevSid);
  const pane = prevSess?.pane;
  if (!pane) return;
  const drawerPane = UI.drawer?.kind === 'terminal' ? UI.drawer.pane : null;
  if (drawerPane === pane) return;
  const mode = UI.pvLines.get(pane);
  if (mode === 'all') {
    UI.pvLines.delete(pane);
    S.previews.delete(pvKeyOf(pane, 'all'));
  }
  const curKey = pvKeyOf(pane, mode || PREVIEW_LINES);
  const pv = S.previews.get(curKey);
  if (pv && (pv.paginated || (pv.text?.length || 0) > HEAVY_PREVIEW_CHAR_LIMIT)) {
    S.previews.delete(curKey);
  }
}

export const enc = encodeURIComponent;
export const draft = (key, def = '') => (UI.drafts.has(key) ? UI.drafts.get(key) : def);
export const base = (p) => (p ? String(p).replace(/\/+$/, '').split('/').pop() || p : '—');
export const stateText = (s) => stateLabel(s);
export const stageName = (k) => stageLabel(k);
export const groupDisplayName = (g) => (g === NON_TMUX_GROUP ? L('非 tmux / 后台', 'Non-tmux / Background') : g);
export const short = (s, n = 8) => (s ? String(s).slice(0, n) : '—');

export const RTT_FAST_MAX_MS = 100;
export const RTT_MEDIUM_MAX_MS = 300;
const EPOCH_MS_THRESHOLD = 1_000_000_000_000;

export function hostOf(s) {
  return (s?.host_id && String(s.host_id).trim()) || 'local';
}

export function isRemoteHost(hostId) {
  const id = (hostId && String(hostId).trim()) || 'local';
  return id !== 'local';
}

export function hostRecord(hostId) {
  const id = (hostId && String(hostId).trim()) || 'local';
  return S.hosts.get(id) || null;
}

export function sortedHosts() {
  const list = [...S.hosts.values()].filter((h) => h && h.id);
  if (!list.length) {
    return [{ id: 'local', display_name: L('当前主机', 'This Host'), kind: 'local', status: 'online' }];
  }
  return list.sort((a, b) => {
    if (a.id === 'local') return -1;
    if (b.id === 'local') return 1;
    return a.id.localeCompare(b.id);
  });
}

export function hostStatusLabel(status) {
  return {
    online: L('在线', 'Online'),
    degraded: L('连接降级', 'Degraded'),
    offline: L('已离线', 'Offline'),
  }[status] || String(status || '');
}

export function rttTone(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) return 'fast';
  if (ms <= RTT_FAST_MAX_MS) return 'fast';
  if (ms <= RTT_MEDIUM_MAX_MS) return 'med';
  return 'slow';
}

export function fmtRtt(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

export function formatRetryCountdown(nextRetryMs) {
  if (typeof nextRetryMs !== 'number' || !Number.isFinite(nextRetryMs) || nextRetryMs <= 0) return '';
  const remMs = nextRetryMs > EPOCH_MS_THRESHOLD
    ? Math.max(0, nextRetryMs - Date.now())
    : nextRetryMs;
  if (remMs <= 0) return '';
  const secs = Math.ceil(remMs / 1000);
  return L(`${secs}s 后重试`, `retry in ${secs}s`);
}

export function hostDisplayName(hostId, fallbackSession = null) {
  const id = (hostId && String(hostId).trim()) || 'local';
  const rec = S.hosts.get(id);
  if (rec?.display_name && !/^local$/i.test(rec.display_name.trim())) return rec.display_name;
  if (fallbackSession?.host_display_name && !/^local$/i.test(fallbackSession.host_display_name.trim())) {
    return fallbackSession.host_display_name;
  }
  return id === 'local' ? L('当前主机', 'This Host') : id;
}

export function shortPaneId(s) {
  if (!s?.pane) return '';
  const raw = String(s.pane);
  const hid = hostOf(s);
  if (hid !== 'local' && raw.startsWith(`${hid}:`)) return raw.slice(hid.length + 1);
  return raw;
}

export function paneDisplayId(s) {
  const raw = shortPaneId(s);
  if (!raw) return '';
  const name = hostDisplayName(hostOf(s), s);
  return name ? `${name}:${raw}` : raw;
}

export function muxKindOf(s) {
  if (s?.mux_kind) return s.mux_kind;
  const p = String(s?.pane || '');
  if (p.startsWith('zj:') || p.includes(':zj:')) return 'zellij';
  return 'tmux';
}

export function isTmuxSession(s) {
  const hasPane = Boolean(s?.pane && String(s.pane).trim());
  if (s?.state !== 'ended') return hasPane;
  return hasPane || Boolean(s?.tmux_target && String(s.tmux_target).trim());
}

export function tmuxGroupOf(s) {
  if (s?.state !== 'ended' && !(s?.pane && String(s.pane).trim())) return NON_TMUX_GROUP;
  if (s.tmux_target && String(s.tmux_target).includes(':')) {
    const raw = String(s.tmux_target);
    if (raw.startsWith('zj:')) return raw.slice(3).split('/')[0] || 'zellij';
    return raw.split(':')[0];
  }
  if (s.tmux_target && String(s.tmux_target).trim()) return String(s.tmux_target).trim();
  if (s.pane && String(s.pane).trim()) {
    if (s.tmux_label && String(s.tmux_label).includes('-')) return String(s.tmux_label).split('-')[0];
    return muxKindOf(s) === 'zellij' ? 'zellij' : 'tmux';
  }
  return NON_TMUX_GROUP;
}

export function modelBaseOf(model, effort) {
  if (!model) return '';
  if (effort && model.toLowerCase().endsWith(`-${effort.toLowerCase()}`)) {
    return model.slice(0, -(effort.length + 1));
  }
  return model;
}

export function modelEffortLabel(model, effort) {
  const m = modelBaseOf(model, effort);
  if (m && effort) return `${m} · ${effort}`;
  return m || effort || '';
}

export function sessionLabel(s) {
  if (s.tmux_label) return s.tmux_label;
  if (s.tmux_target) return s.tmux_target;
  return `${s.agent} ${s.pane || base(s.git_root || s.cwd)}`.trim();
}

export function subagentDisplayName(c) {
  if (c.tmux_label) return c.tmux_label;
  const id = String(c.session_id || '');
  if (id.includes('/')) return id.split('/').slice(1).join('/');
  return short(id, 14);
}

export function setMute(until) {
  S.muteUntil = typeof until === 'number' && Number.isFinite(until) && until > 0 ? until : 0;
  try {
    localStorage.setItem('starfane_mute_until', String(S.muteUntil));
  } catch { /* ignore */ }
}

function cleanMuteMs(v) {
  return typeof v === 'number' && Number.isFinite(v) && v > 0 ? v : 0;
}

function normalizeNotifierItem(item, globalUntil, channelMutes) {
  if (typeof item === 'string') {
    const id = item.trim();
    if (!id) return null;
    const chUntil = cleanMuteMs(channelMutes[id]);
    const eff = Math.max(globalUntil || 0, chUntil);
    return {
      id,
      kind: id.startsWith('feishu') ? 'feishu' : 'webhook',
      ownership: 'node_local',
      channel_muted_until_ms: chUntil || null,
      muted_until_ms: eff || null,
    };
  }
  if (!item || typeof item !== 'object') return null;
  const id = String(item.id || '').trim();
  if (!id) return null;
  const chUntil = cleanMuteMs(channelMutes[id] ?? item.channel_muted_until_ms);
  const eff = Math.max(globalUntil || 0, chUntil, cleanMuteMs(item.muted_until_ms));
  return {
    id,
    kind: String(item.kind || 'webhook').toLowerCase(),
    ownership: String(item.ownership || 'node_local'),
    channel_muted_until_ms: chUntil || null,
    muted_until_ms: eff || null,
  };
}

export function notifiersDiffSig() {
  const chMutes = Object.entries(S.channelMutes || {})
    .filter(([, v]) => cleanMuteMs(v) > 0)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${k}=${v}`)
    .join(',');
  const list = (S.notifiers || [])
    .map((n) => `${n.id}|${n.kind}|${n.ownership}|${n.channel_muted_until_ms || 0}|${n.muted_until_ms || 0}`)
    .join(';');
  return `${S.notifiersLoaded ? 1 : 0}|${S.muteUntil || 0}|${chMutes}|${list}`;
}

export function applyNotifiersSnapshot(payload) {
  if (!payload || typeof payload !== 'object') return false;
  const beforeSig = notifiersDiffSig();

  const hasGlobal = Object.prototype.hasOwnProperty.call(payload, 'global_mute_until_ms');
  const hasChannelMutes = payload.channel_mutes && typeof payload.channel_mutes === 'object' && !Array.isArray(payload.channel_mutes);
  const hasDetails = Array.isArray(payload.details);
  const hasNotifiers = Array.isArray(payload.notifiers);

  if (hasGlobal) {
    setMute(cleanMuteMs(payload.global_mute_until_ms));
  } else if (!hasChannelMutes && !hasDetails && Object.prototype.hasOwnProperty.call(payload, 'feishu_muted_until_ms')) {
    setMute(cleanMuteMs(payload.feishu_muted_until_ms));
  }

  if (hasChannelMutes) {
    const nextMutes = {};
    for (const [k, v] of Object.entries(payload.channel_mutes)) {
      const id = String(k || '').trim();
      const ms = cleanMuteMs(v);
      if (id && ms > 0) nextMutes[id] = ms;
    }
    S.channelMutes = nextMutes;
  }

  let rawList = null;
  if (hasDetails) {
    rawList = payload.details;
  } else if (hasNotifiers) {
    // In GET /api/state, `notifiers` is the full detail object array (or [] when 0 configured).
    // In legacy GET /api/notifiers without `details`, `notifiers` may be string[].
    rawList = payload.notifiers;
  }

  if (rawList !== null) {
    if (!hasChannelMutes) {
      const derived = {};
      for (const item of rawList) {
        if (item && typeof item === 'object' && item.id) {
          const ms = cleanMuteMs(item.channel_muted_until_ms);
          if (ms > 0) derived[String(item.id)] = ms;
        }
      }
      S.channelMutes = derived;
    }
    const normalized = rawList
      .map((item) => normalizeNotifierItem(item, S.muteUntil, S.channelMutes))
      .filter(Boolean)
      .sort((a, b) => a.id.localeCompare(b.id));
    S.notifiers = normalized;
    S.notifiersLoaded = true;
  } else if (hasGlobal || hasChannelMutes) {
    // SSE `notifiers` event updates mute timestamps without re-sending the static channel list.
    S.notifiers = (S.notifiers || []).map((n) => {
      const chUntil = cleanMuteMs(S.channelMutes[n.id]);
      const eff = Math.max(S.muteUntil || 0, chUntil);
      return {
        ...n,
        channel_muted_until_ms: chUntil || null,
        muted_until_ms: eff || null,
      };
    });
  }

  if (UI.muteTarget !== 'all' && !S.notifiers.some((n) => n.id === UI.muteTarget)) {
    UI.muteTarget = 'all';
  }

  return notifiersDiffSig() !== beforeSig;
}

export const globalMuted = (now = Date.now()) => S.muteUntil > now;

export function channelMuteUntil(id, now = Date.now()) {
  if (!id) return 0;
  const ms = cleanMuteMs(S.channelMutes?.[id]);
  return ms > now ? ms : 0;
}

export function effectiveMuteUntil(id, now = Date.now()) {
  const g = S.muteUntil > now ? S.muteUntil : 0;
  const c = channelMuteUntil(id, now);
  return Math.max(g, c);
}

export function activeChannelMuteIds(now = Date.now()) {
  const known = S.notifiers.length ? new Set(S.notifiers.map((n) => n.id)) : null;
  const ids = [];
  for (const [id, v] of Object.entries(S.channelMutes || {})) {
    const ms = cleanMuteMs(v);
    if (ms > now && (!known || known.has(id))) ids.push(id);
  }
  ids.sort();
  return ids;
}

export function mutedChannelCount(now = Date.now()) {
  if (globalMuted(now)) return S.notifiers.length;
  return activeChannelMuteIds(now).length;
}

export const anyMuted = (now = Date.now()) => globalMuted(now) || activeChannelMuteIds(now).length > 0;
export const muted = () => anyMuted();

export function notifyTopSig() {
  const now = Date.now();
  const minBucket = (ts) => (ts > now ? Math.ceil((ts - now) / 60000) : 0);
  const gBucket = minBucket(S.muteUntil);
  const chBuckets = activeChannelMuteIds(now)
    .map((id) => `${id}:${minBucket(channelMuteUntil(id, now))}`)
    .join(',');
  const busyKeys = [...UI.busy]
    .filter((k) => k === 'mute' || k.startsWith('mute:') || k.startsWith('notify-test:'))
    .sort()
    .join(',');
  return `${notifiersDiffSig()}|${gBucket}|${chBuckets}|${UI.muteMenu ? 1 : 0}|${UI.muteTarget}|${busyKeys}`;
}

export const connState = () => (S.restarting ? 'restarting' : (S.healthFails >= 2 ? 'down' : S.conn));

export function rootSessions() {
  return [...S.sessions.values()]
    .filter((s) => !s.is_subagent && (UI.showEnded || s.state !== 'ended'))
    .sort((a, b) => (STATE_PRIO[a.state] ?? 5) - (STATE_PRIO[b.state] ?? 5) || (b.last_event_ms || 0) - (a.last_event_ms || 0));
}

export function subagentsOf(parentId) {
  return [...S.sessions.values()].filter((s) => s.is_subagent && s.parent_session_id === parentId);
}

export function currentSession() {
  if (UI.selSession && S.sessions.has(UI.selSession)) return S.sessions.get(UI.selSession);
  return rootSessions()[0] || null;
}

export const allPipelines = () => [...S.pipelines.values()].sort((a, b) => (b.updated_ms || 0) - (a.updated_ms || 0));
export const isFinished = (p) => ['done', 'cancelled', 'failed'].includes(p.status);
export const stageOf = (p, k) => (p.stages || []).find((s) => s.id === k);

export function activeStage(p) {
  const st = p.stages || [];
  return st.find((s) => ['awaiting_human', 'blocked', 'failed'].includes(s.status))
    || st.find((s) => s.status === 'running')
    || st.find((s) => s.status === 'pending')
    || st[st.length - 1];
}

export const lastArtifact = (st) => (st && st.artifacts && st.artifacts.length ? st.artifacts[st.artifacts.length - 1] : null);

export function inboxItems() {
  const items = [];
  const seen = new Set();
  const add = (kind, o) => {
    const key = `${kind}:${o.pid || o.sid || o.rid || ''}`;
    if (seen.has(key)) return;
    seen.add(key);
    items.push({ key, kind, ...o });
  };
  for (const a of S.attention) {
    const kind = a.kind === 'blocked' && a.session_id ? 'session_alert' : a.kind;
    add(kind, { pid: a.pipeline_id, sid: a.session_id, rid: a.review_id, title: a.title, since: a.since_ms });
  }
  for (const p of S.pipelines.values()) {
    if (p.status === 'awaiting_binding_confirm') add('binding_confirm', { pid: p.id, title: L(`确认绑定：${p.goal}`, `Confirm bindings: ${p.goal}`), since: p.updated_ms });
    const ah = (p.stages || []).find((s) => s.status === 'awaiting_human');
    if (p.status === 'awaiting_human' && ah) {
      if (ah.id === 'plan') add('plan_approval', { pid: p.id, title: L(`批准计划：${p.goal}`, `Approve plan: ${p.goal}`), since: p.updated_ms });
      else if (ah.id === 'triage') add('triage_ready', { pid: p.id, title: L(`快筛结论待决：${p.goal}`, `Triage decision needed: ${p.goal}`), since: p.updated_ms });
      else add('blocked', { pid: p.id, title: L(`${stageName(ah.id)} 需要人工处理：${p.goal}`, `${stageName(ah.id)} needs human action: ${p.goal}`), since: p.updated_ms });
    }
    if (p.status === 'blocked') add('blocked', { pid: p.id, title: L(`流程阻塞：${p.goal}`, `Pipeline blocked: ${p.goal}`), since: p.updated_ms });
  }
  for (const s of S.sessions.values()) {
    if (!s.is_subagent && ['needs_permission', 'needs_input', 'error'].includes(s.state)) {
      add('session_alert', { sid: s.session_id, title: `${sessionLabel(s)} · ${stateText(s.state)}`, since: s.state_since_ms });
    }
  }
  const prio = {
    session_alert: 0,
    binding_confirm: 1,
    plan_approval: 2,
    triage_ready: 3,
    blocked: 4,
    needs_human_finding: 5,
    delivery_unconfirmed: 6,
  };
  return items.sort((a, b) => (prio[a.kind] ?? 9) - (prio[b.kind] ?? 9) || (a.since || 0) - (b.since || 0));
}

export const alertCount = () => inboxItems().length;

export function knownProjects(hostId = null) {
  const scopeHost = hostId != null && String(hostId).trim() ? String(hostId).trim() : null;
  const set = new Set();
  for (const s of S.sessions.values()) {
    if (scopeHost && hostOf(s) !== scopeHost) continue;
    if (s.git_root) set.add(s.git_root);
    else if (s.cwd) set.add(s.cwd);
  }
  if (!scopeHost || scopeHost === 'local') {
    for (const p of S.pipelines.values()) if (p.project?.root) set.add(p.project.root);
  }
  return [...set];
}

export function migrateSessionSelection(oldId, newId) {
  if (!oldId || !newId || oldId === newId) return;
  if (UI.selSession === oldId) UI.selSession = newId;
  if (UI.arg === oldId) {
    UI.arg = newId;
    if (UI.view === 'workspace') history.replaceState(null, '', `#/workspace/${enc(newId)}`);
  }
  const dkOld = `ws:msg:${oldId}`;
  const dkNew = `ws:msg:${newId}`;
  if (UI.drafts.has(dkOld) && !UI.drafts.has(dkNew)) UI.drafts.set(dkNew, UI.drafts.get(dkOld));
  if (UI.attachments.has(oldId) && !UI.attachments.has(newId)) UI.attachments.set(newId, UI.attachments.get(oldId));
  if (UI.optimisticTurns.has(oldId) && !UI.optimisticTurns.has(newId)) {
    UI.optimisticTurns.set(newId, UI.optimisticTurns.get(oldId));
    UI.optimisticTurns.delete(oldId);
  }
  if (S.liveTails.has(oldId) && !S.liveTails.has(newId)) {
    S.liveTails.set(newId, S.liveTails.get(oldId));
    S.liveTails.delete(oldId);
  }
  if (UI.wsShell?.comp?.sid === oldId) {
    UI.wsShell.comp.sid = newId;
    UI.wsShell.comp.ta.dataset.draft = dkNew;
  }
}

export function upsertSession(s) {
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
        APP.loadSessionOutput(repl, { silent: false });
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
      APP.loadSessionOutput(repl, { silent: false });
    } else {
      UI.selSession = null;
    }
  }
}

export function upsertPipeline(p) {
  if (p && p.id) S.pipelines.set(p.id, p);
  return p;
}

export function upsertHosts(list) {
  if (!Array.isArray(list)) return;
  const next = new Map();
  for (const h of list) {
    if (h && h.id) next.set(h.id, h);
  }
  S.hosts = next;
}

export function pickDefaultSession() {
  const prevSid = UI.selSession;
  if (UI.view === 'workspace' && UI.arg && S.sessions.has(UI.arg)) {
    const target = S.sessions.get(UI.arg);
    if (target && (UI.showEnded || target.state !== 'ended')) {
      if (UI.selSession !== target.session_id) {
        onLeaveSession(prevSid, target.session_id);
        UI.selSession = target.session_id;
        APP.loadSessionOutput(target, { silent: false });
      }
      return;
    }
  }
  const cur = UI.selSession ? S.sessions.get(UI.selSession) : null;
  if (cur && (UI.showEnded || cur.state !== 'ended')) return;
  const live = rootSessions();
  const first = live.find(isTmuxSession) || live[0];
  if (first) {
    onLeaveSession(prevSid, first.session_id);
    UI.selSession = first.session_id;
    APP.loadSessionOutput(first, { silent: false });
  } else {
    onLeaveSession(prevSid, null);
    UI.selSession = null;
  }
}

export function slashCommandsFor(agent) {
  const ag = AGENTS.includes(agent) ? agent : 'claude';
  if (S.slashCmds.has(ag)) return S.slashCmds.get(ag);
  return [...fallbackSlash(ag), ...starfaneSlash()];
}

export function modelsForAgent(agent) {
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
  if (def && !seen.has(def)) list.unshift({ id: def, label: L(`${def}（默认）`, `${def} (default)`), efforts: agentEffs });
  return { default: def || list[0]?.id || '', default_effort: defEff, efforts: agentEffs, models: list };
}

export function effortsForModel(mInfo, modelId) {
  if (!modelId || modelId === '__custom__') return mInfo.efforts || [];
  const found = (mInfo.models || []).find((m) => m.id === modelId);
  return found && Array.isArray(found.efforts) ? found.efforts : (mInfo.efforts || []);
}

export function hydrateSpawnAgent(ag, prefs = loadSpawnPrefs()) {
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
