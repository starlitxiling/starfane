// starfane web console — HTTP client, SSE stream, true cursor pagination, and background polling
import {
  AGENTS,
  MAX_ACTIVITY,
  MAX_BACKOFF_MS,
  PREVIEW_LINES,
  PREVIEW_PAGE_STEP,
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
  currentSession,
  enc,
  hashMessage,
  hashStr,
  hashTranscriptPayload,
  migrateSessionSelection,
  pickDefaultSession,
  pvKeyOf,
  pvModeOf,
  pvReqSeq,
  setMute,
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

export async function api(method, path, body) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), REQUEST_TIMEOUT_MS);
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
    .map((s) => `${s.session_id}|${s.host_id || 'local'}|${s.host_display_name || ''}|${s.mux_kind || ''}|${s.state}|${s.tmux_label || ''}|${s.tmux_target || ''}|${s.pane || ''}|${s.model || ''}|${s.effort || ''}|${s.last_message || ''}|${s.last_error || ''}|${s.subagent_count || 0}|${s.metrics?.context_used_pct ?? ''}|${s.metrics?.tps ?? ''}`)
    .sort()
    .join('\n');
}

function hostsDiffSig(list) {
  return (list || [])
    .map((h) => `${h.id}|${h.display_name}|${h.status}|${h.session_count || 0}|${h.error || ''}`)
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
export async function syncStateSilent() {
  if (document.hidden || !S.loaded || S.authNeeded || stateSyncBusy) return;
  stateSyncBusy = true;
  try {
    const st = await api('GET', '/api/state');
    const prevSig = sessionsDiffSig(S.sessions);
    const prevHostsSig = hostsDiffSig([...S.hosts.values()]);
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
    const nextHostsSig = hostsDiffSig(st.hosts || []);
    const nextPipelines = new Map();
    for (const p of st.pipelines || []) if (p && p.id) nextPipelines.set(p.id, p);
    const nextPipe = [...nextPipelines.values()].map((p) => `${p.id}:${p.status}:${p.updated_ms || 0}`).sort().join(',');
    const nextAttn = st.attention || [];
    const nextMute = typeof st.feishu_muted_until_ms === 'number' ? st.feishu_muted_until_ms : (st.feishu_muted_until_ms === null ? 0 : S.muteUntil);
    const changed = prevSig !== nextSig || prevHostsSig !== nextHostsSig || prevPipe !== nextPipe || prevAttn !== nextAttn.length || prevMute !== nextMute;
    S.lastSeq = Math.max(S.lastSeq, st.event_seq || 0);
    if (st.terminal_font || st.has_user_term_font) applyTerminalFontConfig(st.terminal_font || '', !!st.has_user_term_font);
    if (changed) {
      S.sessions = nextSessions;
      S.pipelines = nextPipelines;
      S.attention = nextAttn;
      if (Array.isArray(st.hosts)) upsertHosts(st.hosts);
      if (nextMute !== prevMute) setMute(nextMute);
      pickDefaultSession();
      APP.schedulePassive();
      const subQs = UI.showEnded ? { subagents: 1, ended: 1 } : { subagents: 1 };
      if (UI.showEnded || [...nextSessions.values()].some((s) => (s.subagent_count || 0) > 0)) {
        api('GET', `/api/sessions${qs(subQs)}`)
          .then((list) => {
            const before = sessionsDiffSig(S.sessions);
            (list || []).forEach(upsertSession);
            if (sessionsDiffSig(S.sessions) !== before) APP.schedulePassive();
          })
          .catch(() => {});
      }
    }
  } catch { /* silent fallback */ } finally {
    stateSyncBusy = false;
  }
}

export async function loadAll() {
  S.lastLoad = Date.now();
  try {
    const st = await api('GET', '/api/state');
    if (setLang(st.lang)) S.slashCmds.clear();
    S.version = st.version || S.version;
    S.lastSeq = st.event_seq || 0;
    S.sessions = new Map();
    S.pipelines = new Map();
    if (Array.isArray(st.hosts)) upsertHosts(st.hosts);
    applyTerminalFontConfig(st.terminal_font || '', !!st.has_user_term_font);
    (st.sessions || []).forEach(upsertSession);
    (st.pipelines || []).forEach(upsertPipeline);
    S.attention = st.attention || [];
    if (typeof st.feishu_muted_until_ms === 'number') setMute(st.feishu_muted_until_ms);
    else if (st.feishu_muted_until_ms === null) setMute(0);
    S.loaded = true;
    S.loadError = null;
    S.authNeeded = false;
    pickDefaultSession();
    const subQs = UI.showEnded ? { subagents: 1, ended: 1 } : { subagents: 1 };
    api('GET', `/api/sessions${qs(subQs)}`)
      .then((list) => {
        (list || []).forEach(upsertSession);
        pickDefaultSession();
        APP.schedule();
      })
      .catch((e) => console.warn('sessions?subagents=1 failed', e));
  } catch (e) {
    S.loadError = errText(e);
  }
  APP.schedule();
}

export function loadSessionOutput(s, { silent = false, forcePreview = false } = {}) {
  if (!s) return;
  if (s.agent !== 'shell') loadTranscript(s.session_id, { silent });
  if (!s.pane) return;
  const pvMode = pvModeOf(s.pane);
  if (silent && !forcePreview && s.agent !== 'shell' && UI.outputMode !== 'raw') {
    const tr = S.transcripts.get(s.session_id);
    const tailOpen = UI.open.has(`tail:${s.session_id}`) || Boolean($('details.live-tail')?.open);
    if (!sessionNeedsLiveTail(s, tr) && !tailOpen) return;
  }
  // Never clobber full-history or upward-paginated terminal views during background silent polls.
  const existingPv = S.previews.get(pvKeyOf(s.pane, pvMode));
  if (silent && !forcePreview && (pvMode === 'all' || existingPv?.paginated)) return;
  loadPreview(s.pane, { mode: pvMode, silent });
}

export async function loadTranscript(sessionId, { silent = false, limit, all = false } = {}) {
  if (!sessionId) return;
  const prev = S.transcripts.get(sessionId);
  if (prev?.unsupported) return;
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
  if (!prev) {
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
      updatedMs: 0,
    });
    if (!silent) APP.schedule();
  }
  try {
    const q = effectiveAll ? { all: 1 } : { limit: lim };
    const data = await api('GET', `/api/sessions/${enc(sessionId)}/transcript${qs(q)}`);
    if (trReqSeq.get(sessionId) !== seq) return;
    const msgs = Array.isArray(data?.messages) ? data.messages : [];
    const source = data?.source === 'transcript' && msgs.length > 0 ? 'transcript' : 'none';
    const hasMore = Boolean(data?.has_more);
    const totalMessages = Number.isFinite(data?.total_messages) ? data.total_messages : msgs.length;
    const startSeq = Number.isFinite(data?.start_seq) ? data.start_seq : Math.max(0, totalMessages - msgs.length);
    const nextBefore = Number.isFinite(data?.next_before) ? data.next_before : (hasMore ? startSeq : null);

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
          prev.updatedMs = Date.now();
          APP.updateOutputClock(sessionId);
          if (!silent) APP.schedule();
          return;
        }
        const merged = [...prev.messages.slice(0, prefixLen), ...msgs];
        const mergedHasMore = prevStart > 0;
        const mergedTotal = Math.max(totalMessages, prevStart + merged.length);
        const nextHash = hashTranscriptPayload(source, prevStart, mergedTotal, mergedHasMore, merged);
        setTranscriptEntry(sessionId, {
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
          updatedMs: Date.now(),
          err: null,
        });
        APP.notifyOutputUpdated(sessionId);
        return;
      }
      if (silent) {
        // Larger gap than one 60-message tail while reviewing expanded/full history:
        // pause silent auto-refresh instead of silently downloading `all=1` or clobbering scrollback.
        setTranscriptEntry(sessionId, {
          ...prev,
          autoPollPaused: true,
          latestTotalMessages: totalMessages,
          updatedMs: Date.now(),
        });
        APP.notifyOutputUpdated(sessionId);
        return;
      }
    }

    const nextHash = hashTranscriptPayload(source, startSeq, totalMessages, hasMore, msgs);
    const changed = !prev || prev.state !== 'ok' || prev.hash !== nextHash || Boolean(prev.autoPollPaused);
    setTranscriptEntry(sessionId, {
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
      updatedMs: Date.now(),
      err: null,
    });
    if (changed) APP.notifyOutputUpdated(sessionId);
    else {
      if (UI.loadingEarlierSid === sessionId) UI.loadingEarlierSid = null;
      APP.updateOutputClock(sessionId);
      if (!silent) APP.schedule();
    }
  } catch (e) {
    if (trReqSeq.get(sessionId) !== seq) return;
    if (UI.loadingEarlierSid === sessionId) UI.loadingEarlierSid = null;
    if (e instanceof ApiError && e.status === 405) {
      setTranscriptEntry(sessionId, {
        state: 'ok', source: 'none', messages: [], hasMore: false, totalMessages: 0, startSeq: 0, nextBefore: null,
        explicitAll: false, loadedAll: true, autoPollPaused: false, latestTotalMessages: null,
        limit: TRANSCRIPT_LIMIT, hash: 'none', updatedMs: Date.now(), unsupported: true,
      });
      if (!prev || prev.state === 'loading') APP.notifyOutputUpdated(sessionId);
      return;
    }
    const errMsg = errText(e);
    const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
    setTranscriptEntry(sessionId, {
      ...(prev || { source: 'none', messages: [], hasMore: false, totalMessages: 0, startSeq: 0, nextBefore: null, explicitAll: false, autoPollPaused: false, latestTotalMessages: null, limit: TRANSCRIPT_LIMIT, hash: '' }),
      state: 'err',
      err: errMsg,
      updatedMs: Date.now(),
    });
    if (changed) APP.notifyOutputUpdated(sessionId);
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
      updatedMs: Date.now(),
    });
    APP.notifyOutputUpdated(sessionId);
  });
}

