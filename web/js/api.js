// starfane web console — HTTP client, SSE stream, true cursor pagination, and background polling
import {
  AGENTS,
  HEALTH_TIMEOUT_MS,
  MAX_ACTIVITY,
  MAX_BACKOFF_MS,
  PREVIEW_LINES,
  PREVIEW_PAGE_STEP,
  RECONNECT_STEPS_MS,
  REQUEST_TIMEOUT_MS,
  SSE_TYPES,
  starfaneSlash,
  TOKEN_KEY,
  TRANSCRIPT_LIMIT,
  TRANSCRIPT_PAGE_STEP,
} from './constants.js';
import { $, deliveryText, errText, toast } from './dom.js';
import { L, setLang } from './i18n.js';
import {
  APP,
  COMPACT_TRANSCRIPT_THRESHOLD,
  S,
  UI,
  applyNotifiersSnapshot,
  currentSession,
  enc,
  fmtRtt,
  hashMessage,
  hashStr,
  hashTranscriptPayload,
  hostDisplayName,
  hostOf,
  hostStatusLabel,
  migrateSessionSelection,
  pickDefaultSession,
  pvKeyOf,
  pvModeOf,
  pvReqSeq,
  reconcileOptimisticTurns,
  setLiveTailEntry,
  setPreviewEntry,
  setTranscriptEntry,
  stateText,
  trLimitOf,
  trReqSeq,
  upsertHosts,
  upsertPipeline,
  upsertSession,
} from './store.js';
import { applyTerminalFontConfig } from './terminal.js';

