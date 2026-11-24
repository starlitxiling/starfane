// starfane web console — reactive state store, host helpers, and module callback registry
import {
  AGENTS,
  COLLAPSED_KEY,
  FALLBACK_MODELS,
  FALLBACK_SLASH,
  GLYPH_MODE_KEY,
  GROUPS_INIT_KEY,
  NON_TMUX_GROUP,
  NON_TMUX_OPEN_KEY,
  OUTLINE_COLLAPSED_KEY,
  PREVIEW_LINES,
  SPAWN_PREFS_KEY,
  STAGE_LABEL,
  STARFANE_SLASH,
  STATE_LABEL,
  STATE_PRIO,
  TRANSCRIPT_LIMIT,
} from './constants.js';

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
  healthFails: 0,
  version: '',
  uptimeMs: null,
  lastSeq: 0,
  lastLoad: 0,
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
  activity: [],
  muteUntil: Number(localStorage.getItem('starfane_mute_until') || 0),
};

export const UI = {
  view: 'workspace',
  arg: null,
  modal: null,
  drawer: null,
  renameHostId: null,
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
  busy: new Set(),
  open: new Set(),
  trLimit: new Map(),
  pvLines: new Map(),
  loadingEarlierSid: null,
  loadingEarlierPane: null,
  drawerPvLines: 1000,
  termGlyphMode: loadGlyphMode(),
  slash: { open: false, start: 0, end: 0, query: '', items: [], idx: 0 },
  collapsedGroups: loadCollapsedGroups(),
  groupsInited: !!localStorage.getItem(GROUPS_INIT_KEY),
  outlineCollapsed: localStorage.getItem(OUTLINE_COLLAPSED_KEY) === '1',
  jumpLockUntil: 0,
  muteMenu: false,
  muteCustomVal: '45',
  muteCustomUnit: 'm',
  muteTimeVal: '',
  wsShell: null,
  topShell: null,
};

// Cross-module callback registry wired at boot in web/app.js (prevents circular ES module imports).
export const APP = {
  schedule: () => {},
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

export function hashStr(str) {
  let h = 2166136261;
  const s = String(str || '');
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0).toString(36);
}

export const enc = encodeURIComponent;
export const draft = (key, def = '') => (UI.drafts.has(key) ? UI.drafts.get(key) : def);
export const base = (p) => (p ? String(p).replace(/\/+$/, '').split('/').pop() || p : '—');
export const stateText = (s) => STATE_LABEL[s] || String(s ?? '').replace(/_/g, ' ');
export const stageName = (k) => STAGE_LABEL[k] || String(k ?? '').replace(/_/g, ' ');
export const short = (s, n = 8) => (s ? String(s).slice(0, n) : '—');

export function hostOf(s) {
  return (s?.host_id && String(s.host_id).trim()) || 'local';
}

export function hostDisplayName(hostId, fallbackSession = null) {
  const id = (hostId && String(hostId).trim()) || 'local';
  const rec = S.hosts.get(id);
  if (rec?.display_name) return rec.display_name;
  if (fallbackSession?.host_display_name) return fallbackSession.host_display_name;
  return id === 'local' ? 'Local' : id;
}

export function paneDisplayId(s) {
  if (!s?.pane) return '';
  const raw = String(s.pane);
  const hid = hostOf(s);
  if (hid !== 'local' && raw.startsWith(`${hid}:`)) return raw;
  return `${hid}:${raw}`;
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
  S.muteUntil = until || 0;
  try {
    localStorage.setItem('starfane_mute_until', String(S.muteUntil));
  } catch { /* ignore */ }
}

export const muted = () => S.muteUntil > Date.now();
export const connState = () => (S.healthFails >= 2 ? 'down' : S.conn);

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

export function knownProjects() {
  const set = new Set();
  for (const s of S.sessions.values()) {
    if (s.git_root) set.add(s.git_root);
    else if (s.cwd) set.add(s.cwd);
  }
  for (const p of S.pipelines.values()) if (p.project?.root) set.add(p.project.root);
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
  if (UI.view === 'workspace' && UI.arg && S.sessions.has(UI.arg)) {
    const target = S.sessions.get(UI.arg);
    if (target && (UI.showEnded || target.state !== 'ended')) {
      if (UI.selSession !== target.session_id) {
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
    UI.selSession = first.session_id;
    APP.loadSessionOutput(first, { silent: false });
  } else {
    UI.selSession = null;
  }
}

export function slashCommandsFor(agent) {
  const ag = AGENTS.includes(agent) ? agent : 'claude';
  if (S.slashCmds.has(ag)) return S.slashCmds.get(ag);
  return [...(FALLBACK_SLASH[ag] || FALLBACK_SLASH.claude), ...STARFANE_SLASH];
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
  if (def && !seen.has(def)) list.unshift({ id: def, label: `${def}（默认）`, efforts: agentEffs });
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