export async function loadAllTranscript(sessionId) {
  UI.loadingEarlierSid = sessionId;
  return act(`trall:${sessionId}`, () => loadTranscript(sessionId, { silent: false, all: true }));
}

export async function loadPreview(paneId, { lines, mode, silent = false } = {}) {
  if (!paneId) return;
  const effMode = mode !== undefined ? mode : (lines !== undefined ? lines : pvModeOf(paneId));
  const isAll = effMode === 'all';
  const key = pvKeyOf(paneId, effMode);
  const prev = S.previews.get(key);
  const seq = (pvReqSeq.get(key) || 0) + 1;
  pvReqSeq.set(key, seq);
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
      updatedMs: 0,
    });
    if (!silent) APP.schedule();
  }
  try {
    const query = isAll ? { all: 1 } : { lines: Number(effMode) || PREVIEW_LINES };
    const data = await api('GET', `/api/panes/${enc(paneId)}/preview${qs(query)}`);
    if (pvReqSeq.get(key) !== seq) return;
    const text = data?.text ?? '';
    const totalLines = Number.isFinite(data?.total_lines) ? data.total_lines : (text ? text.split('\n').length : 0);
    const startLine = Number.isFinite(data?.start_line) ? data.start_line : 0;
    const hasMore = Boolean(data?.has_more);
    const nextBeforeLine = Number.isFinite(data?.next_before_line) ? data.next_before_line : (hasMore ? startLine : null);
    const truncated = Boolean(data?.truncated);
    const nextHash = hashStr(`${text}|${startLine}|${totalLines}|${hasMore ? 1 : 0}|${!!data?.in_mode}|${!!data?.dead}`);
    const changed = !prev || prev.state !== 'ok' || prev.hash !== nextHash;
    setPreviewEntry(key, {
      state: 'ok',
      updatedMs: Date.now(),
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
    });
    if (effMode === pvModeOf(paneId)) {
      const cur = currentSession();
      if (cur?.pane === paneId) {
        if (changed) APP.notifyOutputUpdated(cur.session_id);
        else APP.updateOutputClock(cur.session_id);
      }
    } else if (changed) {
      APP.schedulePassive();
    }
  } catch (e) {
    if (pvReqSeq.get(key) !== seq) return;
    const errMsg = errText(e);
    const changed = !prev || prev.state !== 'err' || prev.err !== errMsg;
    setPreviewEntry(key, {
      ...(prev || { text: '', hash: '', totalLines: 0, startLine: 0, hasMore: false, nextBeforeLine: null }),
      state: 'err',
      err: errMsg,
      status: e?.status || 0,
      updatedMs: Date.now(),
    });
    if (changed) {
      const cur = currentSession();
      if (effMode === pvModeOf(paneId) && cur?.pane === paneId) APP.notifyOutputUpdated(cur.session_id);
      else APP.schedulePassive();
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
      updatedMs: Date.now(),
    });
    const cur = currentSession();
    if (cur?.pane === paneId) APP.notifyOutputUpdated(cur.session_id);
    else APP.schedule();
  });
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
    case 'host':
      if (d.host && d.host.id) S.hosts.set(d.host.id, d.host);
      break;
    case 'pipeline': upsertPipeline(d.pipeline); break;
    case 'attention': S.attention = d.items || []; break;
    case 'resync': syncStateSilent(); return;
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
  S.conn = 'connecting';
  const es = new EventSource('/api/events');
  S.es = es;
  es.onopen = () => {
    S.conn = 'live';
    S.retry = 0;
    if (Date.now() - S.lastLoad > 1500) loadAll();
    APP.schedule();
  };
  es.onerror = () => {
    if (es.readyState === EventSource.CLOSED) {
      S.conn = 'down';
      const wait = Math.min(MAX_BACKOFF_MS, 1000 * 2 ** S.retry++);
      S.reconnectTimer = setTimeout(connectSSE, wait);
    } else S.conn = 'connecting';
    APP.schedule();
  };
  for (const t of SSE_TYPES) {
    es.addEventListener(t, (ev) => {
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

export async function loadTmuxSessions() {
  try {
    const data = await api('GET', '/api/tmux/sessions');
    if (Array.isArray(data?.sessions)) { S.tmuxSessions = data.sessions; APP.schedule(); }
  } catch (e) { console.warn('GET /api/tmux/sessions failed', e); }
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

export async function pollHealth() {
  if (document.hidden && S.loaded) return;
  const prevFails = S.healthFails;
  const prevVer = S.version;
  const prevAuth = S.authNeeded;
  const prevUpMin = S.uptimeMs != null ? Math.floor(S.uptimeMs / 60000) : -1;
  try {
    const r = await api('GET', '/api/health');
    if (setLang(r.lang)) {
      S.slashCmds.clear();
      APP.rerender();
    }
    S.healthFails = 0;
    S.version = r.version || S.version;
    S.uptimeMs = r.uptime_ms ?? null;
    if (typeof r.auth_enabled === 'boolean') {
      S.authEnabled = r.auth_enabled;
      if (!r.auth_enabled) S.authNeeded = false;
    }
    if (S.conn === 'down') connectSSE();
    if (!S.loaded) loadAll();
  } catch { S.healthFails++; }
  const nextUpMin = S.uptimeMs != null ? Math.floor(S.uptimeMs / 60000) : -1;
  if (S.healthFails !== prevFails || S.version !== prevVer || S.authNeeded !== prevAuth || nextUpMin !== prevUpMin) {
    APP.schedulePassive();
  }
}

const IDLE_READING_POLL_MS = 15000;

export function pollActivePreview() {
  if (document.hidden || !S.loaded || !UI.autoRefreshPreview || UI.view !== 'workspace') return;
  const s = currentSession();
  if (!s || s.state === 'ended') return;
  // Pause automatic preview polling while user is scrolled up reviewing history.
  const vp = UI.wsShell?.vp || $('.term-viewport');
  if (vp && (vp.scrollHeight - vp.scrollTop - vp.clientHeight > 120)) return;
  if (s.agent !== 'shell' && UI.outputMode === 'reading') {
    const tr = S.transcripts.get(s.session_id);
    const isBusy = ['working', 'starting', 'needs_permission', 'needs_input', 'error'].includes(s.state)
      || Boolean(s.discovered_only)
      || sessionNeedsLiveTail(s, tr);
    if (!isBusy) {
      const tailOpen = UI.open.has(`tail:${s.session_id}`) || Boolean($('details.live-tail')?.open);
      const lastUpdated = tr?.updatedMs || 0;
      if (!tailOpen && lastUpdated && (Date.now() - lastUpdated) < IDLE_READING_POLL_MS) return;
      if (tailOpen && lastUpdated && (Date.now() - lastUpdated) < IDLE_READING_POLL_MS) {
        // Still allow gentle 15s checks when idle with tail expanded, without 3s transcript spam.
        return;
      }
    }
  }
  loadSessionOutput(s, { silent: true });
}