export class ApiError extends Error {
  constructor(code, message, status) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

export function authHeaders() {
  const t = localStorage.getItem(TOKEN_KEY);
  return t ? { 'X-Starfane-Token': t } : {};
}

export async function api(method, path, body, opts = {}) {
  const ctl = new AbortController();
  const timeoutMs = Number.isFinite(opts?.timeoutMs) && opts.timeoutMs > 0
    ? opts.timeoutMs
    : REQUEST_TIMEOUT_MS;
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  let res;
  try {
    res = await fetch(path, {
      method,
      credentials: 'same-origin',
      signal: ctl.signal,
      headers: {
        Accept: 'application/json',
        ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
        ...authHeaders(),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new ApiError('unavailable', e.name === 'AbortError' ? L('请求超时', 'Request timed out') : L(`无法连接本地 daemon（${e.message}）`, `Cannot connect to local daemon (${e.message})`), 0);
  } finally {
    clearTimeout(timer);
  }
  let env = null;
  try { env = await res.json(); } catch { env = null; }
  if (res.status === 401 || (res.status === 403 && S.authEnabled && env?.error?.code === 'unauthorized')) {
    if (S.authEnabled) { S.authNeeded = true; APP.schedule(); }
    throw new ApiError('unauthorized', env?.error?.message || L('需要认证 token', 'Authentication token required'), res.status);
  }
  if (!res.ok || !env || env.ok === false) {
    throw new ApiError(env?.error?.code || 'internal', env?.error?.message || `HTTP ${res.status}`, res.status);
  }
  return env.data !== undefined ? env.data : env;
}

export const qs = (o) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(o)) if (v !== '' && v != null) p.set(k, v);
  const s = p.toString();
  return s ? `?${s}` : '';
};

export async function act(key, fn, ok) {
  if (UI.busy.has(key)) return undefined;
  UI.busy.add(key);
  APP.schedule();
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
  } finally {
    UI.busy.delete(key);
    APP.schedule();
  }
}

export function sessionsDiffSig(map) {
  return [...map.values()]
    .map((s) => `${s.session_id}|${s.host_id || 'local'}|${s.host_display_name || ''}|${s.mux_kind || ''}|${s.state}|${s.tmux_label || ''}|${s.tmux_target || ''}|${s.pane || ''}|${s.model || ''}|${s.effort || ''}|${s.resume_id || ''}|${s.suspended_at_ms || 0}|${s.last_message || ''}|${s.last_error || ''}|${s.transcript_path || ''}|${s.last_event_ms || 0}|${s.subagent_count || 0}|${s.metrics?.context_used_pct ?? ''}|${s.metrics?.tps ?? ''}|${s.metrics?.ttft_ms ?? ''}|${s.metrics?.avg_ttft_ms ?? ''}|${s.metrics?.total_input_tokens ?? ''}|${s.metrics?.total_output_tokens ?? ''}|${s.metrics?.total_cache_read_tokens ?? ''}|${s.metrics?.recent_turns?.length || 0}|${s.metrics?.recent_turns?.at(-1)?.ts_ms || 0}`)
    .sort()
    .join('\n');
}

function hostsDiffSig(list) {
  return (list || [])
    .map((h) => `${h.id}|${h.display_name}|${h.status}|${h.session_count || 0}|${h.rtt_ms ?? ''}|${h.sync_ms ?? ''}|${h.next_retry_ms ?? ''}|${h.retry_paused ? 1 : 0}|${h.error || ''}`)
    .sort()
    .join('\n');
}

export function sessionNeedsLiveTail(s, tr) {
  if (!s) return false;
  if (['error', 'needs_permission', 'needs_input'].includes(s.state)) return true;
  if (!tr || tr.state !== 'ok' || tr.source !== 'transcript' || !tr.messages.length) return true;
  const r = tr.messages[tr.messages.length - 1]?.role;
  return r === 'user' || r === 'system';
}

let stateSyncBusy = false;
let stateSyncPendingRefresh = false;
export async function syncStateSilent({ refreshActive = false } = {}) {
  if (document.hidden || !S.loaded || S.authNeeded) return;
  if (stateSyncBusy) {
    if (refreshActive) stateSyncPendingRefresh = true;
    return;
  }
  stateSyncBusy = true;
  stateSyncPendingRefresh = false;
  try {
    const prevCur = currentSession();
    const st = await api('GET', '/api/state');
    if (checkUiHash(st?.ui_hash)) return;
    if (typeof st?.uptime_ms === 'number') S.uptimeMs = st.uptime_ms;
    const prevSig = sessionsDiffSig(S.sessions);
    const prevHostsSig = hostsDiffSig([...S.hosts.values()]);
    const prevAttn = S.attention.length;
    const prevPipe = [...S.pipelines.values()].map((p) => `${p.id}:${p.status}:${p.updated_ms || 0}`).sort().join(',');
    const nextSessions = new Map();
    for (const s of st.sessions || []) {
      if (!s || !s.session_id) continue;
      if (s.state === 'ended' && (s.discovered_only || !UI.showEnded)) continue;
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
    const nextHostsSig = hostsDiffSig(st.hosts || []);
    const nextPipelines = new Map();
    for (const p of st.pipelines || []) if (p && p.id) nextPipelines.set(p.id, p);
    const nextPipe = [...nextPipelines.values()].map((p) => `${p.id}:${p.status}:${p.updated_ms || 0}`).sort().join(',');
    const nextAttn = st.attention || [];
    const notifyChanged = applyNotifiersSnapshot(st);
    const prevTodayTok = S.todayUsage?.total_tokens ?? null;
    S.todayUsage = st.today_usage || null;
    const nextTodayTok = S.todayUsage?.total_tokens ?? null;
    const changed = prevSig !== nextSig || prevHostsSig !== nextHostsSig || prevPipe !== nextPipe || prevAttn !== nextAttn.length || notifyChanged || prevTodayTok !== nextTodayTok;
    S.lastSeq = Math.max(S.lastSeq, st.event_seq || 0);
    if (st.terminal_font || st.has_user_term_font) applyTerminalFontConfig(st.terminal_font || '', !!st.has_user_term_font);
    if (changed) {
      const stateChangedAt = Date.now();
      for (const [id, s] of nextSessions.entries()) {
        const old = S.sessions.get(id);
        if (old && old.state !== s.state) sessionStateChangedAt.set(id, stateChangedAt);
      }
      S.sessions = nextSessions;
      S.pipelines = nextPipelines;
      S.attention = nextAttn;
      if (Array.isArray(st.hosts)) upsertHosts(st.hosts);
      pickDefaultSession();
      APP.schedulePassive();
    }
    const nextCur = currentSession();
    if (
      nextCur
      && nextCur.state !== 'ended'
      && (
        refreshActive
        || !prevCur
        || prevCur.session_id !== nextCur.session_id
        || prevCur.state !== nextCur.state
        || prevCur.last_message !== nextCur.last_message
        || prevCur.last_error !== nextCur.last_error
        || prevCur.transcript_path !== nextCur.transcript_path
        || prevCur.last_event_ms !== nextCur.last_event_ms
      )
    ) {
      loadSessionOutput(nextCur, { silent: true });
    }
  } catch { /* silent fallback */ } finally {
    stateSyncBusy = false;
    if (stateSyncPendingRefresh) {
      stateSyncPendingRefresh = false;
      queueMicrotask(() => syncStateSilent({ refreshActive: true }));
    }
  }
}

export async function loadAll() {
  const wasLoaded = S.loaded;
  S.lastLoad = Date.now();
  try {
    const st = await api('GET', '/api/state');
    if (checkUiHash(st?.ui_hash)) return;
    if (setLang(st.lang)) S.slashCmds.clear();
    S.version = st.version || S.version;
    if (typeof st?.uptime_ms === 'number') S.uptimeMs = st.uptime_ms;
    S.lastSeq = st.event_seq || 0;
    S.sessions = new Map();
    S.pipelines = new Map();
    if (Array.isArray(st.hosts)) upsertHosts(st.hosts);
    applyTerminalFontConfig(st.terminal_font || '', !!st.has_user_term_font);
    (st.sessions || []).forEach(upsertSession);
    (st.pipelines || []).forEach(upsertPipeline);
    S.attention = st.attention || [];
    S.todayUsage = st.today_usage || null;
    applyNotifiersSnapshot(st);
    S.loaded = true;
    S.loadError = null;
    S.authNeeded = false;
    pickDefaultSession();
    if (wasLoaded && UI.view === 'workspace') {
      const cur = currentSession();
      if (cur && cur.state !== 'ended') {
        loadSessionOutput(cur, { silent: true, force: true });
      }
    }
    if (!Array.isArray(st.notifiers) && !Array.isArray(st.details)) {
      loadNotifiers({ silent: true });
    }
  } catch (e) {
    S.loadError = errText(e);
  }
  APP.schedule();
}

const trInFlight = new Set();
const pvInFlight = new Set();
const trPrefetchInFlight = new Map();
const pvPrefetchInFlight = new Map();
const trPendingRefetch = new Set();
const pvPendingRefetch = new Map();
const tmuxSessInFlight = new Set();
const sessionPollAt = new Map();
const sessionStateChangedAt = new Map();
const postIdleTimers = new Map();
let lastSseConnectAt = 0;
let uiReloading = false;
let uiUpdateNotifiedHash = '';
let healthInFlight = false;
let recoveryTimer = 0;
let recoveryAttempt = 0;
let restartingSince = 0;

export function reloadForUpdate() {
  if (uiReloading) return;
  if (typeof location === 'undefined' || typeof location.reload !== 'function') return;
  uiReloading = true;
  if (S.uiUpdateHash) S.uiHash = S.uiUpdateHash;
  S.uiUpdateAvailable = false;
  location.reload();
}

function checkUiHash(nextHash) {
  if (typeof nextHash !== 'string' || !nextHash) return false;
  if (!S.uiHash) {
    S.uiHash = nextHash;
    return false;
  }
  if (S.uiHash === nextHash) {
    if (S.uiUpdateAvailable) {
      S.uiUpdateAvailable = false;
      S.uiUpdateHash = '';
      APP.schedule();
    }
    return false;
  }
  S.uiUpdateHash = nextHash;
  const hasDraft = Boolean(UI.wsShell?.comp?.ta?.value?.trim())
    || ((UI.attachments.get(UI.selSession) || []).length > 0);
  const hasOverlay = Boolean(UI.modal || UI.drawer);
  if (!hasDraft && !hasOverlay && S.loaded && typeof location !== 'undefined' && typeof location.reload === 'function') {
    reloadForUpdate();
    return true;
  }
  S.uiUpdateAvailable = true;
  APP.schedule();
  if (uiUpdateNotifiedHash !== nextHash) {
    uiUpdateNotifiedHash = nextHash;
    toast(L('发现新版本，随时可点击刷新', 'New version ready — tap to reload'), 'info', {
      actionLabel: L('立即刷新', 'Reload'),
      onAction: () => reloadForUpdate(),
      durationMs: 12000,
    });
  }
  return false;
}

function stopFastRecovery() {
  if (recoveryTimer) {
    clearTimeout(recoveryTimer);
    recoveryTimer = 0;
  }
  recoveryAttempt = 0;
  restartingSince = 0;
}

async function fastRecoveryTick() {
  recoveryTimer = 0;
  if (document.hidden && S.loaded) return;
  if (S.conn === 'live' && !S.restarting && S.healthFails === 0) {
    stopFastRecovery();
    return;
  }
  await pollHealth({ fromRecovery: true });
  if (uiReloading) return;
  if (S.conn !== 'live' || S.restarting || S.healthFails > 0) {
    if (S.restarting && restartingSince && (Date.now() - restartingSince) > 15000) {
      S.restarting = false;
      APP.schedule();
    }
    recoveryAttempt += 1;
    const wait = RECONNECT_STEPS_MS[Math.min(recoveryAttempt, RECONNECT_STEPS_MS.length - 1)] || 6000;
    recoveryTimer = setTimeout(fastRecoveryTick, wait);
  } else {
    stopFastRecovery();
  }
}

function startFastRecovery({ restarting = false } = {}) {
  if (restarting) {
    S.restarting = true;
    restartingSince = restartingSince || Date.now();
    recoveryAttempt = 0;
    if (recoveryTimer) {
      clearTimeout(recoveryTimer);
      recoveryTimer = 0;
    }
  } else if (recoveryTimer) {
    return;
  }
  const wait = restarting
    ? 150
    : (RECONNECT_STEPS_MS[Math.min(recoveryAttempt, RECONNECT_STEPS_MS.length - 1)] || 6000);
  recoveryTimer = setTimeout(fastRecoveryTick, wait);
}

const PREFETCH_FRESH_MS = 15000;
const SWITCH_FRESH_MS = 2500;
const PREFETCH_COALESCE_MS = 5000;

function isHostOffline(hostId) {
  if (!hostId || hostId === 'local') return false;
  return S.hosts.get(hostId)?.status === 'offline';
}

export function isTmuxSessionsLoading(hostId = 'local') {
  const hid = (hostId && String(hostId).trim()) || 'local';
  return tmuxSessInFlight.has(hid);
}

export function isSessionOutputFresh(s, maxAgeMs = PREFETCH_FRESH_MS) {
  if (!s || !s.session_id) return true;
  const now = Date.now();
  if (s.agent === 'shell' || UI.outputMode === 'raw') {
    if (!s.pane) return true;
    const pv = S.previews.get(pvKeyOf(s.pane));
    return Boolean(pv?.state === 'ok' && !pv.staleErr && pv.updatedMs && (now - pv.updatedMs) < maxAgeMs);
  }
  const tr = S.transcripts.get(s.session_id);
  if (tr?.state === 'ok' && !tr.staleErr && tr.updatedMs && (now - tr.updatedMs) < maxAgeMs) {
    return true;
  }
  if (tr?.unsupported && s.pane) {
    const pv = S.previews.get(pvKeyOf(s.pane));
    return Boolean(pv?.state === 'ok' && !pv.staleErr && pv.updatedMs && (now - pv.updatedMs) < maxAgeMs);
  }
  return false;
}

export function prefetchSessionOutput(s) {
  if (!s || !s.session_id || s.state === 'ended') return false;
  if (s.session_id === currentSession()?.session_id) return false;
  if (isHostOffline(hostOf(s))) return false;
  if (isSessionOutputFresh(s, PREFETCH_FRESH_MS)) return false;
  loadSessionOutput(s, { silent: true, prefetch: true });
  return true;
}

export function loadSessionOutput(s, { silent = false, forcePreview = false, prefetch = false, force = false } = {}) {
  if (!s) return;
  if ((silent || prefetch) && isHostOffline(hostOf(s))) return;
  if (s.agent !== 'shell') loadTranscript(s.session_id, { silent, prefetch, force });
  if (!s.pane) return;
  const pvMode = pvModeOf(s.pane);
  if (prefetch && !forcePreview && s.agent !== 'shell' && UI.outputMode !== 'raw') {
    const tr = S.transcripts.get(s.session_id);
    const needsAlertTail = ['error', 'needs_permission', 'needs_input'].includes(s.state);
    if (!needsAlertTail && !tr?.unsupported) return;
  }
  if (silent && !forcePreview && s.agent !== 'shell' && UI.outputMode !== 'raw') {
    const tr = S.transcripts.get(s.session_id);
    const tailOpen = UI.open.has(`tail:${s.session_id}`) || Boolean($('details.live-tail')?.open);
    if (!sessionNeedsLiveTail(s, tr) && !tailOpen) return;
  }
  // Never clobber full-history or upward-paginated terminal views during background silent polls.
  const existingPv = S.previews.get(pvKeyOf(s.pane, pvMode));
  if ((silent || prefetch) && !forcePreview && (pvMode === 'all' || existingPv?.paginated)) return;
  loadPreview(s.pane, { mode: pvMode, silent, prefetch, force });
}

export async function loadTranscript(sessionId, { silent = false, limit, all = false, prefetch = false, force = false } = {}) {
  if (!sessionId) return;
  if ((silent || prefetch) && !force && trInFlight.has(sessionId)) {
    if (!prefetch) trPendingRefetch.add(sessionId);
    return;
  }
  const prev = S.transcripts.get(sessionId);
  if (prev?.unsupported) return;
  if (!force && !all && !limit && !prefetch) {
    const pfAt = trPrefetchInFlight.get(sessionId);
    if (pfAt && trInFlight.has(sessionId) && (Date.now() - pfAt) < PREFETCH_COALESCE_MS) {
      if (!prev) {
        setTranscriptEntry(sessionId, {
          state: 'loading',
          source: 'none',
          messages: [],
          hasMore: false,
          totalMessages: 0,
          startSeq: 0,
          nextBefore: null,
          explicitAll: false,
          loadedAll: false,
          autoPollPaused: false,
          latestTotalMessages: null,
          limit: TRANSCRIPT_LIMIT,
          hash: '',
          staleErr: null,
          updatedMs: 0,
        });
        if (!silent) APP.schedule();
      }
      return;
    }
    if (!silent && S.conn === 'live' && prev?._fromPrefetch && prev?.state === 'ok' && !prev.staleErr && !prev.autoPollPaused && prev.updatedMs && (Date.now() - prev.updatedMs) < SWITCH_FRESH_MS) {
      prev._fromPrefetch = false;
      APP.updateOutputClock(sessionId);
      APP.rerender();
      return;
    }
    if (!silent && prev?.state === 'ok' && UI.view === 'workspace' && UI.selSession === sessionId) {
      APP.rerender();
    }
  }
  // Never re-fetch `all=1` megabytes on background silent polling!
  // When user already loaded expanded/full history, silent polls fetch only a lightweight tail
  // (`limit=60`) and splice incremental updates onto the loaded prefix without rebuilding history.
  const prevExpanded = prev?.state === 'ok'
    && prev.source === 'transcript'
    && (prev.explicitAll || (prev.messages?.length || 0) > TRANSCRIPT_LIMIT);
  if (silent && !all && prevExpanded && prev.autoPollPaused) return;
  const incrementalTail = !all && !limit && prevExpanded && !prev.autoPollPaused;
  const effectiveAll = Boolean(all) || (!silent && !limit && prevExpanded && prev.autoPollPaused);
  const lim = effectiveAll
    ? undefined
    : (incrementalTail
      ? TRANSCRIPT_LIMIT
      : (limit || Math.min(Math.max(trLimitOf(sessionId), TRANSCRIPT_LIMIT), COMPACT_TRANSCRIPT_THRESHOLD)));
  const seq = (trReqSeq.get(sessionId) || 0) + 1;
  trReqSeq.set(sessionId, seq);
  trInFlight.add(sessionId);
  trPendingRefetch.delete(sessionId);
  if (prefetch) trPrefetchInFlight.set(sessionId, Date.now());
  else trPrefetchInFlight.delete(sessionId);
  if (!prev && !prefetch) {
    setTranscriptEntry(sessionId, {
      state: 'loading',
      source: 'none',
      messages: [],
      hasMore: false,
      totalMessages: 0,
      startSeq: 0,
      nextBefore: null,
      explicitAll: effectiveAll,
      loadedAll: effectiveAll,
      autoPollPaused: false,
      latestTotalMessages: null,
      limit: lim || TRANSCRIPT_LIMIT,
      hash: '',
      staleErr: null,
      updatedMs: 0,
    });
    if (!silent) APP.schedule();
  }
  try {
    const q = effectiveAll ? { all: 1 } : { limit: lim };
    const data = await api('GET', `/api/sessions/${enc(sessionId)}/transcript${qs(q)}`);
    if (trReqSeq.get(sessionId) !== seq) return;
    const latestPrev = S.transcripts.get(sessionId) || prev;
    const msgs = Array.isArray(data?.messages) ? data.messages : [];
    const source = data?.source === 'transcript' && msgs.length > 0 ? 'transcript' : 'none';
    const hasMore = Boolean(data?.has_more);
    const totalMessages = Number.isFinite(data?.total_messages) ? data.total_messages : msgs.length;
    const startSeq = Number.isFinite(data?.start_seq) ? data.start_seq : Math.max(0, totalMessages - msgs.length);
    const nextBefore = Number.isFinite(data?.next_before) ? data.next_before : (hasMore ? startSeq : null);
    const now = Date.now();
    const liveTail = typeof data?.live_tail === 'string' ? data.live_tail : latestPrev?.live_tail;
    const liveTailUpdatedMs = typeof data?.live_tail === 'string' ? now : (latestPrev?.liveTailUpdatedMs || 0);
    if (typeof data?.live_tail === 'string') setLiveTailEntry(sessionId, data.live_tail, now);
    const isActive = currentSession()?.session_id === sessionId;

    if (data?.metrics && S.sessions.has(sessionId)) {
      const cur = S.sessions.get(sessionId);
      if (JSON.stringify(cur.metrics || null) !== JSON.stringify(data.metrics)) {
        S.sessions.set(sessionId, { ...cur, metrics: data.metrics });
        APP.patchTelemetryStrip(sessionId);
      }
    }

    if (incrementalTail && prev && source === 'transcript') {
      const prevStart = Number.isFinite(prev.startSeq) ? prev.startSeq : 0;
      const prevEnd = prevStart + prev.messages.length;
      if (startSeq >= prevStart && startSeq <= prevEnd) {
        const prefixLen = startSeq - prevStart;
        let same = prev.totalMessages === totalMessages
          && prev.messages.length === prefixLen + msgs.length;
        if (same) {
          for (let i = 0; i < msgs.length; i++) {
            if (hashMessage(prev.messages[prefixLen + i]) !== hashMessage(msgs[i])) {
              same = false;
              break;
            }
          }
        }
        if (same) {
          const hadStale = Boolean(prev.staleErr);
          const tailChanged = typeof data?.live_tail === 'string' && data.live_tail !== prev.live_tail;
          prev.updatedMs = now;
          prev.staleErr = null;
          if (typeof data?.live_tail === 'string') {
            prev.live_tail = liveTail;
            prev.liveTailUpdatedMs = liveTailUpdatedMs;
          }
          const optChanged = reconcileOptimisticTurns(sessionId, prev);
          if (hadStale || optChanged || tailChanged) {
            if (!prefetch || isActive) APP.notifyOutputUpdated(sessionId);
          } else if (isActive) {
            APP.updateOutputClock(sessionId);
          }
          if (!silent && !prefetch) APP.schedule();
          return;
        }
        const merged = [...prev.messages.slice(0, prefixLen), ...msgs];
        const mergedHasMore = prevStart > 0;
        const mergedTotal = Math.max(totalMessages, prevStart + merged.length);
        const nextHash = hashTranscriptPayload(source, prevStart, mergedTotal, mergedHasMore, merged);
        const nextEntry = {
          ...prev,
          state: 'ok',
          source,
          transcriptPath: data?.transcript_path || prev.transcriptPath || null,
          messages: merged,
          hasMore: mergedHasMore,
          totalMessages: mergedTotal,
          startSeq: prevStart,
          nextBefore: mergedHasMore ? prevStart : null,
          explicitAll: Boolean(prev.explicitAll),
          loadedAll: !mergedHasMore,
          autoPollPaused: false,
          latestTotalMessages: null,
          limit: merged.length,
          hash: nextHash,
          live_tail: liveTail,
          liveTailUpdatedMs,
          updatedMs: now,
          err: null,
          staleErr: null,
        };
        setTranscriptEntry(sessionId, nextEntry);
        reconcileOptimisticTurns(sessionId, nextEntry);
        if (!prefetch || isActive) APP.notifyOutputUpdated(sessionId);
        return;
      }
      if (silent) {
        // Larger gap than one 60-message tail while reviewing expanded/full history:
        // pause silent auto-refresh instead of silently downloading `all=1` or clobbering scrollback.
        setTranscriptEntry(sessionId, {
          ...prev,
          autoPollPaused: true,
          latestTotalMessages: totalMessages,
          staleErr: null,
          updatedMs: now,
        });
        if (!prefetch || isActive) APP.notifyOutputUpdated(sessionId);
        return;
      }
    }

    const nextHash = hashTranscriptPayload(source, startSeq, totalMessages, hasMore, msgs);
    const changed = !latestPrev || latestPrev.state !== 'ok' || latestPrev.hash !== nextHash || Boolean(latestPrev.autoPollPaused) || Boolean(latestPrev.staleErr);
    const nextEntry = {
      state: 'ok',
      source,
      transcriptPath: data?.transcript_path || null,
      messages: msgs,
      hasMore,
      totalMessages,
      startSeq,
      nextBefore,
      explicitAll: effectiveAll,
      loadedAll: !hasMore,
      autoPollPaused: false,
      latestTotalMessages: null,
      limit: lim || msgs.length,
      hash: nextHash,
      live_tail: liveTail,
      liveTailUpdatedMs,
      _fromPrefetch: Boolean(prefetch),
      updatedMs: now,
      err: null,
      staleErr: null,
    };
    setTranscriptEntry(sessionId, nextEntry);
    const optChanged = reconcileOptimisticTurns(sessionId, nextEntry);
    if (changed || optChanged) {
      if (!prefetch || isActive) APP.notifyOutputUpdated(sessionId);
    } else {
      if (UI.loadingEarlierSid === sessionId) UI.loadingEarlierSid = null;
      if (isActive) APP.updateOutputClock(sessionId);
      if (!silent && !prefetch) APP.schedule();
    }
  } catch (e) {
    if (trReqSeq.get(sessionId) !== seq) return;
    if (UI.loadingEarlierSid === sessionId) UI.loadingEarlierSid = null;
    const isActive = currentSession()?.session_id === sessionId;
    if (prefetch && !isActive) return;
    if (e instanceof ApiError && e.status === 405) {
      setTranscriptEntry(sessionId, {
        state: 'ok', source: 'none', messages: [], hasMore: false, totalMessages: 0, startSeq: 0, nextBefore: null,
        explicitAll: false, loadedAll: true, autoPollPaused: false, latestTotalMessages: null,
        limit: TRANSCRIPT_LIMIT, hash: 'none', staleErr: null, updatedMs: Date.now(), unsupported: true,
      });
      if (!prev || prev.state === 'loading') APP.notifyOutputUpdated(sessionId);
      return;
    }
    const errMsg = errText(e);
    if (silent && prev?.state === 'ok' && ((prev.messages?.length || 0) > 0 || prev.source === 'transcript')) {
      const changed = prev.staleErr !== errMsg;
      setTranscriptEntry(sessionId, {
        ...prev,
        staleErr: errMsg,
        updatedMs: Date.now(),
      });
      if (changed) APP.notifyOutputUpdated(sessionId);
      return;
    }
    const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
    setTranscriptEntry(sessionId, {
      ...(prev || { source: 'none', messages: [], hasMore: false, totalMessages: 0, startSeq: 0, nextBefore: null, explicitAll: false, autoPollPaused: false, latestTotalMessages: null, limit: TRANSCRIPT_LIMIT, hash: '' }),
      state: 'err',
      err: errMsg,
      staleErr: null,
      updatedMs: Date.now(),
    });
    if (changed) APP.notifyOutputUpdated(sessionId);
  } finally {
    if (trReqSeq.get(sessionId) === seq) {
      trInFlight.delete(sessionId);
      trPrefetchInFlight.delete(sessionId);
      if (trPendingRefetch.delete(sessionId)) {
        queueMicrotask(() => {
          const latest = S.sessions.get(sessionId);
          if (latest && latest.state !== 'ended' && !isHostOffline(hostOf(latest))) {
            loadTranscript(sessionId, { silent: true });
          }
        });
      }
    }
  }
}

/**
 * True cursor pagination: fetch up to `step` messages strictly older than `tr.nextBefore`
 * and prepend them cleanly without re-fetching or losing scroll position.
 */
export async function loadEarlierTranscript(sessionId, step = TRANSCRIPT_PAGE_STEP) {
  const prev = S.transcripts.get(sessionId);
  if (!prev || prev.state !== 'ok' || !prev.hasMore) return;
  const cursor = prev.nextBefore ?? prev.startSeq;
  if (!cursor || cursor <= 0) return;
  UI.loadingEarlierSid = sessionId;
  return act(`trmore:${sessionId}`, async () => {
    const data = await api('GET', `/api/sessions/${enc(sessionId)}/transcript${qs({ before: cursor, limit: step })}`);
    const older = Array.isArray(data?.messages) ? data.messages : [];
    const merged = [...older, ...prev.messages];
    const hasMore = Boolean(data?.has_more);
    const totalMessages = Number.isFinite(data?.total_messages) ? Math.max(prev.totalMessages, data.total_messages) : merged.length;
    const startSeq = Number.isFinite(data?.start_seq) ? data.start_seq : Math.max(0, cursor - older.length);
    const nextBefore = Number.isFinite(data?.next_before) ? data.next_before : (hasMore ? startSeq : null);
    const nextHash = hashTranscriptPayload(prev.source, startSeq, totalMessages, hasMore, merged);
    UI.trLimit.set(sessionId, merged.length);
    setTranscriptEntry(sessionId, {
      ...prev,
      messages: merged,
      hasMore,
      totalMessages,
      startSeq,
      nextBefore,
      explicitAll: prev.explicitAll || !hasMore,
      loadedAll: !hasMore,
      autoPollPaused: false,
      latestTotalMessages: null,
      limit: merged.length,
      hash: nextHash,
      staleErr: null,
      updatedMs: Date.now(),
    });
    APP.notifyOutputUpdated(sessionId);
  });
}

export async function loadAllTranscript(sessionId) {
  UI.loadingEarlierSid = sessionId;
  return act(`trall:${sessionId}`, () => loadTranscript(sessionId, { silent: false, all: true }));
}

export async function loadPreview(paneId, { lines, mode, silent = false, prefetch = false, force = false } = {}) {
  if (!paneId) return;
  const effMode = mode !== undefined ? mode : (lines !== undefined ? lines : pvModeOf(paneId));
  const isAll = effMode === 'all';
  const key = pvKeyOf(paneId, effMode);
  if ((silent || prefetch) && !force && pvInFlight.has(key)) {
    if (!prefetch) pvPendingRefetch.set(key, { paneId, mode: effMode });
    return;
  }
  const prev = S.previews.get(key);
  if (!force && !prefetch && !isAll) {
    const pfAt = pvPrefetchInFlight.get(key);
    if (pfAt && pvInFlight.has(key) && (Date.now() - pfAt) < PREFETCH_COALESCE_MS) {
      if (!prev) {
        setPreviewEntry(key, {
          state: 'loading',
          text: '',
          hash: '',
          totalLines: 0,
          startLine: 0,
          hasMore: false,
          nextBeforeLine: null,
          truncated: false,
          paginated: false,
          staleErr: null,
          updatedMs: 0,
        });
        if (!silent) APP.schedule();
      }
      return;
    }
    if (!silent && S.conn === 'live' && prev?._fromPrefetch && prev?.state === 'ok' && !prev.staleErr && !prev.paginated && prev.updatedMs && (Date.now() - prev.updatedMs) < SWITCH_FRESH_MS) {
      prev._fromPrefetch = false;
      const cur = currentSession();
      if (effMode === pvModeOf(paneId) && cur?.pane === paneId) APP.updateOutputClock(cur.session_id);
      APP.rerender();
      return;
    }
    if (!silent && prev?.state === 'ok' && UI.view === 'workspace' && currentSession()?.pane === paneId) {
      APP.rerender();
    }
  }
  const seq = (pvReqSeq.get(key) || 0) + 1;
  pvReqSeq.set(key, seq);
  pvInFlight.add(key);
  pvPendingRefetch.delete(key);
  if (prefetch) pvPrefetchInFlight.set(key, Date.now());
  else pvPrefetchInFlight.delete(key);
  if (!prev && !prefetch) {
    setPreviewEntry(key, {
      state: 'loading',
      text: '',
      hash: '',
      totalLines: 0,
      startLine: 0,
      hasMore: false,
      nextBeforeLine: null,
      truncated: false,
      paginated: false,
      staleErr: null,
      updatedMs: 0,
    });
    if (!silent) APP.schedule();
  }
  try {
    const query = isAll ? { all: 1 } : { lines: Number(effMode) || PREVIEW_LINES };
    const data = await api('GET', `/api/panes/${enc(paneId)}/preview${qs(query)}`);
    if (pvReqSeq.get(key) !== seq) return;
    const latestPrev = S.previews.get(key) || prev;
    const text = data?.text ?? '';
    const totalLines = Number.isFinite(data?.total_lines) ? data.total_lines : (text ? text.split('\n').length : 0);
    const startLine = Number.isFinite(data?.start_line) ? data.start_line : 0;
    const hasMore = Boolean(data?.has_more);
    const nextBeforeLine = Number.isFinite(data?.next_before_line) ? data.next_before_line : (hasMore ? startLine : null);
    const truncated = Boolean(data?.truncated);
    const nextHash = hashStr(`${text}|${startLine}|${totalLines}|${hasMore ? 1 : 0}|${!!data?.in_mode}|${!!data?.dead}`);
    const changed = !latestPrev || latestPrev.state !== 'ok' || latestPrev.hash !== nextHash || Boolean(latestPrev.staleErr);
    setPreviewEntry(key, {
      state: 'ok',
      updatedMs: Date.now(),
      _fromPrefetch: Boolean(prefetch),
      data,
      text,
      totalLines,
      startLine,
      hasMore,
      nextBeforeLine,
      truncated,
      paginated: false,
      hash: nextHash,
      err: null,
      staleErr: null,
    });
    if (effMode === pvModeOf(paneId)) {
      const cur = currentSession();
      if (cur?.pane === paneId) {
        if (changed) APP.notifyOutputUpdated(cur.session_id);
        else APP.updateOutputClock(cur.session_id);
      } else if (changed && !prefetch) {
        APP.schedulePassive();
      }
    } else if (changed && !prefetch) {
      APP.schedulePassive();
    }
  } catch (e) {
    if (pvReqSeq.get(key) !== seq) return;
    const cur = currentSession();
    const isActivePane = effMode === pvModeOf(paneId) && cur?.pane === paneId;
    if (prefetch && !isActivePane) return;
    const errMsg = errText(e);
    if (silent && prev?.state === 'ok' && typeof prev.text === 'string' && prev.text.length > 0) {
      const changed = prev.staleErr !== errMsg;
      setPreviewEntry(key, {
        ...prev,
        staleErr: errMsg,
        updatedMs: Date.now(),
      });
      if (changed) {
        if (isActivePane) APP.notifyOutputUpdated(cur.session_id);
        else APP.schedulePassive();
      }
      return;
    }
    const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
    setPreviewEntry(key, {
      ...(prev || { text: '', hash: '', totalLines: 0, startLine: 0, hasMore: false, nextBeforeLine: null }),
      state: 'err',
      err: errMsg,
      staleErr: null,
      status: e?.status || 0,
      updatedMs: Date.now(),
    });
    if (changed) {
      if (isActivePane) APP.notifyOutputUpdated(cur.session_id);
      else APP.schedulePassive();
    }
  } finally {
    if (pvReqSeq.get(key) === seq) {
      pvInFlight.delete(key);
      pvPrefetchInFlight.delete(key);
      const pending = pvPendingRefetch.get(key);
      if (pending) {
        pvPendingRefetch.delete(key);
        queueMicrotask(() => {
          const cur = currentSession();
          const owner = (cur?.pane === pending.paneId ? cur : null)
            || [...S.sessions.values()].find((x) => x.pane === pending.paneId && x.state !== 'ended');
          if (owner && isHostOffline(hostOf(owner))) return;
          const activeNeeds = Boolean(cur?.pane === pending.paneId && cur.state !== 'ended' && pvModeOf(cur.pane) === pending.mode);
          const drawerNeeds = Boolean(UI.drawer?.kind === 'terminal' && UI.drawer.pane === pending.paneId && UI.drawerPvLines === pending.mode);
          const inboxNeeds = Boolean(UI.view === 'inbox' && owner && ['needs_permission', 'needs_input'].includes(owner.state) && pvModeOf(owner.pane) === pending.mode);
          if (activeNeeds || drawerNeeds || inboxNeeds) {
            loadPreview(pending.paneId, { mode: pending.mode, silent: true });
          }
        });
      }
    }
  }
}

/**
 * Upward scrollback pagination for terminal view: fetches `step` lines strictly before `nextBeforeLine`
 * and prepends cleanly without resetting scroll position.
 */
export async function loadEarlierPreview(paneId, step = PREVIEW_PAGE_STEP) {
  const mode = pvModeOf(paneId);
  const key = pvKeyOf(paneId, mode);
  const prev = S.previews.get(key);
  if (!prev || prev.state !== 'ok' || !prev.hasMore) return;
  const cursor = prev.nextBeforeLine ?? prev.startLine;
  if (!cursor || cursor <= 0) return;
  UI.loadingEarlierPane = paneId;
  return act(`pvmore:${paneId}`, async () => {
    const data = await api('GET', `/api/panes/${enc(paneId)}/preview${qs({ before_line: cursor, lines: step })}`);
    const olderText = data?.text ?? '';
    const mergedText = olderText && prev.text ? `${olderText}\n${prev.text}` : (olderText || prev.text);
    const startLine = Number.isFinite(data?.start_line) ? data.start_line : 0;
    const hasMore = Boolean(data?.has_more);
    const nextBeforeLine = Number.isFinite(data?.next_before_line) ? data.next_before_line : (hasMore ? startLine : null);
    const totalLines = Number.isFinite(data?.total_lines) ? Math.max(prev.totalLines, data.total_lines) : mergedText.split('\n').length;
    const nextHash = hashStr(`${mergedText}|${startLine}|${totalLines}|${hasMore ? 1 : 0}`);
    setPreviewEntry(key, {
      ...prev,
      text: mergedText,
      startLine,
      totalLines,
      hasMore,
      nextBeforeLine,
      truncated: prev.truncated || Boolean(data?.truncated),
      paginated: true,
      hash: nextHash,
      staleErr: null,
      updatedMs: Date.now(),
    });
    const cur = currentSession();
    if (cur?.pane === paneId) APP.notifyOutputUpdated(cur.session_id);
    else APP.schedule();
  });
}

export async function probeHostApi(hostId) {
  const hid = String(hostId || '').trim();
  if (!hid || hid === 'local') return undefined;
  const busyKey = `probe-host:${hid}`;
  if (UI.busy.has(busyKey)) return undefined;
  UI.busy.add(busyKey);
  APP.schedule();
  try {
    const r = await api('POST', `/api/hosts/${enc(hid)}/probe`, {});
    const prev = S.hosts.get(hid);
    const nextStatus = r?.status || prev?.status || 'online';
    const nextRtt = typeof r?.rtt_ms === 'number' && Number.isFinite(r.rtt_ms) ? r.rtt_ms : prev?.rtt_ms;
    const nextSync = typeof r?.sync_ms === 'number' && Number.isFinite(r.sync_ms) ? r.sync_ms : prev?.sync_ms;
    if (prev) {
      S.hosts.set(hid, {
        ...prev,
        status: nextStatus,
        rtt_ms: nextRtt,
        sync_ms: nextSync,
        next_retry_ms: nextStatus === 'online' ? null : (r?.next_retry_ms ?? prev.next_retry_ms),
        retry_paused: Boolean(r?.retry_paused),
        error: nextStatus === 'online' ? null : (r?.error ?? prev.error),
      });
    }
    const hName = hostDisplayName(hid);
    if (nextStatus === 'online') {
      const rttPart = typeof nextRtt === 'number' && Number.isFinite(nextRtt) ? ` (${fmtRtt(nextRtt)})` : '';
      toast(L(`主机 ${hName} 已重连${rttPart}`, `Host ${hName} reconnected${rttPart}`), 'ok');
      syncStateSilent();
      const cur = currentSession();
      if (cur && hostOf(cur) === hid && cur.state !== 'ended') {
        loadSessionOutput(cur, { silent: false });
      }
    } else {
      toast(
        L(`主机 ${hName} 仍为${hostStatusLabel(nextStatus)}状态`, `Host ${hName} is still ${hostStatusLabel(nextStatus)}`),
        'warn',
      );
    }
    return r;
  } catch (e) {
    if (e instanceof ApiError && e.status === 403) {
      toast(L('当前控制台处于只读模式，无法触发主机重连', 'Console is read-only — cannot trigger host reconnect'), 'warn');
    } else {
      toast(L(`主机重连失败：${errText(e)}`, `Host reconnect failed: ${errText(e)}`), 'err');
    }
    return undefined;
  } finally {
    UI.busy.delete(busyKey);
    APP.schedule();
  }
}

export async function pauseHostRetryApi(hostId) {
  const hid = String(hostId || '').trim();
  if (!hid || hid === 'local') return undefined;
  const busyKey = `pause-host:${hid}`;
  if (UI.busy.has(busyKey)) return undefined;
  UI.busy.add(busyKey);
  APP.schedule();
  try {
    const rec = await api('POST', `/api/hosts/${enc(hid)}/pause`, {});
    const prev = S.hosts.get(hid);
    if (rec?.id) {
      S.hosts.set(hid, rec);
    } else if (prev) {
      S.hosts.set(hid, { ...prev, retry_paused: true, next_retry_ms: null });
    }
    const hName = hostDisplayName(hid);
    toast(L(`已停止主机 ${hName} 自动重试`, `Stopped auto-retry for host ${hName}`), 'ok');
    return rec;
  } catch (e) {
    if (e instanceof ApiError && e.status === 403) {
      toast(L('当前控制台处于只读模式，无法停止主机重试', 'Console is read-only — cannot pause host retries'), 'warn');
    } else {
      toast(L(`停止重试失败：${errText(e)}`, `Failed to pause retries: ${errText(e)}`), 'err');
    }
    return undefined;
  } finally {
    UI.busy.delete(busyKey);
    APP.schedule();
  }
}

export async function loadUsage(days = S.usageDays, { silent = false } = {}) {
  const wantDays = Math.max(1, Math.min(90, Number(days) || 14));
  S.usageDays = wantDays;
  if (!silent || S.usageState === 'idle') {
    S.usageState = 'loading';
    S.usageErr = null;
    APP.schedulePassive();
  }
  try {
    const res = await api('GET', `/api/usage?days=${wantDays}`);
    S.usage = res || null;
    if (res?.today) S.todayUsage = res.today;
    S.usageState = 'ok';
    S.usageErr = null;
    if (res?.daily?.length && !res.daily.some((d) => d.date === S.usageSelDate)) {
      S.usageSelDate = res.today_date || res.daily[res.daily.length - 1].date;
    }
    return res;
  } catch (e) {
    S.usageState = 'err';
    S.usageErr = errText(e);
    return undefined;
  } finally {
    APP.schedulePassive();
  }
}

export async function suspendSessionApi(sessionId, { allowFresh = false } = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return undefined;
  const busyKey = `suspend:${sid}`;
  if (UI.busy.has(busyKey)) return undefined;
  UI.busy.add(busyKey);
  APP.schedule();
  try {
    const r = await api('POST', `/api/sessions/${enc(sid)}/suspend`, { allow_fresh: Boolean(allowFresh) });
    if (r?.session) upsertSession(r.session);
    if (UI.modal === 'suspend-fresh' && UI.suspendFreshSessionId === sid) {
      APP.closeModal();
    }
    toast(
      r?.resume_mode === 'fresh'
        ? L('会话进程已休眠释放，终端窗口已保留（唤醒将重开空白会话）', 'Session suspended to save resources; terminal kept open (wake will start a fresh session)')
        : L('会话进程已休眠释放，终端窗口已保留（唤醒将恢复原会话）', 'Session suspended to save resources; terminal kept open (wake will restore original session)'),
      'ok',
    );
    return r;
  } catch (e) {
    if (!allowFresh && e instanceof ApiError && e.status === 400 && /no_resumable_id|no resumable conversation id/i.test(e.message || '')) {
      UI.suspendFreshSessionId = sid;
      UI.modal = 'suspend-fresh';
      APP.rerender();
      return undefined;
    }
    if (e instanceof ApiError && e.status === 403) {
      toast(L('当前控制台处于只读模式，无法休眠会话', 'Console is read-only — cannot suspend sessions'), 'warn');
    } else {
      toast(L(`休眠会话失败：${errText(e)}`, `Failed to suspend session: ${errText(e)}`), 'err');
    }
    return undefined;
  } finally {
    UI.busy.delete(busyKey);
    APP.schedule();
  }
}

export async function resumeSessionApi(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return undefined;
  const busyKey = `resume:${sid}`;
  if (UI.busy.has(busyKey)) return undefined;
  UI.busy.add(busyKey);
  APP.schedule();
  try {
    const r = await api('POST', `/api/sessions/${enc(sid)}/resume`, {});
    if (r?.session) {
      upsertSession(r.session);
      loadSessionOutput(r.session, { silent: true, force: true });
    }
    toast(L('已唤醒原会话，正在恢复对话上下文…', 'Session woken — restoring original conversation…'), 'ok');
    return r;
  } catch (e) {
    if (e instanceof ApiError && e.status === 403) {
      toast(L('当前控制台处于只读模式，无法唤醒会话', 'Console is read-only — cannot wake session'), 'warn');
    } else {
      toast(L(`唤醒会话失败：${errText(e)}`, `Failed to wake session: ${errText(e)}`), 'err');
    }
    return undefined;
  } finally {
    UI.busy.delete(busyKey);
    APP.schedule();
  }
}

export async function renameHostApi(hostId, displayName) {
  return act(`rename-host:${hostId}`, async () => {
    const rec = await api('POST', `/api/hosts/${enc(hostId)}/rename`, { display_name: displayName });
    if (rec?.id) S.hosts.set(rec.id, rec);
    for (const [sid, s] of S.sessions.entries()) {
      if ((s.host_id || 'local') === hostId) {
        S.sessions.set(sid, { ...s, host_display_name: rec.display_name || displayName });
      }
    }
    APP.closeModal();
    APP.schedule();
    return rec;
  }, (r) => L(`主机名称已更新为「${r?.display_name || displayName}」`, `Host renamed to "${r?.display_name || displayName}"`));
}

function keySentToastLabel(key, keys = [], text = null) {
  if (text) return L('已提交自定义回答', 'Submitted custom answer');
  if (Array.isArray(keys) && keys.length > 1) {
    const last = keys[keys.length - 1];
    if (last === 'Space') return L('已切换选项勾选状态', 'Toggled option selection');
    if (last === 'Enter') return L('已选中并提交选项', 'Selected and submitted option');
  }
  return {
    Up: L('已发送 ↑ 上移按键', 'Sent ↑ Up key'),
    Down: L('已发送 ↓ 下移按键', 'Sent ↓ Down key'),
    Left: L('已发送 ← 上一题', 'Sent ← Previous'),
    Right: L('已发送 → 下一题', 'Sent → Next'),
    Tab: L('已发送 Tab 切换', 'Sent Tab key'),
    Space: L('已发送 Space 勾选', 'Sent Space toggle'),
    Backspace: L('已发送 Backspace 退格', 'Sent Backspace key'),
    Enter: L('已发送 ↵ 确认 (Enter)', 'Sent ↵ Confirm (Enter)'),
    Escape: L('已发送 Esc 取消', 'Sent Esc Cancel'),
  }[key] || L(`已发送按键 ${key}`, `Sent key ${key}`);
}

export async function sendSessionKeysApi(sessionId, { keys = [], text = null, submitText = true, busyTag = '', toastLabel = '' } = {}) {
  const sid = String(sessionId || '').trim();
  const cleanKeys = (Array.isArray(keys) ? keys : []).map((x) => String(x || '').trim()).filter(Boolean);
  const cleanText = typeof text === 'string' && text.length ? text : null;
  if (!sid || (!cleanKeys.length && !cleanText)) return undefined;
  const busyKey = `key:${sid}`;
  const btnBusyKey = busyTag ? `key:${sid}:${busyTag}` : (cleanKeys.length === 1 && !cleanText ? `key:${sid}:${cleanKeys[0]}` : '');
  if (UI.busy.has(busyKey)) return undefined;
  const initialPane = S.sessions.get(sid)?.pane || null;
  UI.busy.add(busyKey);
  if (btnBusyKey) UI.busy.add(btnBusyKey);
  APP.schedule();
  try {
    const payload = cleanKeys.length === 1 && !cleanText
      ? { key: cleanKeys[0] }
      : {
        keys: cleanKeys,
        ...(cleanText ? { text: cleanText, submit_text: submitText !== false } : {}),
      };
    const res = await api('POST', `/api/sessions/${enc(sid)}/key`, payload);
    toast(toastLabel || keySentToastLabel(cleanKeys[0] || '', cleanKeys, cleanText), 'ok');
    const refreshAfterKey = () => {
      syncStateSilent();
      const latest = S.sessions.get(sid);
      if (latest && latest.state !== 'ended') {
        loadSessionOutput(latest, { silent: true, forcePreview: true, force: true });
      } else if (initialPane) {
        loadPreview(initialPane, { mode: pvModeOf(initialPane), silent: true, force: true });
      }
    };
    setTimeout(refreshAfterKey, 140);
    setTimeout(refreshAfterKey, 550);
    return res;
  } catch (e) {
    if (e instanceof ApiError && e.status === 403) {
      toast(
        L('当前控制台处于只读模式，无法向终端发送交互按键', 'Console is read-only — cannot send interactive keys to terminal'),
        'warn',
      );
    } else {
      toast(
        L(`发送按键失败：${errText(e)}`, `Failed to send key: ${errText(e)}`),
        'err',
      );
    }
    return undefined;
  } finally {
    UI.busy.delete(busyKey);
    if (btnBusyKey) UI.busy.delete(btnBusyKey);
    APP.schedule();
  }
}

export async function sendSessionKeyApi(sessionId, key) {
  const k = String(key || '').trim();
  if (!k) return undefined;
  return sendSessionKeysApi(sessionId, { keys: [k], busyTag: k, toastLabel: keySentToastLabel(k) });
}

export async function closeSessionApi(sessionId, { isSplitPane = false } = {}) {
  const initial = sessionId ? S.sessions.get(sessionId) : null;
  const label = initial?.tmux_label || initial?.tmux_target || initial?.pane || sessionId || '';
  return act(`close-session:${sessionId}`, async () => {
    const res = await api('POST', `/api/sessions/${enc(sessionId)}/close`, {});
    const cur = S.sessions.get(sessionId);
    const selObj = UI.selSession ? S.sessions.get(UI.selSession) : null;
    const selIsClosed = UI.selSession === sessionId
      || Boolean(selObj?.is_subagent && selObj.parent_session_id === sessionId);
    const argObj = UI.arg ? S.sessions.get(UI.arg) : null;
    const argIsClosed = UI.arg === sessionId
      || Boolean(argObj?.is_subagent && argObj.parent_session_id === sessionId);

    if (cur) {
      const ended = { ...cur, state: 'ended', last_error: null };
      if (!UI.showEnded || ended.discovered_only) {
        S.sessions.delete(sessionId);
      } else {
        S.sessions.set(sessionId, ended);
      }
      if (!UI.showEnded) {
        for (const [subId, sub] of S.sessions.entries()) {
          if (sub.is_subagent && sub.parent_session_id === sessionId) {
            S.sessions.delete(subId);
          }
        }
      }
    }
    if (S.attention.length) {
      S.attention = S.attention.filter((a) => a.session_id !== sessionId);
    }
    if (UI.drawer?.kind === 'session' && UI.drawer.sessionId === sessionId) {
      APP.closeDrawer();
    } else if (UI.drawer?.kind === 'terminal' && cur?.pane && UI.drawer.pane === cur.pane) {
      APP.closeDrawer();
    }
    if (selIsClosed && (!UI.showEnded || !S.sessions.has(UI.selSession))) {
      UI.selSession = null;
    }
    if (argIsClosed && (!UI.showEnded || !S.sessions.has(UI.arg))) {
      UI.arg = null;
      if (UI.view === 'workspace') history.replaceState(null, '', '#/workspace');
    }
    pickDefaultSession();
    APP.closeModal();
    APP.schedule();
    return res;
  }, (r) => {
    const disp = label || r?.tmux_target || r?.pane || '';
    const suffix = disp ? ` ${disp}` : '';
    return isSplitPane
      ? L(`已关闭窗格${suffix}`, `Closed pane${suffix}`)
      : L(`已关闭窗口${suffix}`, `Closed window${suffix}`);
  });
}

export async function loadHandoffs(sessionId) {
  if (!sessionId) return;
  S.handoffs.set(sessionId, { state: 'loading', items: [] });
  APP.schedule();
  try {
    const items = await api('GET', `/api/handoffs${qs({ session: sessionId })}`);
    S.handoffs.set(sessionId, { state: 'ok', items: items || [] });
  } catch (e) {
    S.handoffs.set(sessionId, { state: 'err', err: errText(e), items: [] });
  }
  APP.schedule();
}

export async function loadReviews() {
  if (S.reviewsState === 'loading') return;
  S.reviewsState = 'loading';
  APP.schedule();
  try {
    const list = await api('GET', '/api/reviews');
    S.reviews = new Map((list || []).map((r) => [r.id, r]));
    S.reviewsState = 'ok';
    S.reviewsErr = null;
  } catch (e) {
    S.reviewsState = 'err';
    S.reviewsErr = errText(e);
  }
  APP.schedule();
}

export async function refreshReview(id) {
  try {
    const r = await api('GET', `/api/reviews/${enc(id)}`);
    if (r && r.id) S.reviews.set(r.id, r);
  } catch (e) { console.warn('review refresh failed', id, e); }
  APP.schedule();
}

export function ensureReview(id) {
  if (!id || S.reviews.has(id) || UI.busy.has(`rv:${id}`)) return S.reviews.get(id);
  UI.busy.add(`rv:${id}`);
  refreshReview(id).finally(() => UI.busy.delete(`rv:${id}`));
  return undefined;
}

export async function loadPrecision() {
  S.precisionState = 'loading';
  APP.schedule();
  try {
    S.precision = await api('GET', `/api/reviews/precision${qs({ agent: UI.precAgent })}`) || [];
    S.precisionState = 'ok';
    S.precisionErr = null;
  } catch (e) {
    S.precisionState = 'err';
    S.precisionErr = errText(e);
  }
  APP.schedule();
}

export function artifact(id) {
  let a = S.artifacts.get(id);
  if (!a) {
    a = { state: 'loading' };
    S.artifacts.set(id, a);
    api('GET', `/api/artifacts/${enc(id)}`)
      .then((d) => { a.state = 'ok'; a.data = d; })
      .catch((e) => { a.state = 'err'; a.err = errText(e); })
      .finally(APP.schedule);
  }
  return a;
}

function handleOutputEvent(d) {
  const sid = d?.session_id;
  if (!sid || typeof sid !== 'string') return false;
  const now = Date.now();
  let s = S.sessions.get(sid) || null;
  let metricsChanged = false;
  if (d.metrics && typeof d.metrics === 'object' && s) {
    if (JSON.stringify(s.metrics || null) !== JSON.stringify(d.metrics)) {
      s = { ...s, metrics: d.metrics };
      S.sessions.set(sid, s);
      APP.patchTelemetryStrip(sid);
      metricsChanged = true;
    }
  }
  const isActive = sid === currentSession()?.session_id;
  const hasLiveTail = typeof d.live_tail === 'string';
  if (hasLiveTail) {
    setLiveTailEntry(sid, d.live_tail, now);
  }

  const cur = S.transcripts.get(sid);
  const appended = Array.isArray(d.appended_messages) ? d.appended_messages : [];
  const hasAppended = appended.length > 0 && typeof d.start_seq === 'number' && Number.isFinite(d.start_seq);
  let handledInline = false;
  let needsReload = false;

  if (cur?.state === 'ok' && hasAppended) {
    if (cur.source === 'none' && d.start_seq > 0) {
      needsReload = true;
    } else {
      const curStart = Number.isFinite(cur.startSeq) ? cur.startSeq : 0;
      const curLen = Array.isArray(cur.messages) ? cur.messages.length : 0;
      const curTotal = Number.isFinite(cur.totalMessages) ? cur.totalMessages : (curStart + curLen);
      const curEnd = curStart + curLen;
      const isContiguous = d.start_seq === curTotal && !cur.autoPollPaused && curEnd === curTotal;
      const isOverlap = !isContiguous && !cur.autoPollPaused && d.start_seq >= curStart && d.start_seq < curEnd;
      if (isContiguous || isOverlap) {
        const prefixLen = isContiguous ? curLen : (d.start_seq - curStart);
        const hasAuthoritativeTotal = typeof d.total_messages === 'number' && Number.isFinite(d.total_messages);
        const isAuthoritativeTail = isOverlap && hasAuthoritativeTotal && (d.start_seq + appended.length === d.total_messages);
        let nextMsgs = [...cur.messages.slice(0, prefixLen), ...appended];
        if (isOverlap && !isAuthoritativeTail && prefixLen + appended.length < curLen) {
          nextMsgs = [...nextMsgs, ...cur.messages.slice(prefixLen + appended.length)];
        }
        let nextStartSeq = curStart;
        if (!cur.explicitAll) {
          const cap = Math.max(COMPACT_TRANSCRIPT_THRESHOLD, UI.trLimit.get(sid) || TRANSCRIPT_LIMIT);
          if (nextMsgs.length > cap) {
            const drop = nextMsgs.length - cap;
            nextMsgs = nextMsgs.slice(drop);
            nextStartSeq += drop;
          }
        }
        const nextHasMore = nextStartSeq > 0;
        const nextTotal = isAuthoritativeTail
          ? d.total_messages
          : (hasAuthoritativeTotal
            ? Math.max(d.total_messages, nextStartSeq + nextMsgs.length)
            : Math.max(curTotal, nextStartSeq + nextMsgs.length));
        const nextHash = hashTranscriptPayload('transcript', nextStartSeq, nextTotal, nextHasMore, nextMsgs);
        const nextEntry = {
          ...cur,
          state: 'ok',
          source: 'transcript',
          messages: nextMsgs,
          hasMore: nextHasMore,
          totalMessages: nextTotal,
          startSeq: nextStartSeq,
          nextBefore: nextHasMore ? nextStartSeq : null,
          loadedAll: !nextHasMore,
          autoPollPaused: false,
          latestTotalMessages: null,
          limit: Math.max(cur.limit || TRANSCRIPT_LIMIT, nextMsgs.length),
          hash: nextHash,
          live_tail: hasLiveTail ? d.live_tail : cur.live_tail,
          liveTailUpdatedMs: hasLiveTail ? now : (cur.liveTailUpdatedMs || 0),
          err: null,
          staleErr: null,
          updatedMs: now,
        };
        setTranscriptEntry(sid, nextEntry);
        reconcileOptimisticTurns(sid, nextEntry);
        handledInline = true;
        if (isActive) APP.notifyOutputUpdated(sid);
      } else if (cur.autoPollPaused && typeof d.total_messages === 'number' && Number.isFinite(d.total_messages)) {
        setTranscriptEntry(sid, {
          ...cur,
          latestTotalMessages: Math.max(cur.latestTotalMessages || 0, d.total_messages),
          live_tail: hasLiveTail ? d.live_tail : cur.live_tail,
          liveTailUpdatedMs: hasLiveTail ? now : (cur.liveTailUpdatedMs || 0),
          staleErr: null,
          updatedMs: now,
        });
        handledInline = true;
        if (isActive) APP.notifyOutputUpdated(sid);
      } else if (isActive) {
        needsReload = true;
      }
    }
  } else if (hasLiveTail && cur?.state === 'ok') {
    cur.live_tail = d.live_tail;
    cur.liveTailUpdatedMs = now;
    cur.staleErr = null;
    if (isActive && s && s.agent !== 'shell' && UI.outputMode === 'reading') {
      APP.notifyOutputUpdated(sid);
    }
  } else if (hasLiveTail && isActive && s && s.agent !== 'shell' && UI.outputMode === 'reading') {
    APP.notifyOutputUpdated(sid);
  }

  if (isActive && s && s.state !== 'ended') {
    const isRawMode = s.agent === 'shell' || UI.outputMode === 'raw';
    if (isRawMode) {
      if ((d.dirty || hasLiveTail) && s.pane) {
        const pvMode = pvModeOf(s.pane);
        const existingPv = S.previews.get(pvKeyOf(s.pane, pvMode));
        if (pvMode !== 'all' && !existingPv?.paginated) {
          loadPreview(s.pane, { mode: pvMode, silent: true });
        }
      }
    } else {
      const latestTr = S.transcripts.get(sid);
      const needsTranscriptRefresh = !handledInline && (d.dirty || needsReload || (hasAppended && (!cur || cur.state !== 'ok')));
      if (needsTranscriptRefresh) {
        loadSessionOutput(s, { silent: true });
      } else if (hasLiveTail && s.pane && (!latestTr || latestTr.state !== 'ok' || latestTr.source === 'none' || latestTr.unsupported)) {
        const pvMode = pvModeOf(s.pane);
        const existingPv = S.previews.get(pvKeyOf(s.pane, pvMode));
        if (pvMode !== 'all' && !existingPv?.paginated) {
          loadPreview(s.pane, { mode: pvMode, silent: true });
        }
      }
    }
  }
  return metricsChanged;
}

function handleEvent(type, d) {
  if (d?.seq) S.lastSeq = Math.max(S.lastSeq, d.seq);
  switch (type) {
    case 'ping':
      if (d?.hello) {
        if (setLang(d.lang)) {
          S.slashCmds.clear();
          APP.rerender();
        }
        if (d.version) S.version = d.version;
        const prevUptimeMs = S.uptimeMs;
        if (typeof d.uptime_ms === 'number') S.uptimeMs = d.uptime_ms;
        if (checkUiHash(d.ui_hash)) return;
        const restarted = prevUptimeMs != null && typeof d.uptime_ms === 'number' && d.uptime_ms < prevUptimeMs;
        if (restarted || !S.loaded || (Date.now() - S.lastLoad > 1500)) {
          loadAll();
        }
      }
      return;
    case 'restarting': {
      S.restarting = true;
      S.conn = 'connecting';
      S.healthFails = 0;
      S.retry = 0;
      clearTimeout(S.reconnectTimer);
      if (S.es) {
        S.es.close();
        S.es = null;
      }
      startFastRecovery({ restarting: true });
      APP.schedule();
      return;
    }
    case 'output': {
      if (handleOutputEvent(d)) APP.schedulePassive();
      return;
    }
    case 'session': {
      const ns = d.session;
      const sid = ns?.session_id;
      const prev = sid ? S.sessions.get(sid) : null;
      if (sid && (!prev || prev.state !== ns.state)) {
        sessionStateChangedAt.set(sid, Date.now());
      }
      if (sid && (!prev || prev.state !== ns.state || ns.state === 'ended')) {
        const oldTimer = postIdleTimers.get(sid);
        if (oldTimer) {
          clearTimeout(oldTimer);
          postIdleTimers.delete(sid);
        }
      }
      upsertSession(ns);
      pickDefaultSession();
      const activeSid = currentSession()?.session_id;
      if (sid && sid === activeSid && ns.state !== 'ended') {
        if (
          !prev
          || prev.state !== ns.state
          || prev.last_message !== ns.last_message
          || prev.last_error !== ns.last_error
          || prev.transcript_path !== ns.transcript_path
        ) {
          loadSessionOutput(ns, { silent: true });
        }
        if (prev && ['working', 'starting'].includes(prev.state) && !['working', 'starting'].includes(ns.state)) {
          const timerId = setTimeout(() => {
            postIdleTimers.delete(sid);
            const latest = S.sessions.get(sid);
            if (!latest || latest.state === 'ended') return;
            if (currentSession()?.session_id !== sid) return;
            if (isHostOffline(hostOf(latest))) return;
            loadSessionOutput(latest, { silent: true });
          }, 650);
          postIdleTimers.set(sid, timerId);
        }
      }
      break;
    }
    case 'host':
      if (d.host && d.host.id) {
        const prevHost = S.hosts.get(d.host.id);
        S.hosts.set(d.host.id, d.host);
        if (prevHost && prevHost.status !== 'online' && d.host.status === 'online') {
          const cur = currentSession();
          if (cur && hostOf(cur) === d.host.id && cur.state !== 'ended') {
            loadSessionOutput(cur, { silent: true });
          }
        }
      }
      break;
    case 'pipeline': upsertPipeline(d.pipeline); break;
    case 'attention': S.attention = d.items || []; break;
    case 'notifiers': applyNotifiersSnapshot(d); break;
    case 'resync': syncStateSilent({ refreshActive: true }); return;
    case 'review': refreshReview(d.review_id); break;
    case 'artifact': if (d.kind === 'review' && S.reviewsState === 'ok') loadReviews(); break;
    case 'delivery': {
      const r = d.report || {};
      if (r.status === 'unconfirmed' || r.status === 'failed') toast(L(`投递${stateText(r.status)}：${deliveryText(r)}`, `Delivery ${stateText(r.status)}: ${deliveryText(r)}`), 'warn');
      return;
    }
    case 'event': {
      S.activity.unshift(d.event);
      if (S.activity.length > MAX_ACTIVITY) S.activity.length = MAX_ACTIVITY;
      // Activity feed is only rendered inside the session inspector drawer; skip rerenders otherwise.
      if (UI.drawer?.kind === 'session') APP.schedulePassive();
      return;
    }
    default: break;
  }
  APP.schedulePassive();
}

export function connectSSE() {
  clearTimeout(S.reconnectTimer);
  if (S.es) S.es.close();
  lastSseConnectAt = Date.now();
  S.conn = 'connecting';
  const es = new EventSource('/api/events');
  S.es = es;
  es.onopen = () => {
    if (S.es !== es) return;
    stopFastRecovery();
    S.conn = 'live';
    S.restarting = false;
    S.retry = 0;
    S.healthFails = 0;
    S.lastSseAt = Date.now();
    if (!uiReloading && (Date.now() - S.lastLoad > 1500)) loadAll();
    APP.schedule();
  };
  es.onerror = () => {
    if (S.es !== es || S.restarting) return;
    if (es.readyState === EventSource.CLOSED) {
      S.conn = 'down';
      const wait = RECONNECT_STEPS_MS[Math.min(S.retry++, RECONNECT_STEPS_MS.length - 1)] || MAX_BACKOFF_MS;
      S.reconnectTimer = setTimeout(connectSSE, wait);
    } else S.conn = 'connecting';
    startFastRecovery({ restarting: false });
    APP.schedule();
  };
  for (const t of SSE_TYPES) {
    es.addEventListener(t, (ev) => {
      S.lastSseAt = Date.now();
      let d;
      try { d = JSON.parse(ev.data); } catch { return; }
      handleEvent(t, d);
    });
  }
}

export async function loadModels() {
  try {
    const data = await api('GET', '/api/models');
    if (data && typeof data === 'object') { S.models = data; APP.schedule(); }
  } catch (e) { console.warn('GET /api/models fallback', e); }
}

export async function loadTmuxSessions(hostId = 'local') {
  const hid = (hostId && String(hostId).trim()) || 'local';
  if (tmuxSessInFlight.has(hid)) return S.tmuxSessionsByHost.get(hid) || null;
  if (isHostOffline(hid)) return S.tmuxSessionsByHost.get(hid) || null;
  tmuxSessInFlight.add(hid);
  if (UI.modal === 'spawn') APP.schedule();
  try {
    const path = hid === 'local' ? '/api/tmux/sessions' : `/api/tmux/sessions${qs({ host: hid })}`;
    const data = await api('GET', path);
    if (Array.isArray(data?.sessions)) {
      S.tmuxSessionsByHost.set(hid, data.sessions);
      if (hid === 'local') S.tmuxSessions = data.sessions;
      APP.schedule();
      return data.sessions;
    }
    return null;
  } catch (e) {
    console.warn('GET /api/tmux/sessions failed', hid, e);
    return null;
  } finally {
    tmuxSessInFlight.delete(hid);
    if (UI.modal === 'spawn') APP.schedule();
  }
}

function isMissingRouteError(e) {
  return e instanceof ApiError && (e.status === 405 || (e.status === 404 && /no such route/i.test(e.message || '')));
}

export async function loadNotifiers({ silent = false } = {}) {
  try {
    const data = await api('GET', '/api/notifiers');
    const changed = applyNotifiersSnapshot(data);
    if (changed || !silent) {
      if (silent) APP.schedulePassive();
      else APP.schedule();
    }
    return data;
  } catch (e) {
    if (silent) {
      console.warn('GET /api/notifiers failed', e);
      return null;
    }
    throw e;
  }
}

export async function muteNotifiersApi({ target = 'all', minutes, until_ms } = {}) {
  const cleanTarget = String(target || 'all').trim() || 'all';
  const body = { target: cleanTarget };
  if (minutes !== undefined) body.minutes = minutes;
  if (until_ms !== undefined) body.until_ms = until_ms;
  try {
    return await api('POST', '/api/notifiers/mute', body);
  } catch (e) {
    if (isMissingRouteError(e)) {
      if (cleanTarget === 'all' || cleanTarget === 'feishu-default') {
        const legacyBody = {};
        if (minutes !== undefined) legacyBody.minutes = minutes;
        if (until_ms !== undefined) legacyBody.until_ms = until_ms;
        try {
          return await api('POST', '/api/feishu/mute', legacyBody);
        } catch (legacyErr) {
          if (isMissingRouteError(legacyErr)) {
            throw new ApiError(
              'unavailable',
              L('当前 daemon 未启用 Web 通知静音接口', 'Web notification mute API is not enabled on this daemon'),
              legacyErr.status,
            );
          }
          throw legacyErr;
        }
      }
      throw new ApiError(
        'unavailable',
        L('当前 daemon 暂不支持按单渠道静音，请升级或选择「全部通知渠道」', 'This daemon version does not support per-channel mute yet; upgrade or select "All channels"'),
        e.status,
      );
    }
    if (e instanceof ApiError && e.status === 404) {
      loadNotifiers({ silent: true });
    }
    throw e;
  }
}

export async function testNotifierApi(id, text) {
  const cleanId = String(id || '').trim();
  if (!cleanId) {
    throw new ApiError('bad_request', L('缺少通知渠道 ID', 'Missing notification channel ID'), 400);
  }
  try {
    return await api('POST', `/api/notifiers/${enc(cleanId)}/test`, { text });
  } catch (e) {
    if (cleanId === 'feishu-default' && isMissingRouteError(e)) {
      return api('POST', '/api/feishu/test', { text });
    }
    throw e;
  }
}

export async function loadSlashCommands(agent) {
  if (agent === 'shell') return;
  const ag = AGENTS.includes(agent) ? agent : 'claude';
  if (S.slashCmds.has(ag) || S.slashFetching.has(ag)) return;
  S.slashFetching.add(ag);
  try {
    const data = await api('GET', `/api/slash-commands${qs({ agent: ag })}`);
    const list = Array.isArray(data?.commands) ? data.commands : [];
    if (list.length) {
      const hasStarfane = list.some((c) => c.group === 'starfane');
      S.slashCmds.set(ag, hasStarfane ? list : [...list, ...starfaneSlash()]);
      if (UI.slash.open && UI.wsShell?.comp?.ta && UI.wsShell.comp.agent === ag) {
        APP.updateSlashFromInput(UI.wsShell.comp.ta, ag);
      }
    }
  } catch { /* fallback to built-in slash list */ } finally {
    S.slashFetching.delete(ag);
  }
}

export async function pollHealth({ fromRecovery = false } = {}) {
  if (document.hidden && S.loaded) return;
  if (healthInFlight) return;
  healthInFlight = true;
  const prevFails = S.healthFails;
  const prevVer = S.version;
  const prevAuth = S.authNeeded;
  const prevRestarting = S.restarting;
  const prevUptimeMs = S.uptimeMs;
  const prevUpMin = S.uptimeMs != null ? Math.floor(S.uptimeMs / 60000) : -1;
  try {
    const r = await api('GET', '/api/health', undefined, { timeoutMs: HEALTH_TIMEOUT_MS });
    if (setLang(r.lang)) {
      S.slashCmds.clear();
      APP.rerender();
    }
    S.healthFails = 0;
    S.restarting = false;
    S.version = r.version || S.version;
    S.uptimeMs = r.uptime_ms ?? null;
    if (typeof r.auth_enabled === 'boolean') {
      S.authEnabled = r.auth_enabled;
      if (!r.auth_enabled) S.authNeeded = false;
    }
    if (checkUiHash(r?.ui_hash)) return;
    const restarted = prevRestarting || (prevUptimeMs != null && r.uptime_ms != null && r.uptime_ms < prevUptimeMs);
    const sseSilent = S.conn === 'live' && S.lastSseAt > 0 && (Date.now() - S.lastSseAt) > 45000;
    const sseNotLive = S.conn !== 'live';
    const recentlyConnecting = S.conn === 'connecting'
      && S.es
      && S.es.readyState === EventSource.CONNECTING
      && (Date.now() - lastSseConnectAt) < 1200;
    if (restarted) {
      console.warn('daemon restarted, reconnecting SSE and reloading state');
      stopFastRecovery();
      connectSSE();
      loadAll();
    } else {
      if ((sseNotLive && !recentlyConnecting) || sseSilent) {
        if (sseSilent) console.warn('SSE heartbeat silent for >45s, reconnecting SSE');
        connectSSE();
      }
      if (S.conn === 'live') stopFastRecovery();
      if (!S.loaded) loadAll();
    }
  } catch {
    if (!S.restarting) S.healthFails++;
    if (!fromRecovery && S.healthFails >= 1) startFastRecovery({ restarting: false });
  } finally {
    healthInFlight = false;
  }
  const nextUpMin = S.uptimeMs != null ? Math.floor(S.uptimeMs / 60000) : -1;
  if (S.healthFails !== prevFails || S.restarting !== prevRestarting || S.version !== prevVer || S.authNeeded !== prevAuth || nextUpMin !== prevUpMin) {
    APP.schedulePassive();
  }
}

export function refreshActiveOnResume() {
  const recentlyConnecting = S.conn === 'connecting'
    && S.es
    && S.es.readyState === EventSource.CONNECTING
    && (Date.now() - lastSseConnectAt) < 2000;
  if (!recentlyConnecting && (S.conn !== 'live' || (S.lastSseAt > 0 && (Date.now() - S.lastSseAt) > 45000))) {
    connectSSE();
  }
  if (S.loaded && UI.view === 'workspace') {
    const s = currentSession();
    if (s && s.state !== 'ended' && !isHostOffline(hostOf(s))) {
      sessionPollAt.set(s.session_id, Date.now());
      loadSessionOutput(s, { silent: true, force: true });
    }
  }
}

const SSE_LIVE_BUSY_POLL_MS = 6000;
const SSE_LIVE_IDLE_LOCAL_POLL_MS = 12000;
const SSE_LIVE_IDLE_REMOTE_POLL_MS = 15000;
const IDLE_READING_POLL_MS = 12000;
const REMOTE_BUSY_POLL_MIN_MS = 4500;
const REMOTE_BUSY_POLL_MAX_MS = 10000;
const REMOTE_RTT_POLL_MULT = 12;
const REMOTE_IDLE_POLL_MS = 15000;
const RECENTLY_SETTLED_MS = 15000;
const POLL_JITTER_SLOP_MS = 250;

export function pollActivePreview() {
  if (document.hidden || !S.loaded || !UI.autoRefreshPreview || UI.view !== 'workspace') return;
  const s = currentSession();
  if (!s || s.state === 'ended') return;
  const isReadingView = s.agent !== 'shell' && UI.outputMode === 'reading';
  // Only pause automatic preview polling on scrollback in raw/shell mode; reading mode preserves scrollTop.
  const vp = !isReadingView ? (UI.wsShell?.vp || $('.term-viewport')) : null;
  if (vp && (vp.scrollHeight - vp.scrollTop - vp.clientHeight > 120)) return;
  const hid = hostOf(s);
  const hostRec = S.hosts.get(hid);
  if (hid !== 'local' && hostRec?.status === 'offline') return;
  const tr = S.transcripts.get(s.session_id);
  const pv = s.pane ? S.previews.get(pvKeyOf(s.pane)) : null;
  if (trInFlight.has(s.session_id) || (s.pane && pvInFlight.has(pvKeyOf(s.pane)))) return;

  const now = Date.now();
  const sseLive = S.conn === 'live';
  const isBusy = ['working', 'starting', 'needs_permission', 'needs_input', 'error'].includes(s.state)
    || (!sseLive && hid === 'local' && Boolean(s.discovered_only))
    || (!sseLive && isReadingView && sessionNeedsLiveTail(s, tr));
  const isRecentlySettled = (now - (sessionStateChangedAt.get(s.session_id) || 0)) < RECENTLY_SETTLED_MS;
  const hasTranscript = isReadingView && tr?.state === 'ok' && tr?.source === 'transcript';
  const lastActivity = Math.max(
    sessionPollAt.get(s.session_id) || 0,
    hasTranscript ? (tr?.updatedMs || 0) : (pv?.updatedMs || 0),
  );

  let intervalMs = 0;
  if (sseLive && isReadingView && !tr?.unsupported) {
    intervalMs = (isBusy || isRecentlySettled)
      ? SSE_LIVE_BUSY_POLL_MS
      : (hid !== 'local' ? SSE_LIVE_IDLE_REMOTE_POLL_MS : SSE_LIVE_IDLE_LOCAL_POLL_MS);
  } else if (hid !== 'local') {
    const rtt = typeof hostRec?.rtt_ms === 'number' && Number.isFinite(hostRec.rtt_ms) ? hostRec.rtt_ms : 0;
    intervalMs = (isBusy || isRecentlySettled)
      ? Math.max(REMOTE_BUSY_POLL_MIN_MS, Math.min(REMOTE_BUSY_POLL_MAX_MS, rtt * REMOTE_RTT_POLL_MULT))
      : REMOTE_IDLE_POLL_MS;
  } else if (isReadingView && !(isBusy || isRecentlySettled)) {
    intervalMs = IDLE_READING_POLL_MS;
  }

  if (intervalMs > 0 && lastActivity && (now - lastActivity) < Math.max(0, intervalMs - POLL_JITTER_SLOP_MS)) return;
  sessionPollAt.set(s.session_id, now);
  loadSessionOutput(s, { silent: true });
}
